//! Ed25519 keypair management.
//!
//! Each rotten-apple node owns a long-lived Ed25519 keypair stored at
//! `/var/lib/rotten-apple/node.key` (mode 0600). The private key signs
//! every event we issue; the corresponding public key is what peers
//! verify against. We deliberately do NOT borrow identity from the
//! transport layer (Tailscale, WireGuard) — keeping the chain
//! independent of transport means a compromised mesh-VPN doesn't
//! invalidate event-log integrity.
//!
//! On-disk format: 32 raw bytes (Ed25519 secret seed). No PEM, no
//! BER, no JSON. Single fixed-size file, atomically written via
//! create-and-rename. Permissions 0600.
//!
//! This module exposes [`KeyPair`] (load-or-generate + sign) and
//! [`PublicKey`] (verify only). The public key is `Copy` so it can
//! flow through borrows freely; the secret key is held by KeyPair
//! and never copied.

use ed25519_dalek::{
    Signature, Signer, SigningKey, Verifier, VerifyingKey,
    SECRET_KEY_LENGTH, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH,
};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Length of an Ed25519 secret-key seed on disk.
pub const SECRET_KEY_BYTES: usize = SECRET_KEY_LENGTH;
/// Length of an Ed25519 public key.
pub const PUBLIC_KEY_BYTES: usize = PUBLIC_KEY_LENGTH;
/// Length of an Ed25519 signature.
pub const SIGNATURE_BYTES: usize = SIGNATURE_LENGTH;

const DEFAULT_KEY_PATH: &str = "/var/lib/rotten-apple/node.key";

/// A node's signing keypair. Holds the secret; never serializable.
/// Debug intentionally redacts the secret — only the public-key
/// fingerprint is printable so log lines can identify the keypair
/// without leaking material.
pub struct KeyPair {
    inner: SigningKey,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("public_fingerprint", &self.public().fingerprint_short())
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl KeyPair {
    /// Generate a fresh random keypair using the OS RNG.
    pub fn generate() -> Self {
        use rand_core::OsRng;
        let mut rng = OsRng;
        KeyPair { inner: SigningKey::generate(&mut rng) }
    }

    /// Load the node's keypair from `/var/lib/rotten-apple/node.key`,
    /// generating one on first run. Persists the new key with mode 0600.
    /// Use [`KeyPair::load_or_generate_at`] in tests.
    pub fn load_or_generate() -> io::Result<Self> {
        Self::load_or_generate_at(Path::new(DEFAULT_KEY_PATH))
    }

    /// Test seam — explicit path. Same load-or-generate semantics.
    pub fn load_or_generate_at(path: &Path) -> io::Result<Self> {
        if path.exists() {
            return Self::load_from(path);
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let kp = Self::generate();
        kp.persist_to(path)?;
        Ok(kp)
    }

    fn load_from(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        if bytes.len() != SECRET_KEY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("node.key: expected {} bytes, got {}", SECRET_KEY_BYTES, bytes.len()),
            ));
        }
        let mut arr = [0u8; SECRET_KEY_BYTES];
        arr.copy_from_slice(&bytes);
        Ok(KeyPair { inner: SigningKey::from_bytes(&arr) })
    }

    /// Atomically write the secret seed (32 bytes) with mode 0600.
    /// Atomic = write-tmp-then-rename, so a crash mid-write doesn't
    /// leave a half-key.
    pub fn persist_to(&self, path: &Path) -> io::Result<()> {
        let tmp = tmp_sibling(path);
        {
            let mut f = fs::OpenOptions::new()
                .write(true).create(true).truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&self.inner.to_bytes())?;
            f.sync_all()?;
        }
        // Re-set perms in case umask altered them on creation.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, path)
    }

    /// Bare public key. Cheap to clone (it's `Copy`).
    pub fn public(&self) -> PublicKey {
        PublicKey { inner: self.inner.verifying_key() }
    }

    /// Sign an arbitrary message. Returns the 64-byte signature.
    pub fn sign(&self, msg: &[u8]) -> [u8; SIGNATURE_BYTES] {
        self.inner.sign(msg).to_bytes()
    }
}

/// A node's verifying-only public key. Cheap, copyable, serializable.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PublicKey {
    inner: VerifyingKey,
}

impl PublicKey {
    /// Load from a 32-byte raw representation.
    pub fn from_bytes(bytes: &[u8; PUBLIC_KEY_BYTES]) -> Result<Self, ed25519_dalek::SignatureError> {
        Ok(PublicKey { inner: VerifyingKey::from_bytes(bytes)? })
    }

    /// Raw 32-byte public key — for serialization or fingerprint.
    pub fn to_bytes(self) -> [u8; PUBLIC_KEY_BYTES] {
        self.inner.to_bytes()
    }

    /// Hex-encoded full public key. Cheap; useful in MCP outputs.
    pub fn hex(self) -> String {
        hex::encode(self.inner.to_bytes())
    }

    /// Short, human-friendly fingerprint: first 8 bytes hex (16 chars).
    /// For TOFU UX ("trust this peer? fingerprint: deadbeef..."), not
    /// for verification (use the full public key for that).
    pub fn fingerprint_short(self) -> String {
        let b = self.inner.to_bytes();
        hex::encode(&b[..8])
    }

    /// Verify a signature against a message.
    pub fn verify(self, msg: &[u8], sig: &[u8; SIGNATURE_BYTES]) -> Result<(), ed25519_dalek::SignatureError> {
        let s = Signature::from_bytes(sig);
        self.inner.verify(msg, &s)
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn tmp_sibling(path: &Path) -> PathBuf {
    // Same parent dir so rename is atomic across the same filesystem.
    let mut p = path.to_path_buf();
    let file = path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "tmp".into());
    p.set_file_name(format!(".{file}.tmp"));
    p
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generate_then_sign_and_verify_roundtrips() {
        let kp = KeyPair::generate();
        let msg = b"the quick brown fox jumps over the lazy dog";
        let sig = kp.sign(msg);
        kp.public().verify(msg, &sig).expect("self-verify must succeed");
    }

    #[test]
    fn signature_does_not_verify_against_wrong_pubkey() {
        let alice = KeyPair::generate();
        let mallory = KeyPair::generate();
        let msg = b"alice signed this";
        let sig = alice.sign(msg);
        assert!(mallory.public().verify(msg, &sig).is_err(),
                "mallory's pubkey must NOT accept alice's sig");
    }

    #[test]
    fn signature_does_not_verify_against_modified_message() {
        let kp = KeyPair::generate();
        let sig = kp.sign(b"original message");
        assert!(kp.public().verify(b"tampered message", &sig).is_err());
    }

    #[test]
    fn load_or_generate_persists_with_mode_0600() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("node.key");
        let _kp = KeyPair::load_or_generate_at(&path).unwrap();
        assert!(path.exists(), "first call must persist");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret key MUST be 0600 — it's a private key");
    }

    #[test]
    fn load_or_generate_returns_same_pubkey_on_reload() {
        // The whole point of persistence: reload returns a key that
        // produces the same pubkey + verifies its prior signatures.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("node.key");
        let kp1 = KeyPair::load_or_generate_at(&path).unwrap();
        let pub1 = kp1.public();
        let msg = b"test payload";
        let sig = kp1.sign(msg);
        drop(kp1);

        let kp2 = KeyPair::load_or_generate_at(&path).unwrap();
        assert_eq!(kp2.public(), pub1, "reload must yield same pubkey");
        kp2.public().verify(msg, &sig).expect("old sig still verifies");
    }

    #[test]
    fn corrupt_key_file_returns_invaliddata() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("node.key");
        // Write something obviously not 32 bytes.
        fs::write(&path, b"not-a-key").unwrap();
        let err = KeyPair::load_or_generate_at(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn fingerprint_is_short_and_stable() {
        let kp = KeyPair::generate();
        let fp = kp.public().fingerprint_short();
        assert_eq!(fp.len(), 16, "fingerprint = 8 bytes hex = 16 chars");
        assert_eq!(fp, kp.public().fingerprint_short(), "stable across calls");
    }

    #[test]
    fn pubkey_round_trips_to_and_from_bytes() {
        let kp = KeyPair::generate();
        let original = kp.public();
        let bytes = original.to_bytes();
        let reloaded = PublicKey::from_bytes(&bytes).unwrap();
        assert_eq!(original, reloaded);
    }
}
