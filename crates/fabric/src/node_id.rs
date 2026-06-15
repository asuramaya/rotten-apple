//! `NodeId` — stable per-host identifier.
//!
//! A NodeId is an opaque short string that uniquely identifies a
//! rotten-apple node. It MUST be:
//! - **stable** across reboots and software updates
//! - **unique** across the user's fleet (collisions = bad day)
//! - **printable** (used in MCP outputs, manifests, log lines)
//!
//! Source of truth (priority order):
//!
//! 1. `/etc/rotten-apple/node-id` — user-set or first-run-derived,
//!    authoritative once present. Frozen to disk so even if
//!    `/etc/machine-id` rotates (it shouldn't, but live-restored
//!    VMs occasionally do), the NodeId stays put.
//!
//! 2. Derive from `/etc/machine-id` if (1) is absent — first-run
//!    derivation. We hash the machine-id with a domain prefix so
//!    we never expose the raw machine-id (it's not secret but
//!    it's not ours to leak), then take the first 8 bytes hex.
//!
//! 3. Random fallback if (2) also fails — last resort, persisted
//!    to (1) for next boot.
//!
//! Format: 16 hex chars by default, prefixed with the host's
//! kebab-case role hint when known (`laptop-7f3a9c4d2e1b8a6f`).
//! The role hint is ergonomic only — it is NOT part of equality;
//! NodeId equality compares the hex suffix only, so renaming a
//! host (`laptop` → `desk-box`) doesn't invalidate the chain.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const NODE_ID_PATH: &str = "/etc/rotten-apple/node-id";
const MACHINE_ID_PATH: &str = "/etc/machine-id";
const HEX_SUFFIX_LEN: usize = 16; // 8 bytes hex-encoded

/// Stable per-host identifier. Wraps a string; equality compares
/// the hex suffix only so role hints can change without breaking
/// chain verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeId(String);

impl NodeId {
    /// Construct from an explicit string. Panics on empty / whitespace —
    /// callers should use [`NodeId::derive_for_this_host`] for the
    /// auto-bootstrap path.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        let trimmed = s.trim();
        assert!(!trimmed.is_empty(), "NodeId cannot be empty/whitespace");
        NodeId(trimmed.to_string())
    }

    /// Read or create the node id for this host. Reads
    /// `/etc/rotten-apple/node-id` if present; otherwise derives from
    /// `/etc/machine-id` and persists. The role hint is appended only
    /// on first-run derivation; it is informational and not load-
    /// bearing for equality.
    pub fn derive_for_this_host(role_hint: Option<&str>) -> io::Result<Self> {
        Self::derive_from_paths(
            Path::new(NODE_ID_PATH),
            Path::new(MACHINE_ID_PATH),
            role_hint,
        )
    }

    /// Test seam for [`derive_for_this_host`]: full control of file paths.
    pub fn derive_from_paths(
        node_id_file: &Path,
        machine_id_file: &Path,
        role_hint: Option<&str>,
    ) -> io::Result<Self> {
        // (1) authoritative file
        if let Ok(s) = fs::read_to_string(node_id_file) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Ok(NodeId(trimmed.to_string()));
            }
        }

        // (2) derive from /etc/machine-id
        let derived = if let Ok(machine_id) = fs::read_to_string(machine_id_file) {
            derive_hex_suffix_from_machine_id(machine_id.trim())
        } else {
            // (3) random fallback
            random_hex_suffix()
        };

        let id = match role_hint {
            Some(role) if !role.is_empty() => format!("{}-{}", kebab(role), derived),
            _ => derived,
        };

        // Persist for next boot. Best-effort — running unprivileged in
        // tests / dev shouldn't fail the whole call. The PathBuf dance
        // creates the parent dir if it's missing.
        if let Some(parent) = node_id_file.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(node_id_file, &id);

        Ok(NodeId(id))
    }

    /// Hex suffix only — the part used for equality. 16 chars.
    pub fn hex_suffix(&self) -> &str {
        let s = self.0.as_str();
        // role-hint-<HEX> or bare <HEX>; suffix is everything after the last '-'
        s.rsplit_once('-').map(|(_, h)| h).unwrap_or(s)
    }

    /// Full display form including any role hint.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl PartialEq for NodeId {
    fn eq(&self, other: &Self) -> bool {
        self.hex_suffix() == other.hex_suffix()
    }
}
impl Eq for NodeId {}

impl std::hash::Hash for NodeId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hex_suffix().hash(state);
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn derive_hex_suffix_from_machine_id(machine_id: &str) -> String {
    // Domain-separate so /etc/machine-id can't be recovered from a
    // leaked NodeId (defense in depth — machine-id isn't secret, but
    // it's not ours to leak either).
    let mut h = Sha256::new();
    h.update(b"rotten-apple-node-id-v1\0");
    h.update(machine_id.as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..HEX_SUFFIX_LEN / 2])
}

fn random_hex_suffix() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; HEX_SUFFIX_LEN / 2];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn kebab(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[allow(dead_code)]
fn nodeid_file_default() -> PathBuf { PathBuf::from(NODE_ID_PATH) }

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn derive_uses_explicit_file_when_present() {
        // (1) wins over (2): if /etc/rotten-apple/node-id exists, return
        // its contents verbatim — even if machine-id is also there.
        let tmp = tempdir().unwrap();
        let nid = tmp.path().join("node-id");
        let mid = tmp.path().join("machine-id");
        fs::write(&nid, "explicit-deadbeefcafebabe\n").unwrap();
        fs::write(&mid, "11111111111111111111111111111111\n").unwrap();
        let id = NodeId::derive_from_paths(&nid, &mid, Some("laptop")).unwrap();
        assert_eq!(id.as_str(), "explicit-deadbeefcafebabe");
    }

    #[test]
    fn derive_falls_back_to_machine_id_hash() {
        // (2) when (1) is absent: hash the machine-id with our domain
        // prefix and persist the result. Same machine-id always
        // produces same NodeId — pin the determinism.
        let tmp = tempdir().unwrap();
        let nid = tmp.path().join("node-id");
        let mid = tmp.path().join("machine-id");
        fs::write(&mid, "11111111111111111111111111111111\n").unwrap();
        let a = NodeId::derive_from_paths(&nid, &mid, Some("laptop")).unwrap();
        // After first call, (1) should now exist with the persisted value.
        assert!(nid.exists(), "first-run must persist node-id to file");
        // Second call returns the same id (now via path 1).
        let b = NodeId::derive_from_paths(&nid, &mid, Some("ignored")).unwrap();
        assert_eq!(a, b, "stable across calls");
        // Role hint affects display, NOT equality.
        assert!(a.as_str().starts_with("laptop-"));
    }

    #[test]
    fn equality_ignores_role_hint() {
        // The whole point of separating role from hex is that you can
        // rename the host without breaking the chain. Two NodeIds with
        // the same hex suffix MUST compare equal regardless of prefix.
        let a = NodeId::new("laptop-deadbeefcafebabe");
        let b = NodeId::new("desk-box-deadbeefcafebabe");
        assert_eq!(a, b);
        assert_eq!(a.hex_suffix(), b.hex_suffix());
    }

    #[test]
    fn equality_distinguishes_different_hex() {
        let a = NodeId::new("laptop-deadbeefcafebabe");
        let b = NodeId::new("laptop-1111111111111111");
        assert_ne!(a, b);
    }

    #[test]
    fn machine_id_hash_is_deterministic() {
        // Belt-and-braces — direct test of the helper used in path (2).
        let h1 = derive_hex_suffix_from_machine_id("abc123");
        let h2 = derive_hex_suffix_from_machine_id("abc123");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), HEX_SUFFIX_LEN);
        let h3 = derive_hex_suffix_from_machine_id("def456");
        assert_ne!(h1, h3);
    }

    #[test]
    fn random_fallback_when_machine_id_absent() {
        // (3): no node-id file, no machine-id file → still produce a
        // valid NodeId via random suffix. Persist for next boot.
        let tmp = tempdir().unwrap();
        let nid = tmp.path().join("node-id");
        let mid = tmp.path().join("nope");
        assert!(!mid.exists());
        let id = NodeId::derive_from_paths(&nid, &mid, None).unwrap();
        assert_eq!(id.hex_suffix().len(), HEX_SUFFIX_LEN);
        assert!(nid.exists(), "random fallback must persist");
    }

    #[test]
    fn kebab_normalizes_role_hint() {
        assert_eq!(kebab("Desk Box"),     "desk-box");
        assert_eq!(kebab("MyLaptop_42"),  "mylaptop-42");
        assert_eq!(kebab("---weird--"),   "weird");
    }
}
