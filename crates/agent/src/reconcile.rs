//! Reconcile algorithm: `wanted × truth → plan`.
//!
//! Pure function. Takes the agent's current [`WantedState`] +
//! [`Truth`] snapshot from the backend, produces an ordered list of
//! [`ReconcileAction`]s. The actuator (orchestratord's libxl actor)
//! is responsible for executing the plan; the reconciler doesn't
//! touch the backend.
//!
//! Decision rules — wanted side:
//!
//! For each `WantedEntry`:
//! - if anchor pins to a different node → `WrongAnchor` (plan skips it,
//!   alerts the operator; controller assigned to the wrong agent)
//! - if no domain in truth matches the manifest_id → `Create`
//! - if domain exists but is `Absent` → `Create` (orphan recovery)
//! - if domain exists and is `Transient` → `WaitOneCycle`
//! - if domain exists and is `Running` AND memory_mb differs from
//!   wanted active size → `Balloon`
//! - if domain exists and is `Running` and matches → `NoOp`
//!
//! Decision rules — truth side:
//!
//! For each domain in truth not covered above:
//! - if `manifest_id` is None → `LeaveOrphan` (manual, hands-off)
//! - if `manifest_id` is Some(unknown) → `DestroyUnknown` (controller
//!   no longer wants this; agent removes it). Anchored manifests with
//!   the special `"this-host"` value are recognized as locally-
//!   declared and preserved without controller assignment — the
//!   user-desktop guest path on a laptop.
//!
//! Stability invariant: the reconcile output is deterministic given
//! the same inputs. The plan's actions are sorted (by manifest_id /
//! domain_id) for diff-friendliness in tests.

use crate::truth::{DomainState, Truth};
use crate::wanted::{WantedEntry, WantedState};
use rotten_apple_fabric::NodeId;
use rotten_apple_manifest::Profile;

/// One action the actuator should perform. The plan is a `Vec<Self>`.
/// New variants extend the language; the actuator MUST handle every
/// variant (we keep the list small on purpose).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Manifest is wanted but not running anywhere — start a fresh
    /// domain from this profile.
    Create { manifest_id: String },

    /// Domain is running but its memory size doesn't match wanted —
    /// balloon to target.
    Balloon { manifest_id: String, domain_id: String, target_mb: u64 },

    /// Domain exists but no manifest_id matches it; the wanted-state
    /// no longer references it. Stop + destroy.
    DestroyUnknown { domain_id: String },

    /// Manifest pins to a different node; do not start here. The
    /// reconciler emits this so the controller hears about it
    /// (typically via the next heartbeat) and reassigns elsewhere.
    WrongAnchor { manifest_id: String, expected_node: String, this_node: String },

    /// Manifest's TOML failed to parse. Operator must fix it; agent
    /// won't start a guest from a broken profile.
    BadManifest { manifest_id: String },

    /// Manifest's anchor is `"this-host"` AND the manifest hasn't
    /// been seen from a controller — locally-declared (e.g. the
    /// user-desktop guest on a laptop). Reconciler emits this only
    /// for visibility; no actuator side-effect.
    Local { manifest_id: String },
}

/// Ordered list of actions for the actuator, plus a flag for whether
/// any cycle-deferred work was emitted (so the agent knows to retick).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    pub actions: Vec<ReconcileAction>,
    /// Some manifest had a transient backend state and we're waiting
    /// it out. Caller should re-run reconcile after a short delay.
    pub retick_needed: bool,
}

/// Compute the reconcile plan. `this_node` is required so we can
/// honor `anchor.node` correctly: anything pinned to a different node
/// becomes a [`ReconcileAction::WrongAnchor`].
pub fn reconcile(
    wanted: &WantedState,
    truth: &Truth,
    this_node: &NodeId,
) -> ReconcilePlan {
    let mut plan = ReconcilePlan::default();

    // Sort wanted by manifest_id (BTreeMap iterates sorted) so the
    // output is deterministic.
    for (manifest_id, entry) in &wanted.manifests {
        if let Some(action) = consider_wanted(entry, manifest_id, truth, this_node, &mut plan) {
            plan.actions.push(action);
        }
    }

    // Now scan truth for domains the wanted-state didn't cover.
    let wanted_ids: std::collections::HashSet<&str> =
        wanted.manifests.keys().map(|s| s.as_str()).collect();
    for (domain_id, t_entry) in &truth.domains {
        let Some(mid) = t_entry.manifest_id.as_deref() else {
            // Orphan — leave alone (manual ops boundary).
            continue;
        };
        if wanted_ids.contains(mid) {
            // Covered by the wanted-side pass above.
            continue;
        }
        // Manifest_id present but not wanted: controller withdrew it.
        plan.actions.push(ReconcileAction::DestroyUnknown {
            domain_id: domain_id.clone(),
        });
    }

    plan
}

/// Decide what to do with one wanted entry. Returns Some(action) for
/// the plan, None if nothing visible should be emitted (running &
/// matching state). May set `plan.retick_needed`.
fn consider_wanted(
    entry: &WantedEntry,
    manifest_id: &str,
    truth: &Truth,
    this_node: &NodeId,
    plan: &mut ReconcilePlan,
) -> Option<ReconcileAction> {
    let Some(profile) = entry.parsed.as_ref() else {
        return Some(ReconcileAction::BadManifest {
            manifest_id: manifest_id.to_string(),
        });
    };

    // anchor check first — pinned-elsewhere is a hard skip.
    if let Some(anchor_to) = profile.anchor.node.as_deref()
        && anchor_to != "this-host"
        && anchor_to != this_node.as_str()
        && anchor_to != this_node.hex_suffix()
    {
        return Some(ReconcileAction::WrongAnchor {
            manifest_id: manifest_id.to_string(),
            expected_node: anchor_to.to_string(),
            this_node: this_node.as_str().to_string(),
        });
    }

    // Now compare against truth.
    let observed = truth.find_by_manifest(manifest_id);
    match observed {
        None => Some(ReconcileAction::Create { manifest_id: manifest_id.to_string() }),
        Some(t) => match t.state {
            DomainState::Absent =>
                Some(ReconcileAction::Create { manifest_id: manifest_id.to_string() }),
            DomainState::Transient => {
                // Don't act this cycle. Retick after a short delay.
                plan.retick_needed = true;
                None
            }
            DomainState::Running => {
                // Running and matched — only act if memory drifted.
                let want_mb = profile_memory_active_mb(profile);
                if want_mb != t.memory_mb {
                    return Some(ReconcileAction::Balloon {
                        manifest_id: manifest_id.to_string(),
                        domain_id: t.domain_id.clone(),
                        target_mb: want_mb,
                    });
                }
                None
            }
        },
    }
}

fn profile_memory_active_mb(p: &Profile) -> u64 {
    // memory_active is bytes; balloon target is MB.
    p.resources.memory_active_bytes / (1024 * 1024)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truth::{DomainState, Truth, TruthEntry};
    use rotten_apple_fabric::EventBody;

    fn this_node() -> NodeId { NodeId::new("agent-1111111111111111") }
    fn other_node() -> NodeId { NodeId::new("desk-box-2222222222222222") }
    fn ctrl() -> NodeId { NodeId::new("ctrl-deadbeefcafebabe") }

    fn profile_toml(name: &str, anchor_node: Option<&str>) -> String {
        let anchor_block = match anchor_node {
            Some(n) => format!("[anchor]\nnode = \"{n}\"\n"),
            None => String::new(),
        };
        format!(r#"
[profile]
name = "{name}"
type = "appliance"
[resources]
memory_active = "1024M"
memory_idle = "256M"
memory_minimum = "128M"
vcpus_active = 1
vcpus_idle = 1
vcpus_minimum = 1
[storage]
root = {{ kind = "qcow2", path = "/tmp/{name}.qcow2" }}
{anchor_block}"#)
    }

    fn wanted_with_one(name: &str, anchor: Option<&str>) -> WantedState {
        let mut s = WantedState::new();
        s.apply_event(&ctrl(), 1, &EventBody::ClaimLease {
            target_node: this_node(),
        }).unwrap();
        s.apply_event(&ctrl(), 1, &EventBody::Assign {
            manifest_id: name.into(),
            manifest_toml: profile_toml(name, anchor),
        }).unwrap();
        s
    }

    #[test]
    fn empty_inputs_yield_empty_plan() {
        // No work in either direction — plan is empty, no retick.
        let p = reconcile(&WantedState::new(), &Truth::new(), &this_node());
        assert!(p.actions.is_empty());
        assert!(!p.retick_needed);
    }

    #[test]
    fn wanted_but_not_running_emits_create() {
        let w = wanted_with_one("ci", None);
        let p = reconcile(&w, &Truth::new(), &this_node());
        assert_eq!(p.actions, vec![
            ReconcileAction::Create { manifest_id: "ci".into() },
        ]);
    }

    #[test]
    fn wanted_and_running_emits_no_action() {
        // Steady state: wanted matches truth, sizes agree, no work.
        let w = wanted_with_one("ci", None);
        let mut t = Truth::new();
        t.insert(TruthEntry {
            domain_id: "42".into(),
            manifest_id: Some("ci".into()),
            state: DomainState::Running,
            memory_mb: 1024, vcpus: 1,
        });
        let p = reconcile(&w, &t, &this_node());
        assert!(p.actions.is_empty(), "steady state must be quiet: {:?}", p.actions);
    }

    #[test]
    fn memory_drift_emits_balloon() {
        // Running guest at 512 MB; wanted active is 1024 MB; balloon up.
        let w = wanted_with_one("ci", None);
        let mut t = Truth::new();
        t.insert(TruthEntry {
            domain_id: "42".into(),
            manifest_id: Some("ci".into()),
            state: DomainState::Running,
            memory_mb: 512, vcpus: 1,
        });
        let p = reconcile(&w, &t, &this_node());
        assert_eq!(p.actions, vec![
            ReconcileAction::Balloon {
                manifest_id: "ci".into(),
                domain_id: "42".into(),
                target_mb: 1024,
            },
        ]);
    }

    #[test]
    fn transient_state_defers_with_retick_flag() {
        // Domain in flight (creating, paused, etc.) — leave alone for
        // one cycle. The reconciler signals the agent to retick soon.
        let w = wanted_with_one("ci", None);
        let mut t = Truth::new();
        t.insert(TruthEntry {
            domain_id: "42".into(),
            manifest_id: Some("ci".into()),
            state: DomainState::Transient,
            memory_mb: 0, vcpus: 0,
        });
        let p = reconcile(&w, &t, &this_node());
        assert!(p.actions.is_empty(),
                "transient must produce NO actions this cycle");
        assert!(p.retick_needed, "transient must request a retick");
    }

    #[test]
    fn unknown_domain_in_truth_is_destroyed() {
        // Truth has a domain whose manifest_id isn't in wanted —
        // controller withdrew the assignment. Stop + destroy.
        let w = WantedState::new();
        let mut t = Truth::new();
        t.insert(TruthEntry {
            domain_id: "42".into(),
            manifest_id: Some("orphan-ci".into()),
            state: DomainState::Running,
            memory_mb: 1024, vcpus: 1,
        });
        let p = reconcile(&w, &t, &this_node());
        assert_eq!(p.actions, vec![
            ReconcileAction::DestroyUnknown { domain_id: "42".into() },
        ]);
    }

    #[test]
    fn truly_orphan_domains_are_left_alone() {
        // manifest_id = None → manual ops boundary. Reconciler MUST
        // not act on these (someone may have run `xl create` outside
        // the mesh).
        let w = WantedState::new();
        let mut t = Truth::new();
        t.insert(TruthEntry {
            domain_id: "42".into(),
            manifest_id: None,
            state: DomainState::Running,
            memory_mb: 1024, vcpus: 1,
        });
        let p = reconcile(&w, &t, &this_node());
        assert!(p.actions.is_empty(),
                "orphan domains (no manifest_id) must be left alone");
    }

    #[test]
    fn anchor_to_other_node_emits_wrong_anchor() {
        // Controller assigned a manifest pinned to a different node.
        // Agent refuses to start it AND surfaces the mismatch.
        let w = wanted_with_one("ci", Some("desk-box-2222222222222222"));
        let p = reconcile(&w, &Truth::new(), &this_node());
        assert_eq!(p.actions, vec![
            ReconcileAction::WrongAnchor {
                manifest_id: "ci".into(),
                expected_node: "desk-box-2222222222222222".into(),
                this_node: this_node().as_str().to_string(),
            },
        ]);
    }

    #[test]
    fn anchor_this_host_runs_locally() {
        // The "this-host" magic value resolves to the local node, so
        // a manifest pinned with anchor.node="this-host" runs here
        // regardless of which node we are. Pin: matches like a wildcard.
        let w = wanted_with_one("ci", Some("this-host"));
        let p = reconcile(&w, &Truth::new(), &this_node());
        assert_eq!(p.actions, vec![
            ReconcileAction::Create { manifest_id: "ci".into() },
        ]);
    }

    #[test]
    fn anchor_to_our_full_id_or_hex_runs_locally() {
        // Both forms — full "agent-..." string and bare hex — must
        // count as "this is me." Pin both to head off "node id renamed
        // and now my pin doesn't match" footguns.
        let full = this_node().as_str().to_string();
        let hex  = this_node().hex_suffix().to_string();
        for pinned in [full, hex] {
            let w = wanted_with_one("ci", Some(&pinned));
            let p = reconcile(&w, &Truth::new(), &this_node());
            assert_eq!(p.actions, vec![
                ReconcileAction::Create { manifest_id: "ci".into() },
            ], "pin form should match: {pinned:?}");
        }
    }

    #[test]
    fn deterministic_action_order() {
        // Two wanted manifests + one running; ensure plan order is
        // stable across runs (BTreeMap iteration). Important for
        // diff-friendly tests and for stable cockpit output.
        let mut s = WantedState::new();
        s.apply_event(&ctrl(), 1, &EventBody::ClaimLease { target_node: this_node() }).unwrap();
        for name in ["b-runner", "a-runner", "c-runner"] {
            s.apply_event(&ctrl(), 1, &EventBody::Assign {
                manifest_id: name.into(),
                manifest_toml: profile_toml(name, None),
            }).unwrap();
        }
        let plan1 = reconcile(&s, &Truth::new(), &this_node());
        let plan2 = reconcile(&s, &Truth::new(), &this_node());
        assert_eq!(plan1, plan2);
        // First action manifest_id should be alphabetic-first.
        match &plan1.actions[0] {
            ReconcileAction::Create { manifest_id } => assert_eq!(manifest_id, "a-runner"),
            other => panic!("expected Create, got {other:?}"),
        }
    }
}
