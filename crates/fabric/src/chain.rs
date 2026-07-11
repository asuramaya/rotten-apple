//! Append-only event log with chain verification.
//!
//! The mesh's authoritative record. Every state-mutating event flows
//! through here and is checked for:
//!   1. **signature** — issuer's pubkey verifies the envelope
//!   2. **schema** — version matches what we know
//!   3. **chain link** — `prev_hash` equals the head's hash (or all
//!      zeros for genesis)
//!   4. **monotonic epoch** — `lease_epoch >= head.lease_epoch`
//!   5. **per-issuer-per-epoch monotonic sequence** — `seq` is 0 on
//!      epoch reset, otherwise prior + 1 for the same issuer
//!   6. **issuer continuity within an epoch** — within a lease epoch
//!      only the lease-holder's events are accepted; switching
//!      issuer mid-epoch would mean two simultaneous controllers
//!      (split-brain) and is rejected
//!
//! This module is the single point that enforces those invariants;
//! storage (SQLite, file, in-memory) lives elsewhere and just feeds
//! envelopes through `append()`.

use crate::event::{Envelope, EventHash, LeaseEpoch, Sequence, VerifyError};
use crate::keypair::PublicKey;
use crate::node_id::NodeId;
use std::collections::HashMap;

/// Maps issuer NodeId → trusted Ed25519 pubkey. The chain consults
/// this on every `append` to verify signatures. Callers manage the
/// mapping (add peers via TOFU, config, or CA — see config.rs).
#[derive(Default, Debug, Clone)]
pub struct TrustStore {
    keys: HashMap<NodeId, PublicKey>,
}

impl TrustStore {
    pub fn new() -> Self { Self::default() }

    pub fn insert(&mut self, node: NodeId, key: PublicKey) {
        self.keys.insert(node, key);
    }

    pub fn get(&self, node: &NodeId) -> Option<PublicKey> {
        self.keys.get(node).copied()
    }

    pub fn len(&self) -> usize { self.keys.len() }
    pub fn is_empty(&self) -> bool { self.keys.is_empty() }
}

/// Append-only signed event log. In-memory; persistence is the
/// caller's job (see `agent::store`). Maintains the head hash and
/// per-issuer-per-epoch seq counters needed for fast `append`.
#[derive(Default, Debug, Clone)]
pub struct EventLog {
    events: Vec<Envelope>,
    /// Cached head hash so `append` is O(1). `None` when log is empty.
    head_hash: Option<EventHash>,
    /// Last accepted lease_epoch. Strictly non-decreasing.
    head_epoch: LeaseEpoch,
    /// Last accepted issuer at `head_epoch` (None when empty or right
    /// after epoch bump but no event yet).
    head_issuer: Option<NodeId>,
    /// Last accepted seq at (head_epoch, head_issuer).
    head_seq: Option<Sequence>,
}

impl EventLog {
    pub fn new() -> Self { Self::default() }

    pub fn len(&self) -> usize { self.events.len() }
    pub fn is_empty(&self) -> bool { self.events.is_empty() }

    /// All accepted envelopes, in append order.
    pub fn events(&self) -> &[Envelope] { &self.events }

    /// Hash of the head's signed bytes. Use as `prev_hash` when
    /// signing a new envelope. Returns `[0u8; 32]` for an empty log
    /// (the genesis prev_hash).
    pub fn head_hash(&self) -> EventHash {
        self.head_hash.unwrap_or([0u8; 32])
    }

    /// Highest accepted lease_epoch.
    pub fn head_epoch(&self) -> LeaseEpoch { self.head_epoch }

    /// Append a new envelope. Verifies all chain invariants and
    /// rejects with [`ChainError`] on any violation. The log is left
    /// unchanged on rejection (no partial writes).
    pub fn append(&mut self, env: Envelope, trust: &TrustStore) -> Result<(), ChainError> {
        // 1. We need the issuer's pubkey to verify the signature.
        let pubkey = trust.get(&env.core.issuer)
            .ok_or_else(|| ChainError::UnknownIssuer(env.core.issuer.clone()))?;

        // 2. Signature + schema version.
        env.verify(pubkey).map_err(ChainError::Verify)?;

        // 3. Chain-link: prev_hash matches our current head.
        if env.core.prev_hash != self.head_hash() {
            return Err(ChainError::PrevHashMismatch {
                expected: self.head_hash(),
                got: env.core.prev_hash,
            });
        }

        // 4. Lease epoch monotonic.
        if env.core.lease_epoch < self.head_epoch {
            return Err(ChainError::EpochRegression {
                head: self.head_epoch, got: env.core.lease_epoch,
            });
        }

        // 5–6. Sequence + issuer continuity rules:
        //   - new epoch (lease_epoch > head): seq must be 0, issuer is fresh
        //   - same epoch: issuer must match head_issuer; seq = head_seq + 1
        //   - empty log: lease_epoch arbitrary >=1, seq must be 0
        if self.events.is_empty() {
            if env.core.seq != 0 {
                return Err(ChainError::BadGenesisSeq(env.core.seq));
            }
        } else if env.core.lease_epoch > self.head_epoch {
            // epoch reset
            if env.core.seq != 0 {
                return Err(ChainError::EpochResetSeqNotZero {
                    new_epoch: env.core.lease_epoch, got_seq: env.core.seq,
                });
            }
        } else {
            // same epoch — issuer must continue, seq = prior + 1
            let head_issuer = self.head_issuer.as_ref().expect("non-empty log has issuer");
            if &env.core.issuer != head_issuer {
                return Err(ChainError::IssuerSwitchInEpoch {
                    epoch: self.head_epoch,
                    expected: head_issuer.clone(),
                    got: env.core.issuer.clone(),
                });
            }
            let want = self.head_seq.expect("non-empty log has seq") + 1;
            if env.core.seq != want {
                return Err(ChainError::SeqGap {
                    issuer: env.core.issuer.clone(),
                    expected: want,
                    got: env.core.seq,
                });
            }
        }

        // All checks passed — commit.
        let new_hash = env.core.hash();
        self.head_hash = Some(new_hash);
        self.head_epoch = env.core.lease_epoch;
        self.head_issuer = Some(env.core.issuer.clone());
        self.head_seq = Some(env.core.seq);
        self.events.push(env);
        Ok(())
    }
}

/// Reasons an append can fail. Each variant is a distinct invariant
/// violation so callers (and operators) can act appropriately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// The envelope's issuer is not in the trust store.
    UnknownIssuer(NodeId),
    /// Signature or schema-version check failed.
    Verify(VerifyError),
    /// `prev_hash` doesn't match our current head.
    PrevHashMismatch { expected: EventHash, got: EventHash },
    /// Lease epoch went backwards.
    EpochRegression { head: LeaseEpoch, got: LeaseEpoch },
    /// First event in chain (or after epoch reset) must have seq 0.
    BadGenesisSeq(Sequence),
    /// Epoch incremented but seq isn't 0.
    EpochResetSeqNotZero { new_epoch: LeaseEpoch, got_seq: Sequence },
    /// Within the same lease epoch, a different issuer tried to append —
    /// would mean split-brain (two simultaneous controllers).
    IssuerSwitchInEpoch { epoch: LeaseEpoch, expected: NodeId, got: NodeId },
    /// Same issuer, same epoch, but seq isn't monotonic +1.
    SeqGap { issuer: NodeId, expected: Sequence, got: Sequence },
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::UnknownIssuer(n) =>
                write!(f, "unknown issuer (not in trust store): {n}"),
            ChainError::Verify(e) =>
                write!(f, "envelope verify: {e}"),
            ChainError::PrevHashMismatch { expected, got } =>
                write!(f, "prev_hash mismatch: expected {}, got {}",
                       hex::encode(expected), hex::encode(got)),
            ChainError::EpochRegression { head, got } =>
                write!(f, "lease epoch regression: head={head}, got={got}"),
            ChainError::BadGenesisSeq(s) =>
                write!(f, "genesis event must have seq=0, got {s}"),
            ChainError::EpochResetSeqNotZero { new_epoch, got_seq } =>
                write!(f, "epoch reset to {new_epoch} requires seq=0, got {got_seq}"),
            ChainError::IssuerSwitchInEpoch { epoch, expected, got } =>
                write!(f, "issuer switch within epoch {epoch}: expected {expected}, got {got}"),
            ChainError::SeqGap { issuer, expected, got } =>
                write!(f, "seq gap for {issuer}: expected {expected}, got {got}"),
        }
    }
}
impl std::error::Error for ChainError {}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventBody, ENVELOPE_SCHEMA_VERSION};
    use crate::keypair::KeyPair;

    /// Convenience: sign an envelope chained off `log`'s current head.
    fn next_event(
        log: &EventLog,
        kp: &KeyPair,
        issuer: NodeId,
        seq: Sequence,
        epoch: LeaseEpoch,
        body: EventBody,
    ) -> Envelope {
        Envelope::sign(kp, issuer, seq, epoch, log.head_hash(), 1700000000, body)
    }

    fn fresh_pair() -> (KeyPair, NodeId, TrustStore) {
        let kp = KeyPair::generate();
        let id = NodeId::new("ctrl-deadbeefcafebabe");
        let mut trust = TrustStore::new();
        trust.insert(id.clone(), kp.public());
        (kp, id, trust)
    }

    #[test]
    fn empty_log_head_hash_is_zero_and_genesis_appends() {
        let mut log = EventLog::new();
        assert_eq!(log.head_hash(), [0u8; 32]);
        let (kp, issuer, trust) = fresh_pair();
        let env = next_event(&log, &kp, issuer.clone(), 0, 1,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(env, &trust).expect("genesis must append");
        assert_eq!(log.len(), 1);
        assert_ne!(log.head_hash(), [0u8; 32]);
    }

    #[test]
    fn unknown_issuer_is_rejected_before_signature_check() {
        // Don't insert the issuer's pubkey at all — append must fail
        // with UnknownIssuer, not BadSignature.
        let mut log = EventLog::new();
        let kp = KeyPair::generate();
        let issuer = NodeId::new("stranger-deadbeefcafebabe");
        let trust = TrustStore::new(); // empty
        let env = next_event(&log, &kp, issuer.clone(), 0, 1,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        let err = log.append(env, &trust).unwrap_err();
        assert!(matches!(err, ChainError::UnknownIssuer(n) if n == issuer));
    }

    #[test]
    fn forged_signature_is_rejected() {
        // Trust store maps issuer to MALLORY's pubkey, but the envelope
        // was actually signed by ALICE. Verify must catch this.
        let mut log = EventLog::new();
        let alice = KeyPair::generate();
        let mallory = KeyPair::generate();
        let issuer = NodeId::new("ctrl-deadbeefcafebabe");
        let mut trust = TrustStore::new();
        trust.insert(issuer.clone(), mallory.public()); // wrong pubkey

        let env = next_event(&log, &alice, issuer, 0, 1,
            EventBody::ReleaseLease { target_node: NodeId::new("agent-1111111111111111") });
        let err = log.append(env, &trust).unwrap_err();
        assert!(matches!(err, ChainError::Verify(VerifyError::BadSignature)));
    }

    #[test]
    fn happy_path_three_events_in_one_epoch() {
        let mut log = EventLog::new();
        let (kp, issuer, trust) = fresh_pair();

        for seq in 0..3 {
            let body = EventBody::Balloon {
                manifest_id: format!("m{seq}"), target_mb: 1024,
            };
            let env = next_event(&log, &kp, issuer.clone(), seq, 1, body);
            log.append(env, &trust).expect("happy-path append");
        }
        assert_eq!(log.len(), 3);
        assert_eq!(log.head_epoch(), 1);
    }

    #[test]
    fn seq_gap_within_epoch_is_rejected() {
        // Within the same epoch, seq must be exactly prior + 1. Skipping
        // 1 and going from 0 → 2 is a chain integrity violation.
        let mut log = EventLog::new();
        let (kp, issuer, trust) = fresh_pair();

        let e0 = next_event(&log, &kp, issuer.clone(), 0, 1,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(e0, &trust).unwrap();

        let bad = next_event(&log, &kp, issuer.clone(), 2, 1,
            EventBody::ReleaseLease { target_node: NodeId::new("agent-1111111111111111") });
        let err = log.append(bad, &trust).unwrap_err();
        assert!(matches!(err, ChainError::SeqGap { expected: 1, got: 2, .. }),
                "got: {err:?}");
    }

    #[test]
    fn epoch_increment_resets_sequence_to_zero() {
        // Lease handoff: new epoch, seq starts back at 0, new issuer.
        // The chain's head_seq for the OLD issuer is irrelevant.
        let mut log = EventLog::new();
        let alice_kp = KeyPair::generate();
        let alice = NodeId::new("alice-aaaaaaaaaaaaaaaa");
        let bob_kp = KeyPair::generate();
        let bob = NodeId::new("bob-bbbbbbbbbbbbbbbb");
        let mut trust = TrustStore::new();
        trust.insert(alice.clone(), alice_kp.public());
        trust.insert(bob.clone(), bob_kp.public());

        // Alice in epoch 1: two events.
        for seq in 0..2 {
            let env = next_event(&log, &alice_kp, alice.clone(), seq, 1,
                EventBody::Balloon { manifest_id: "x".into(), target_mb: 1024 });
            log.append(env, &trust).unwrap();
        }
        // Bob takes over in epoch 2 with seq=0 — must succeed.
        let env = next_event(&log, &bob_kp, bob.clone(), 0, 2,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(env, &trust).expect("epoch-2 genesis must append");
        assert_eq!(log.head_epoch(), 2);
    }

    #[test]
    fn epoch_reset_with_nonzero_seq_is_rejected() {
        // New epoch must restart seq at 0; otherwise replay attacks
        // could splice in events from "the future."
        let mut log = EventLog::new();
        let (kp_a, alice, mut trust) = fresh_pair();
        let kp_b = KeyPair::generate();
        let bob = NodeId::new("bob-bbbbbbbbbbbbbbbb");
        trust.insert(bob.clone(), kp_b.public());

        let g = next_event(&log, &kp_a, alice, 0, 1,
            EventBody::ReleaseLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(g, &trust).unwrap();

        let bad = next_event(&log, &kp_b, bob, 5, 2,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        let err = log.append(bad, &trust).unwrap_err();
        assert!(matches!(err, ChainError::EpochResetSeqNotZero { new_epoch: 2, got_seq: 5 }));
    }

    #[test]
    fn epoch_regression_is_rejected() {
        // Once we accept epoch 2, nothing from epoch 1 may be appended.
        let mut log = EventLog::new();
        let (kp_a, alice, mut trust) = fresh_pair();
        let kp_b = KeyPair::generate();
        let bob = NodeId::new("bob-bbbbbbbbbbbbbbbb");
        trust.insert(bob.clone(), kp_b.public());

        let e0 = next_event(&log, &kp_a, alice.clone(), 0, 1,
            EventBody::ReleaseLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(e0, &trust).unwrap();
        let e1 = next_event(&log, &kp_b, bob, 0, 2,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(e1, &trust).unwrap();

        // Try to sneak alice's epoch-1 event in after bob took over.
        let stale = next_event(&log, &kp_a, alice, 1, 1,
            EventBody::Unassign { manifest_id: "ci-runner".into() });
        let err = log.append(stale, &trust).unwrap_err();
        assert!(matches!(err, ChainError::EpochRegression { head: 2, got: 1 }));
    }

    #[test]
    fn split_brain_within_epoch_is_rejected() {
        // Within the SAME lease_epoch, only the first issuer may
        // append. A second issuer claiming epoch 1 mid-stream would be
        // split-brain (or replay) and is rejected.
        let mut log = EventLog::new();
        let (kp_a, alice, mut trust) = fresh_pair();
        let kp_b = KeyPair::generate();
        let bob = NodeId::new("bob-bbbbbbbbbbbbbbbb");
        trust.insert(bob.clone(), kp_b.public());

        let e0 = next_event(&log, &kp_a, alice.clone(), 0, 1,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(e0, &trust).unwrap();

        // Bob tries to inject seq 1 in alice's epoch.
        let bad = next_event(&log, &kp_b, bob, 1, 1,
            EventBody::Balloon { manifest_id: "x".into(), target_mb: 1024 });
        let err = log.append(bad, &trust).unwrap_err();
        assert!(matches!(err, ChainError::IssuerSwitchInEpoch { .. }));
    }

    #[test]
    fn prev_hash_mismatch_is_rejected() {
        // Forged prev_hash (not equal to the actual head) means the
        // event was constructed against a different chain history.
        // Rejecting catches both replay and rebase attempts.
        let mut log = EventLog::new();
        let (kp, issuer, trust) = fresh_pair();
        let e0 = next_event(&log, &kp, issuer.clone(), 0, 1,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(e0, &trust).unwrap();

        // Build an envelope whose prev_hash is NOT the current head.
        let mut bad = next_event(&log, &kp, issuer.clone(), 1, 1,
            EventBody::Balloon { manifest_id: "x".into(), target_mb: 1024 });
        bad.core.prev_hash = [0xAAu8; 32];
        // Re-sign to keep the signature valid (otherwise we'd hit
        // BadSignature first; we want to isolate the prev_hash check).
        let signed = bad.core.signed_bytes();
        bad.signature = kp.sign(&signed);

        let err = log.append(bad, &trust).unwrap_err();
        assert!(matches!(err, ChainError::PrevHashMismatch { .. }));
    }

    #[test]
    fn rejected_append_does_not_mutate_log() {
        // Crucial invariant: a failed append must not change head_hash,
        // head_epoch, head_seq, or events. Otherwise downstream callers
        // could observe partial writes.
        let mut log = EventLog::new();
        let (kp, issuer, trust) = fresh_pair();
        let e0 = next_event(&log, &kp, issuer.clone(), 0, 1,
            EventBody::ClaimLease { target_node: NodeId::new("agent-1111111111111111") });
        log.append(e0, &trust).unwrap();
        let snapshot = (log.head_hash(), log.head_epoch(), log.len());

        let bad = next_event(&log, &kp, issuer.clone(), 7, 1, // seq gap
            EventBody::ReleaseLease { target_node: NodeId::new("agent-1111111111111111") });
        let _ = log.append(bad, &trust).unwrap_err();

        assert_eq!(snapshot.0, log.head_hash(), "head_hash unchanged");
        assert_eq!(snapshot.1, log.head_epoch(), "head_epoch unchanged");
        assert_eq!(snapshot.2, log.len(),       "len unchanged");
    }

    #[test]
    fn schema_version_constant_is_one_for_phase_one() {
        // If the schema version moves from 1, this test surfaces it
        // and forces the author to update the tests + agent compat.
        assert_eq!(ENVELOPE_SCHEMA_VERSION, 1);
    }
}
