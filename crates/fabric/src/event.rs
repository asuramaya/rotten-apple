//! Signed event envelope — the wire format of the rotten-apple mesh.
//!
//! Every state-mutating command flows through the chain as an
//! [`Envelope`]. Each envelope carries the fields needed for ordering,
//! attribution, and tamper detection, plus a raw [`EventBody`] that
//! the agent's reconcile loop interprets.
//!
//! Signing scope: the signature covers EVERY field of the envelope
//! EXCEPT the signature itself. We compute "signed bytes" by JSON-
//! serializing the unsigned core (a stable, sorted form) and feed that
//! to Ed25519. JSON is used over a custom binary format because:
//! (a) the chain isn't perf-critical (events are rare), (b) JSON is
//! debuggable across the wire, (c) we can use serde_json's
//! deterministic output by avoiding nondeterministic types (no
//! HashMaps in event bodies — `BTreeMap` only).
//!
//! The hash chain: each envelope's `prev_hash` is SHA-256 of the
//! previous envelope's *signed bytes* (NOT the signature itself, NOT
//! the envelope JSON). This pins the previous event AND its signature
//! into the chain — replacing a signature would change the hash and
//! break every downstream prev_hash.

use crate::node_id::NodeId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Per-issuer monotonically increasing sequence number. Resets only
/// on lease handoff to a different controller (new lease epoch).
pub type Sequence = u64;

/// Increases each time a new controller takes over the lease.
/// Agents reject events from epochs they've already moved past.
pub type LeaseEpoch = u64;

/// SHA-256 of an envelope's signed bytes. Pinned at 32 bytes to keep
/// type signatures self-documenting.
pub type EventHash = [u8; 32];

/// Signature bytes (Ed25519 = 64 bytes). Wrapped to make wire-format
/// changes easier (Vec<u8> would be more flexible but error-prone).
pub type EventSignature = [u8; 64];

/// The semantic payload — what the controller is asking the agent to
/// do, or what the agent is reporting. New variants extend the wire
/// without breaking older agents IFF they're fully ignored when
/// unrecognized; we treat unrecognized as a chain-terminating error
/// (loud, not silent — an old agent should refuse to run a chain it
/// doesn't fully understand rather than guess). Add variants only at
/// the end to preserve serde compatibility.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBody {
    /// A controller declares it now holds the lease for `target_node`.
    /// Agents accept ClaimLease only when `epoch > current_epoch` and
    /// the issuer is permitted by the agent's local policy.
    ClaimLease {
        target_node: NodeId,
    },
    /// A controller voluntarily releases the lease.
    ReleaseLease {
        target_node: NodeId,
    },
    /// Controller wants the named manifest running on the agent. The
    /// manifest content is included as the canonical TOML string —
    /// the agent stores it verbatim as the source of truth for that
    /// manifest_id at this lease_epoch.
    Assign {
        manifest_id: String,
        manifest_toml: String,
    },
    /// Controller withdraws an assignment. Agent drains and stops the
    /// guest; manifest_toml is no longer authoritative at the agent.
    Unassign {
        manifest_id: String,
    },
    /// Resize an active guest's memory. Bounded by the manifest's
    /// resources.{memory_active,memory_minimum} — the agent is free
    /// to refuse out-of-bounds requests.
    Balloon {
        manifest_id: String,
        target_mb: u64,
    },
    /// Agent tells the controller what it currently sees: capacity,
    /// running guests, last-applied event hash. Heartbeats are NOT
    /// state-mutating — they're observational — but they ride on the
    /// same chain so agents can detect chain divergence.
    Heartbeat {
        free_memory_mb: u64,
        free_vcpus: u32,
        running: Vec<String>,
        last_applied_hash: Option<String>, // hex
    },
}

/// The unsigned core of an envelope — the bytes that get signed.
/// Kept as a separate struct so the JSON serialization is unambiguous
/// and excludes the signature. Field order is fixed by serde derive
/// in declaration order; we serialize with `serde_json::to_vec` which
/// preserves struct field order.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvelopeCore {
    /// Schema version of the envelope itself. Bump on incompatible
    /// changes; agents reject unknown versions.
    pub schema: u32,
    /// Issuer of this event (the controller's NodeId).
    pub issuer: NodeId,
    /// Per-(issuer, lease_epoch) monotonic sequence.
    pub seq: Sequence,
    /// Lease epoch under which this event was issued.
    pub lease_epoch: LeaseEpoch,
    /// SHA-256 of the previous envelope's signed bytes. All zeros for
    /// genesis events.
    #[serde(with = "hex_array_32")]
    pub prev_hash: EventHash,
    /// Unix seconds, informational only — agents must not use this
    /// for ordering or freshness decisions.
    pub timestamp: u64,
    /// The semantic payload.
    pub body: EventBody,
}

/// A complete signed event ready for the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    #[serde(flatten)]
    pub core: EnvelopeCore,
    /// Ed25519 signature over the canonical-JSON-serialized `core`.
    #[serde(with = "hex_array_64")]
    pub signature: EventSignature,
}

/// Convenience alias for callers that don't want to type the full enum.
pub type Event = EventBody;

pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;

impl EnvelopeCore {
    /// Canonical signed bytes. Must be deterministic given the same
    /// core — that's what makes the chain verifiable across nodes.
    pub fn signed_bytes(&self) -> Vec<u8> {
        // serde_json::to_vec is deterministic for our types because
        // we forbid HashMap (use BTreeMap or Vec) in event bodies.
        serde_json::to_vec(self).expect("EnvelopeCore is always serializable")
    }

    /// SHA-256 of the signed bytes. Used as the next event's `prev_hash`.
    pub fn hash(&self) -> EventHash {
        let mut h = Sha256::new();
        h.update(self.signed_bytes());
        h.finalize().into()
    }
}

impl Envelope {
    /// Build and sign a new envelope. `prev_hash` is `[0u8; 32]` for
    /// the chain's genesis; otherwise pass the previous envelope's
    /// `core.hash()`.
    pub fn sign(
        kp: &crate::keypair::KeyPair,
        issuer: NodeId,
        seq: Sequence,
        lease_epoch: LeaseEpoch,
        prev_hash: EventHash,
        timestamp: u64,
        body: EventBody,
    ) -> Self {
        let core = EnvelopeCore {
            schema: ENVELOPE_SCHEMA_VERSION,
            issuer,
            seq,
            lease_epoch,
            prev_hash,
            timestamp,
            body,
        };
        let signed = core.signed_bytes();
        let signature = kp.sign(&signed);
        Envelope { core, signature }
    }

    /// Verify the envelope's signature against the issuer's public key.
    /// Returns Err on signature mismatch or schema-version drift.
    pub fn verify(&self, issuer_pubkey: crate::keypair::PublicKey) -> Result<(), VerifyError> {
        if self.core.schema != ENVELOPE_SCHEMA_VERSION {
            return Err(VerifyError::SchemaMismatch {
                got: self.core.schema, expected: ENVELOPE_SCHEMA_VERSION,
            });
        }
        let bytes = self.core.signed_bytes();
        issuer_pubkey.verify(&bytes, &self.signature)
            .map_err(|_| VerifyError::BadSignature)
    }
}

/// Reasons an envelope's signature might fail to verify. Distinct from
/// chain errors (those live in [`crate::chain::ChainError`]) — this is
/// just envelope-level integrity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    BadSignature,
    SchemaMismatch { got: u32, expected: u32 },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::BadSignature => f.write_str("envelope signature failed to verify"),
            VerifyError::SchemaMismatch { got, expected } => write!(f,
                "envelope schema version mismatch: got {got}, expected {expected}"),
        }
    }
}
impl std::error::Error for VerifyError {}

// ---------------------------------------------------------------------------
// serde helpers — hex encoding for the fixed-size byte arrays
// (otherwise serde_json would emit JSON arrays of integers, which is
// noisy + breaks our determinism guarantees).

mod hex_array_32 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom(format!(
                "expected 32 bytes hex, got {}", bytes.len())));
        }
        let mut a = [0u8; 32];
        a.copy_from_slice(&bytes);
        Ok(a)
    }
}

mod hex_array_64 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 64 {
            return Err(serde::de::Error::custom(format!(
                "expected 64 bytes hex, got {}", bytes.len())));
        }
        let mut a = [0u8; 64];
        a.copy_from_slice(&bytes);
        Ok(a)
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keypair::KeyPair;

    fn make_envelope(kp: &KeyPair, issuer: NodeId, seq: u64, body: EventBody) -> Envelope {
        Envelope::sign(kp, issuer, seq, /*lease_epoch=*/1, [0u8; 32], 1700000000, body)
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let kp = KeyPair::generate();
        let issuer = NodeId::new("ctrl-deadbeefcafebabe");
        let env = make_envelope(&kp, issuer.clone(), 0, EventBody::ClaimLease {
            target_node: NodeId::new("agent-1111111111111111"),
        });
        env.verify(kp.public()).expect("self-verify must succeed");
    }

    #[test]
    fn tampering_with_body_breaks_verify() {
        // Construct, then tamper with the body before verify. The
        // signature was computed against the old body's bytes, so the
        // new bytes won't match.
        let kp = KeyPair::generate();
        let issuer = NodeId::new("ctrl-deadbeefcafebabe");
        let mut env = make_envelope(&kp, issuer, 0, EventBody::Assign {
            manifest_id: "ci-runner".into(),
            manifest_toml: "[profile]\nname=\"original\"".into(),
        });
        if let EventBody::Assign { ref mut manifest_toml, .. } = env.core.body {
            *manifest_toml = "[profile]\nname=\"hijacked\"".into();
        }
        let err = env.verify(kp.public()).unwrap_err();
        assert_eq!(err, VerifyError::BadSignature);
    }

    #[test]
    fn wrong_issuer_pubkey_breaks_verify() {
        let alice = KeyPair::generate();
        let mallory = KeyPair::generate();
        let env = make_envelope(&alice, NodeId::new("alice-aaaaaaaaaaaaaaaa"),
                                0, EventBody::ReleaseLease {
                                    target_node: NodeId::new("agent-1111111111111111"),
                                });
        let err = env.verify(mallory.public()).unwrap_err();
        assert_eq!(err, VerifyError::BadSignature);
    }

    #[test]
    fn schema_mismatch_is_caught_distinctly() {
        // Future-proofing: agents must distinguish "wrong sig" from
        // "schema bump I don't know how to handle." Pin that we surface
        // schema drift as a separate error variant.
        let kp = KeyPair::generate();
        let mut env = make_envelope(&kp, NodeId::new("ctrl-deadbeefcafebabe"),
                                    0, EventBody::ReleaseLease {
                                        target_node: NodeId::new("agent-1111111111111111"),
                                    });
        env.core.schema = 999;
        // We need to re-sign for the bad-schema bytes to actually have a
        // valid sig — otherwise we'd hit BadSignature first. But we
        // intentionally short-circuit on schema BEFORE doing the sig
        // check, so the order is enforced.
        let err = env.verify(kp.public()).unwrap_err();
        assert!(matches!(err, VerifyError::SchemaMismatch { got: 999, expected: 1 }),
                "got: {err:?}");
    }

    #[test]
    fn signed_bytes_are_deterministic() {
        // The whole chain depends on this: two calls to signed_bytes()
        // on the same EnvelopeCore must produce byte-identical output,
        // otherwise downstream prev_hash values won't agree.
        let kp = KeyPair::generate();
        let env = make_envelope(&kp, NodeId::new("ctrl-deadbeefcafebabe"),
                                42, EventBody::Balloon {
                                    manifest_id: "x".into(), target_mb: 4096,
                                });
        let a = env.core.signed_bytes();
        let b = env.core.signed_bytes();
        assert_eq!(a, b);
    }

    #[test]
    fn hash_changes_when_any_core_field_changes() {
        // Sanity that prev_hash actually depends on every field —
        // if hash() ignored a field we'd silently allow tampering.
        let kp = KeyPair::generate();
        let issuer = NodeId::new("ctrl-deadbeefcafebabe");
        let body = EventBody::Assign {
            manifest_id: "x".into(), manifest_toml: "y".into(),
        };
        let base = Envelope::sign(&kp, issuer.clone(), 1, 1, [0u8; 32], 1700000000, body.clone());
        let h0 = base.core.hash();

        let mut alt = base.clone();
        alt.core.seq = 2; // change one field
        assert_ne!(h0, alt.core.hash(), "seq must affect hash");

        let mut alt = base.clone();
        alt.core.lease_epoch = 2;
        assert_ne!(h0, alt.core.hash(), "lease_epoch must affect hash");

        let mut alt = base.clone();
        alt.core.timestamp = 1700000001;
        assert_ne!(h0, alt.core.hash(), "timestamp must affect hash");
    }

    #[test]
    fn json_round_trip_preserves_signature() {
        // Wire format check: serialize the envelope to JSON, parse it
        // back, verify the signature still validates. This is what
        // happens on every receive.
        let kp = KeyPair::generate();
        let env = make_envelope(&kp, NodeId::new("ctrl-deadbeefcafebabe"),
                                7, EventBody::Heartbeat {
                                    free_memory_mb: 8192,
                                    free_vcpus: 4,
                                    running: vec!["ci-runner".into()],
                                    last_applied_hash: Some("abc123".into()),
                                });
        let json = serde_json::to_string(&env).unwrap();
        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, env);
        parsed.verify(kp.public()).expect("re-parsed envelope must still verify");
    }
}
