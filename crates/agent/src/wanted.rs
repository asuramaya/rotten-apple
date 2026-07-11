//! Desired-state ("wanted") store + event application.
//!
//! `WantedState` is the agent's authoritative view of what should be
//! running on this node. It's populated by applying signed events
//! from the chain: each `Assign` adds or replaces an entry; each
//! `Unassign` removes one; `ClaimLease`/`ReleaseLease` flip the
//! current lease holder.
//!
//! The state is a pure data structure. Persistence is the agent's
//! job (SQLite or flat file); this module is the in-memory shape +
//! the rules for mutating it.

use rotten_apple_fabric::{EventBody, LeaseEpoch, NodeId};
use rotten_apple_manifest::Profile;
use std::collections::BTreeMap;

/// One assigned manifest, plus the lease-epoch under which it was
/// last set. Used to evict stale assignments when a new controller
/// takes over and re-declares only some of them.
///
/// Equality compares the wire-canonical form (manifest_id, TOML,
/// epoch) — NOT the parsed cache, which is derived. This means two
/// entries with the same TOML compare equal even if only one has
/// `parsed` populated.
#[derive(Clone, Debug)]
pub struct WantedEntry {
    pub manifest_id: String,
    pub profile_toml: String,
    /// Cached parse — kept fresh by `apply_event` since manifest_toml
    /// is the wire-canonical form. None on parse error so the agent
    /// can flag the manifest without crashing the reconcile loop.
    pub parsed: Option<Profile>,
    /// Lease epoch at which this manifest was assigned. New
    /// controllers can supersede; nothing else moves this.
    pub assigned_at_epoch: LeaseEpoch,
}

impl PartialEq for WantedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.manifest_id == other.manifest_id
            && self.profile_toml == other.profile_toml
            && self.assigned_at_epoch == other.assigned_at_epoch
    }
}
impl Eq for WantedEntry {}

/// The agent's local desired-state. Sorted (BTreeMap) to keep
/// iteration deterministic — important for reconcile plans we'd like
/// to compare across runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WantedState {
    /// Manifests currently assigned to this agent.
    pub manifests: BTreeMap<String, WantedEntry>,
    /// Current lease holder (controller that may issue events).
    /// `None` = unleased; agent refuses state-mutating events.
    pub lease_holder: Option<NodeId>,
    /// Current lease epoch. Increases on every successful ClaimLease.
    pub lease_epoch: LeaseEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// Event tried to mutate state without a held lease.
    NoLease,
    /// Event came from a controller that doesn't currently hold the
    /// lease.
    WrongIssuer { expected: NodeId, got: NodeId },
    /// ClaimLease tried to take over with an epoch <= current.
    StaleClaim { current: LeaseEpoch, got: LeaseEpoch },
    /// Manifest wire-format failed to parse.
    BadManifest(String),
    /// Heartbeats are observational; applying one to wanted-state is
    /// a programming error in the caller.
    HeartbeatNotMutating,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::NoLease =>
                f.write_str("no lease held; agent refuses mutating events"),
            ApplyError::WrongIssuer { expected, got } =>
                write!(f, "issuer {got} does not hold lease (held by {expected})"),
            ApplyError::StaleClaim { current, got } =>
                write!(f, "stale lease claim: current epoch {current}, got {got}"),
            ApplyError::BadManifest(s) =>
                write!(f, "manifest parse error: {s}"),
            ApplyError::HeartbeatNotMutating =>
                f.write_str("heartbeats are observational, not state-mutating"),
        }
    }
}
impl std::error::Error for ApplyError {}

impl WantedState {
    pub fn new() -> Self { Self::default() }

    /// Parse the manifest's profile from its TOML; cached on the
    /// entry. None on failure so the reconcile loop can mark it.
    fn parse(toml: &str) -> Option<Profile> { Profile::from_str(toml).ok() }

    /// Apply a chain event to wanted-state. Caller must verify the
    /// event's signature + chain position FIRST (via `chain::EventLog`).
    /// Here we only enforce the *semantic* rules around lease + issuer.
    pub fn apply_event(
        &mut self,
        issuer: &NodeId,
        epoch: LeaseEpoch,
        body: &EventBody,
    ) -> Result<(), ApplyError> {
        match body {
            EventBody::ClaimLease { .. } => {
                if epoch <= self.lease_epoch && self.lease_holder.is_some() {
                    return Err(ApplyError::StaleClaim {
                        current: self.lease_epoch, got: epoch,
                    });
                }
                self.lease_holder = Some(issuer.clone());
                self.lease_epoch = epoch;
                Ok(())
            }
            EventBody::ReleaseLease { .. } => {
                self.require_lease(issuer)?;
                self.lease_holder = None;
                // We DON'T bump the epoch on release; the next
                // ClaimLease must come with a strictly higher epoch
                // (controllers handing-off must coordinate epochs).
                Ok(())
            }
            EventBody::Assign { manifest_id, manifest_toml } => {
                self.require_lease(issuer)?;
                let parsed = Self::parse(manifest_toml);
                if parsed.is_none() {
                    return Err(ApplyError::BadManifest(manifest_id.clone()));
                }
                self.manifests.insert(manifest_id.clone(), WantedEntry {
                    manifest_id: manifest_id.clone(),
                    profile_toml: manifest_toml.clone(),
                    parsed,
                    assigned_at_epoch: epoch,
                });
                Ok(())
            }
            EventBody::Unassign { manifest_id } => {
                self.require_lease(issuer)?;
                self.manifests.remove(manifest_id);
                Ok(())
            }
            EventBody::Balloon { .. } => {
                // Balloon is a runtime command — it does NOT mutate
                // wanted-state. The reconciler reads it as transient
                // and applies via the backend; nothing to record here.
                self.require_lease(issuer)?;
                Ok(())
            }
            EventBody::Heartbeat { .. } => Err(ApplyError::HeartbeatNotMutating),
        }
    }

    fn require_lease(&self, issuer: &NodeId) -> Result<(), ApplyError> {
        match &self.lease_holder {
            None => Err(ApplyError::NoLease),
            Some(h) if h != issuer => Err(ApplyError::WrongIssuer {
                expected: h.clone(),
                got: issuer.clone(),
            }),
            Some(_) => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl_a() -> NodeId { NodeId::new("alice-aaaaaaaaaaaaaaaa") }
    fn ctrl_b() -> NodeId { NodeId::new("bob-bbbbbbbbbbbbbbbb") }
    fn target() -> NodeId { NodeId::new("agent-1111111111111111") }

    fn minimal_profile_toml(name: &str) -> String {
        format!(r#"
[profile]
name = "{name}"
type = "appliance"
[resources]
memory_active = "1G"
memory_idle = "256M"
memory_minimum = "128M"
vcpus_active = 1
vcpus_idle = 1
vcpus_minimum = 1
[storage]
root = {{ kind = "qcow2", path = "/tmp/{name}.qcow2" }}
"#)
    }

    #[test]
    fn fresh_state_has_no_lease_and_no_manifests() {
        let s = WantedState::new();
        assert!(s.lease_holder.is_none());
        assert_eq!(s.lease_epoch, 0);
        assert!(s.manifests.is_empty());
    }

    #[test]
    fn assign_without_lease_is_refused() {
        // The agent's first line of defense — without a held lease,
        // no mutation. Pin that Assign returns NoLease, not panic, not
        // silent acceptance.
        let mut s = WantedState::new();
        let err = s.apply_event(&ctrl_a(), 1, &EventBody::Assign {
            manifest_id: "ci".into(),
            manifest_toml: minimal_profile_toml("ci"),
        }).unwrap_err();
        assert_eq!(err, ApplyError::NoLease);
        assert!(s.manifests.is_empty());
    }

    #[test]
    fn claim_lease_seats_the_controller_and_assign_now_works() {
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() })
            .expect("genesis claim must succeed");
        assert_eq!(s.lease_holder.as_ref(), Some(&ctrl_a()));
        assert_eq!(s.lease_epoch, 1);

        s.apply_event(&ctrl_a(), 1, &EventBody::Assign {
            manifest_id: "ci".into(),
            manifest_toml: minimal_profile_toml("ci"),
        }).expect("assign under valid lease");
        assert_eq!(s.manifests.len(), 1);
        let entry = s.manifests.get("ci").unwrap();
        assert_eq!(entry.assigned_at_epoch, 1);
        assert!(entry.parsed.is_some(), "valid TOML must round-trip into a Profile");
    }

    #[test]
    fn assign_from_non_lease_holder_is_refused() {
        // Defense in depth: even after a lease is held, only that
        // controller's events apply. If Bob signs an Assign while
        // Alice holds the lease, the agent rejects.
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() }).unwrap();
        let err = s.apply_event(&ctrl_b(), 1, &EventBody::Assign {
            manifest_id: "x".into(), manifest_toml: minimal_profile_toml("x"),
        }).unwrap_err();
        assert_eq!(err, ApplyError::WrongIssuer {
            expected: ctrl_a(), got: ctrl_b(),
        });
    }

    #[test]
    fn lease_handoff_via_release_then_higher_claim() {
        // The intended pattern: holder releases, next holder claims
        // with a strictly higher epoch.
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() }).unwrap();
        s.apply_event(&ctrl_a(), 1, &EventBody::ReleaseLease { target_node: target() }).unwrap();
        assert!(s.lease_holder.is_none());
        assert_eq!(s.lease_epoch, 1, "release does NOT bump epoch");

        // Bob claims at epoch 2 — must succeed.
        s.apply_event(&ctrl_b(), 2, &EventBody::ClaimLease { target_node: target() })
            .expect("higher-epoch claim takes over");
        assert_eq!(s.lease_holder.as_ref(), Some(&ctrl_b()));
        assert_eq!(s.lease_epoch, 2);
    }

    #[test]
    fn stale_claim_is_refused() {
        // Bob can't claim with epoch <= current while alice holds it.
        // Pin: the error names BOTH epochs so operators can debug.
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 5, &EventBody::ClaimLease { target_node: target() }).unwrap();
        let err = s.apply_event(&ctrl_b(), 5, &EventBody::ClaimLease { target_node: target() })
            .unwrap_err();
        assert_eq!(err, ApplyError::StaleClaim { current: 5, got: 5 });
    }

    #[test]
    fn bad_manifest_toml_is_caught() {
        // Malformed TOML in an Assign would otherwise leave a wanted
        // entry with `parsed = None` silently. Pin: parse failure is
        // a hard error at apply time so the controller hears about it.
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() }).unwrap();
        let err = s.apply_event(&ctrl_a(), 1, &EventBody::Assign {
            manifest_id: "broken".into(),
            manifest_toml: "this is not valid toml [[[".into(),
        }).unwrap_err();
        assert_eq!(err, ApplyError::BadManifest("broken".into()));
        assert!(s.manifests.is_empty(), "no entry persisted on parse error");
    }

    #[test]
    fn unassign_removes_the_entry() {
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() }).unwrap();
        s.apply_event(&ctrl_a(), 1, &EventBody::Assign {
            manifest_id: "ci".into(), manifest_toml: minimal_profile_toml("ci"),
        }).unwrap();
        assert_eq!(s.manifests.len(), 1);
        s.apply_event(&ctrl_a(), 1, &EventBody::Unassign { manifest_id: "ci".into() }).unwrap();
        assert!(s.manifests.is_empty());
    }

    #[test]
    fn unassign_of_unknown_manifest_is_idempotent_noop() {
        // Don't error on unknown unassign — it's the desired end-state
        // either way. Removes ambiguity for retries.
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() }).unwrap();
        s.apply_event(&ctrl_a(), 1, &EventBody::Unassign { manifest_id: "ghost".into() })
            .expect("unassign of absent must be ok");
    }

    #[test]
    fn balloon_does_not_persist_into_wanted_state() {
        // Balloon is transient; it does NOT mutate wanted-state.
        // Pin that the wanted-state's manifests dict is unchanged.
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() }).unwrap();
        s.apply_event(&ctrl_a(), 1, &EventBody::Assign {
            manifest_id: "ci".into(), manifest_toml: minimal_profile_toml("ci"),
        }).unwrap();
        let snapshot = s.manifests.clone();
        s.apply_event(&ctrl_a(), 1, &EventBody::Balloon {
            manifest_id: "ci".into(), target_mb: 2048,
        }).unwrap();
        assert_eq!(s.manifests, snapshot, "balloon must not touch wanted-state");
    }

    #[test]
    fn heartbeat_into_apply_event_is_a_caller_bug() {
        // Heartbeats ride on the chain for ordering but are
        // observational. Funnelling one through apply_event is a
        // programming error — surface it loudly.
        let mut s = WantedState::new();
        let err = s.apply_event(&ctrl_a(), 1, &EventBody::Heartbeat {
            free_memory_mb: 0, free_vcpus: 0,
            running: vec![], last_applied_hash: None,
        }).unwrap_err();
        assert_eq!(err, ApplyError::HeartbeatNotMutating);
    }

    #[test]
    fn assign_replaces_existing_entry_with_new_epoch() {
        // Same manifest_id reassigned at a higher epoch overwrites
        // the prior entry — the controller wins.
        let mut s = WantedState::new();
        s.apply_event(&ctrl_a(), 1, &EventBody::ClaimLease { target_node: target() }).unwrap();
        s.apply_event(&ctrl_a(), 1, &EventBody::Assign {
            manifest_id: "ci".into(), manifest_toml: minimal_profile_toml("ci"),
        }).unwrap();
        // Lease handoff to bob, who reassigns ci with same id.
        s.apply_event(&ctrl_a(), 1, &EventBody::ReleaseLease { target_node: target() }).unwrap();
        s.apply_event(&ctrl_b(), 2, &EventBody::ClaimLease { target_node: target() }).unwrap();
        s.apply_event(&ctrl_b(), 2, &EventBody::Assign {
            manifest_id: "ci".into(), manifest_toml: minimal_profile_toml("ci"),
        }).unwrap();
        let e = s.manifests.get("ci").unwrap();
        assert_eq!(e.assigned_at_epoch, 2,
                   "later epoch must win — controller continuity over time");
    }
}
