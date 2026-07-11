//! Control-plane caller authorization by mesh-peer identity.
//!
//! DECISION (2026-07-08, operator): orchestratord authorizes privileged
//! JSON-RPC calls by verifying a signed request against the fabric
//! Ed25519 `TrustStore` — the SAME identity layer the mesh rides on, not
//! a dom0-only shortcut. "Is this caller allowed to kill a domain?"
//! reduces to "is this request signed by a node whose pubkey we trust?".
//!
//! ## Threat closed
//! Before this, any process reaching the socket (and, with vsock on, any
//! GUEST) could call `domain.kill`/`domain.create`/… unauthenticated —
//! a guest→host control-plane escalation. Read-only methods stay open;
//! every STATE-MUTATING method now demands a valid signed request.
//!
//! ## Protocol
//! Per connection the daemon issues a random 32-byte `nonce` in its hello
//! reply. For each privileged call the client sends an [`AuthRequest`]
//! carrying its `node_id`, `pubkey`, a strictly-increasing `seq`, and an
//! Ed25519 `signature` over [`signing_payload`] =
//! `DOMAIN || nonce || seq || method || params`. The daemon:
//!   1. rejects unless the method is read-only OR auth is present,
//!   2. looks the node up in the `TrustStore` (unknown ⇒ deny),
//!   3. binds identity to key — the trusted pubkey must equal the
//!      presented pubkey (you can't wear a trusted node's name with your
//!      own key),
//!   4. requires `seq` strictly greater than the last accepted seq on
//!      this connection (replay guard — a captured request can't be
//!      re-sent, and the per-connection nonce stops cross-connection
//!      replay),
//!   5. verifies the signature over the exact payload.
//!
//! The nonce is domain-separated so a control-plane signature can never
//! be confused with a fabric event signature.
//!
//! ## What is and isn't wired here
//! This module is the PURE, fully-tested core (payload construction,
//! signing, verification, replay/identity rejection). Issuing the nonce
//! in the hello handshake and reading the `auth` field off the wire is
//! done in `lib.rs::handle_connection` (see the integration TODO there);
//! that path needs a live socket and can't be unit-tested off-Xen. The
//! node keypair will be TPM2-sealed (task #16) rather than a plain 0600
//! file — that's a `KeyPair` construction detail below this layer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rotten_apple_fabric::chain::TrustStore;
use rotten_apple_fabric::{KeyPair, NodeId, PublicKey, PUBLIC_KEY_BYTES, SIGNATURE_BYTES};

/// Length of the per-connection challenge nonce.
pub const NONCE_BYTES: usize = 32;

/// Domain-separation tag. A signature over this payload can NEVER be
/// replayed as a fabric event signature (different tag) or vice versa.
/// Bump `v1` if the payload layout below changes.
const RPC_SIG_DOMAIN: &[u8] = b"rotten-apple:orchestratord:rpc:v1";

/// Does this JSON-RPC method mutate host/guest state? Mutating methods
/// require a signed request from a trusted node; read-only methods
/// (`ping`, `host.*`, `domain.list`/`get`, `engine.status`,
/// `events.tail`) are served without auth.
///
/// Fail-closed by construction: this is an explicit ALLOW-list of the
/// safe, read-only methods; anything not named is treated as privileged.
/// A newly-added mutating method is therefore protected by default even
/// if someone forgets to classify it.
pub fn method_is_privileged(method: &str) -> bool {
    !matches!(
        method,
        "ping"
            | "host.info"
            | "host.resources"
            | "domain.list"
            | "domain.get"
            | "engine.status"
            | "events.tail"
    )
}

/// The authorization material a client attaches to a privileged call.
/// Hex-encoded so it rides inside JSON cleanly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthRequest {
    /// Claimed node identity — must exist in the daemon's `TrustStore`.
    pub node_id: String,
    /// Ed25519 public key, hex (32 bytes) — must equal the trusted key
    /// registered for `node_id`.
    pub pubkey: String,
    /// Strictly-increasing per-connection counter (replay guard).
    pub seq: u64,
    /// Ed25519 signature over [`signing_payload`], hex (64 bytes).
    pub signature: String,
}

/// Why a privileged call was refused. Deliberately coarse on the wire —
/// callers get "permission denied", not which check failed, so probing
/// learns nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// Privileged method with no auth material attached.
    MissingAuth,
    /// `node_id` empty or otherwise unusable.
    MalformedNode,
    /// `pubkey` not valid hex / not a valid Ed25519 key.
    MalformedPubkey,
    /// `signature` not valid hex / wrong length.
    MalformedSignature,
    /// `node_id` is not in the trust store.
    UnknownNode,
    /// Presented pubkey ≠ the trusted key for this node.
    KeyMismatch,
    /// `seq` did not strictly exceed the last accepted seq (replay).
    StaleSeq,
    /// Signature did not verify over the expected payload.
    BadSignature,
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DenyReason::MissingAuth => "missing auth on a privileged method",
            DenyReason::MalformedNode => "malformed node id",
            DenyReason::MalformedPubkey => "malformed public key",
            DenyReason::MalformedSignature => "malformed signature",
            DenyReason::UnknownNode => "unknown node",
            DenyReason::KeyMismatch => "key does not match trusted node",
            DenyReason::StaleSeq => "stale sequence (replay)",
            DenyReason::BadSignature => "bad signature",
        };
        f.write_str(s)
    }
}

/// Canonical bytes a client signs and the daemon verifies. Length-
/// prefixed so no field boundary is ambiguous. `params` is serialized
/// with `serde_json` whose default `Map` is key-sorted, giving both
/// sides the same bytes for the same logical params.
pub fn signing_payload(nonce: &[u8; NONCE_BYTES], seq: u64, method: &str, params: &Value) -> Vec<u8> {
    let params_bytes = serde_json::to_vec(params).unwrap_or_default();
    let mut buf = Vec::with_capacity(
        RPC_SIG_DOMAIN.len() + 1 + NONCE_BYTES + 8 + 4 + method.len() + params_bytes.len(),
    );
    buf.extend_from_slice(RPC_SIG_DOMAIN);
    buf.push(0x00);
    buf.extend_from_slice(nonce);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&(method.len() as u32).to_le_bytes());
    buf.extend_from_slice(method.as_bytes());
    buf.extend_from_slice(&params_bytes);
    buf
}

/// Client side: produce the [`AuthRequest`] for a call. The `seq` must be
/// strictly greater than any previously sent on this connection.
pub fn sign_request(
    kp: &KeyPair,
    node_id: &NodeId,
    nonce: &[u8; NONCE_BYTES],
    seq: u64,
    method: &str,
    params: &Value,
) -> AuthRequest {
    let payload = signing_payload(nonce, seq, method, params);
    AuthRequest {
        node_id: node_id.as_str().to_string(),
        pubkey: hex::encode(kp.public().to_bytes()),
        seq,
        signature: hex::encode(kp.sign(&payload)),
    }
}

/// Per-connection authorization state: the issued challenge nonce and the
/// high-water sequence number. One of these lives per accepted socket in
/// `handle_connection`.
#[derive(Debug)]
pub struct AuthSession {
    nonce: [u8; NONCE_BYTES],
    last_seq: Option<u64>,
}

impl AuthSession {
    /// Construct with an explicit nonce (tests, and the wire layer once
    /// it has drawn one from [`fresh_nonce`]).
    pub fn new(nonce: [u8; NONCE_BYTES]) -> Self {
        Self { nonce, last_seq: None }
    }

    /// The challenge to hand the client in the hello reply.
    pub fn nonce_hex(&self) -> String {
        hex::encode(self.nonce)
    }

    /// Authorize one call. Returns:
    ///   - `Ok(None)` — read-only method, no identity required,
    ///   - `Ok(Some(node))` — privileged call, authenticated as `node`,
    ///   - `Err(reason)` — refused.
    ///
    /// On success the connection's replay high-water mark advances, so a
    /// captured privileged request cannot be replayed on the same
    /// connection.
    pub fn authorize(
        &mut self,
        method: &str,
        params: &Value,
        auth: Option<&AuthRequest>,
        trust: &TrustStore,
    ) -> Result<Option<NodeId>, DenyReason> {
        if !method_is_privileged(method) {
            return Ok(None);
        }
        let auth = auth.ok_or(DenyReason::MissingAuth)?;

        // NodeId::new panics on empty input — guard before constructing.
        if auth.node_id.trim().is_empty() {
            return Err(DenyReason::MalformedNode);
        }
        let node = NodeId::new(auth.node_id.clone());

        let pk_bytes = decode_hex_fixed::<PUBLIC_KEY_BYTES>(&auth.pubkey)
            .ok_or(DenyReason::MalformedPubkey)?;
        let presented =
            PublicKey::from_bytes(&pk_bytes).map_err(|_| DenyReason::MalformedPubkey)?;
        let sig =
            decode_hex_fixed::<SIGNATURE_BYTES>(&auth.signature).ok_or(DenyReason::MalformedSignature)?;

        // Identity is bound to key: the node must be trusted AND present
        // the exact pubkey we trust for it.
        let trusted = trust.get(&node).ok_or(DenyReason::UnknownNode)?;
        if trusted != presented {
            return Err(DenyReason::KeyMismatch);
        }

        // Strict-monotonic replay guard, scoped to this connection's nonce.
        if let Some(last) = self.last_seq
            && auth.seq <= last
        {
            return Err(DenyReason::StaleSeq);
        }

        let payload = signing_payload(&self.nonce, auth.seq, method, params);
        presented
            .verify(&payload, &sig)
            .map_err(|_| DenyReason::BadSignature)?;

        self.last_seq = Some(auth.seq);
        Ok(Some(node))
    }
}

/// Draw a fresh challenge nonce from the OS CSPRNG (`getrandom(2)`). Used
/// by the wire layer per accepted connection; the pure core takes a nonce
/// as input so it never touches the OS in tests.
pub fn fresh_nonce() -> [u8; NONCE_BYTES] {
    let mut buf = [0u8; NONCE_BYTES];
    let mut filled = 0usize;
    while filled < NONCE_BYTES {
        let rc = unsafe {
            libc::getrandom(
                buf[filled..].as_mut_ptr() as *mut libc::c_void,
                NONCE_BYTES - filled,
                0,
            )
        };
        if rc <= 0 {
            // getrandom should not fail for a 32-byte draw; if the kernel
            // is that broken, fall back to a distinct-but-unpredictable
            // mix rather than shipping zeros. This branch is not expected
            // in practice.
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            buf[filled..].copy_from_slice(&derive_fallback(t, filled)[..NONCE_BYTES - filled]);
            break;
        }
        filled += rc as usize;
    }
    buf
}

fn derive_fallback(seed: u128, salt: usize) -> [u8; NONCE_BYTES] {
    // Only reached if getrandom is unavailable. Not cryptographic; exists
    // solely so a nonce is never all-zeros.
    let mut out = [0u8; NONCE_BYTES];
    let mut x = seed ^ ((salt as u128) << 64 | 0x9E3779B97F4A7C15);
    for b in out.iter_mut() {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (x >> 88) as u8;
    }
    out
}

/// Decode a hex string into exactly `N` bytes, or `None` on bad hex /
/// wrong length.
fn decode_hex_fixed<const N: usize>(s: &str) -> Option<[u8; N]> {
    let v = hex::decode(s).ok()?;
    if v.len() != N {
        return None;
    }
    let mut arr = [0u8; N];
    arr.copy_from_slice(&v);
    Some(arr)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn trusted_pair() -> (KeyPair, NodeId, TrustStore) {
        let kp = KeyPair::generate();
        let node = NodeId::new("laptop-deadbeefcafebabe");
        let mut trust = TrustStore::new();
        trust.insert(node.clone(), kp.public());
        (kp, node, trust)
    }

    fn nonce() -> [u8; NONCE_BYTES] {
        // Deterministic nonce for reproducible tests.
        let mut n = [0u8; NONCE_BYTES];
        for (i, b) in n.iter_mut().enumerate() {
            *b = i as u8;
        }
        n
    }

    #[test]
    fn read_only_methods_need_no_auth() {
        let (_, _, trust) = trusted_pair();
        let mut s = AuthSession::new(nonce());
        for m in ["ping", "host.info", "domain.list", "domain.get", "engine.status", "events.tail"] {
            assert_eq!(s.authorize(m, &json!({}), None, &trust), Ok(None), "{m} should be open");
        }
    }

    #[test]
    fn privileged_methods_are_the_mutating_ones() {
        for m in ["domain.create", "domain.start", "domain.shutdown", "domain.kill", "domain.balloon", "engine.set_policy"] {
            assert!(method_is_privileged(m), "{m} must be privileged");
        }
        // Fail-closed: an unknown/new method is privileged by default.
        assert!(method_is_privileged("domain.detonate"));
    }

    #[test]
    fn valid_signed_privileged_call_is_authorized() {
        let (kp, node, trust) = trusted_pair();
        let mut s = AuthSession::new(nonce());
        let params = json!({"domid": 3});
        let auth = sign_request(&kp, &node, &nonce(), 1, "domain.kill", &params);
        assert_eq!(s.authorize("domain.kill", &params, Some(&auth), &trust), Ok(Some(node)));
    }

    #[test]
    fn missing_auth_on_privileged_is_denied() {
        let (_, _, trust) = trusted_pair();
        let mut s = AuthSession::new(nonce());
        assert_eq!(
            s.authorize("domain.kill", &json!({"domid": 1}), None, &trust),
            Err(DenyReason::MissingAuth)
        );
    }

    #[test]
    fn unknown_node_is_denied() {
        let (_, _, trust) = trusted_pair(); // trust has the laptop only
        let stranger = KeyPair::generate();
        let stranger_id = NodeId::new("rogue-1111111111111111");
        let mut s = AuthSession::new(nonce());
        let params = json!({"domid": 1});
        let auth = sign_request(&stranger, &stranger_id, &nonce(), 1, "domain.kill", &params);
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth), &trust),
            Err(DenyReason::UnknownNode)
        );
    }

    #[test]
    fn trusted_name_with_wrong_key_is_denied() {
        // Attacker knows a trusted node_id but signs with their own key
        // and presents their own pubkey — must be KeyMismatch, not accepted.
        let (_, node, trust) = trusted_pair();
        let attacker = KeyPair::generate();
        let mut s = AuthSession::new(nonce());
        let params = json!({"domid": 1});
        let auth = sign_request(&attacker, &node, &nonce(), 1, "domain.kill", &params);
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth), &trust),
            Err(DenyReason::KeyMismatch)
        );
    }

    #[test]
    fn tampered_params_fail_signature() {
        let (kp, node, trust) = trusted_pair();
        let mut s = AuthSession::new(nonce());
        let signed = json!({"domid": 3});
        let auth = sign_request(&kp, &node, &nonce(), 1, "domain.kill", &signed);
        // Server sees DIFFERENT params than were signed.
        let tampered = json!({"domid": 999});
        assert_eq!(
            s.authorize("domain.kill", &tampered, Some(&auth), &trust),
            Err(DenyReason::BadSignature)
        );
    }

    #[test]
    fn wrong_nonce_fails_signature() {
        // A signature made for a different connection's nonce must not
        // verify here — this is the cross-connection replay defense.
        let (kp, node, trust) = trusted_pair();
        let other_nonce = [0xAAu8; NONCE_BYTES];
        let params = json!({"domid": 1});
        let auth = sign_request(&kp, &node, &other_nonce, 1, "domain.kill", &params);
        let mut s = AuthSession::new(nonce()); // different nonce
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth), &trust),
            Err(DenyReason::BadSignature)
        );
    }

    #[test]
    fn replayed_seq_is_denied() {
        let (kp, node, trust) = trusted_pair();
        let mut s = AuthSession::new(nonce());
        let params = json!({"domid": 1});
        let auth1 = sign_request(&kp, &node, &nonce(), 5, "domain.kill", &params);
        assert!(s.authorize("domain.kill", &params, Some(&auth1), &trust).is_ok());
        // Same seq again — replay.
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth1), &trust),
            Err(DenyReason::StaleSeq)
        );
        // A lower seq is also rejected.
        let auth_low = sign_request(&kp, &node, &nonce(), 4, "domain.kill", &params);
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth_low), &trust),
            Err(DenyReason::StaleSeq)
        );
        // A higher seq proceeds.
        let auth2 = sign_request(&kp, &node, &nonce(), 6, "domain.kill", &params);
        assert!(s.authorize("domain.kill", &params, Some(&auth2), &trust).is_ok());
    }

    #[test]
    fn malformed_pubkey_and_signature_are_denied() {
        let (kp, node, trust) = trusted_pair();
        let mut s = AuthSession::new(nonce());
        let params = json!({"domid": 1});
        let mut auth = sign_request(&kp, &node, &nonce(), 1, "domain.kill", &params);
        let good_sig = auth.signature.clone();
        auth.pubkey = "zzzz".into();
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth), &trust),
            Err(DenyReason::MalformedPubkey)
        );
        // Fix pubkey, break signature.
        auth.pubkey = hex::encode(kp.public().to_bytes());
        auth.signature = "deadbeef".into(); // valid hex, wrong length
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth), &trust),
            Err(DenyReason::MalformedSignature)
        );
        auth.signature = good_sig; // restore → authorized
        assert!(s.authorize("domain.kill", &params, Some(&auth), &trust).is_ok());
    }

    #[test]
    fn empty_node_id_is_rejected_not_panicked() {
        let (kp, node, trust) = trusted_pair();
        let mut s = AuthSession::new(nonce());
        let params = json!({"domid": 1});
        let mut auth = sign_request(&kp, &node, &nonce(), 1, "domain.kill", &params);
        auth.node_id = "   ".into(); // whitespace — NodeId::new would panic
        assert_eq!(
            s.authorize("domain.kill", &params, Some(&auth), &trust),
            Err(DenyReason::MalformedNode)
        );
    }

    #[test]
    fn fresh_nonce_is_not_all_zeros_and_varies() {
        let a = fresh_nonce();
        let b = fresh_nonce();
        assert_ne!(a, [0u8; NONCE_BYTES], "nonce must not be zeros");
        assert_ne!(a, b, "two draws must differ");
    }

    #[test]
    fn auth_request_round_trips_json() {
        let (kp, node, _) = trusted_pair();
        let auth = sign_request(&kp, &node, &nonce(), 1, "domain.kill", &json!({"domid": 2}));
        let wire = serde_json::to_string(&auth).unwrap();
        let back: AuthRequest = serde_json::from_str(&wire).unwrap();
        assert_eq!(auth, back);
    }
}
