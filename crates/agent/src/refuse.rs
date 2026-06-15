//! Refusal layer — policy gate before applying a controller event.
//!
//! Sits between the chain (which proves "this issuer signed this
//! event") and the wanted-state apply (which mutates state). The
//! chain answers *who said it*; the refusal layer answers *are they
//! allowed to say it for this manifest*.
//!
//! Two checks per event:
//!
//! 1. **Controller allowlist** — if the manifest's `lease_policy.
//!    controllers_allowed` is non-empty, the issuer MUST be in it.
//!    Empty = permissive (any lease holder may control).
//!
//! 2. **Operation allowlist** — if `lease_policy.operations_allowed`
//!    is non-empty, the event's operation kind MUST be listed.
//!    Empty = the agent applies its compiled-in *default safe set*
//!    (`assign`, `unassign`, `balloon`, `start`, `shutdown`).
//!    Sensitive operations (`destroy`, `attach_disk`, `migrate_to`)
//!    MUST be listed explicitly to be allowed.
//!
//! Heartbeat + lease events are NOT manifest-scoped; they pass
//! through this layer unconditionally (they're authenticated by the
//! chain's lease-holder rules instead).

use rotten_apple_fabric::{EventBody, NodeId};
use rotten_apple_manifest::Profile;

/// Why a controller event was refused. Returned in place of the
/// regular apply result so callers can log or surface to the issuer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalReason {
    /// The manifest's controller allowlist doesn't include the issuer.
    ControllerNotAllowed { issuer: NodeId, manifest_id: String },
    /// The operation kind isn't in the manifest's allowlist (and
    /// isn't in the default safe set when the manifest's list is empty).
    OperationNotAllowed { op: String, manifest_id: String },
    /// The event references a manifest the agent knows nothing about
    /// (Balloon for an unassigned manifest, etc.). The chain might
    /// accept it; we refuse because we have no policy to consult.
    UnknownManifest { manifest_id: String },
}

impl std::fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefusalReason::ControllerNotAllowed { issuer, manifest_id } =>
                write!(f, "controller {issuer} not in {manifest_id}.lease_policy.controllers_allowed"),
            RefusalReason::OperationNotAllowed { op, manifest_id } =>
                write!(f, "operation {op:?} not allowed for {manifest_id}"),
            RefusalReason::UnknownManifest { manifest_id } =>
                write!(f, "no policy for unknown manifest {manifest_id:?}"),
        }
    }
}
impl std::error::Error for RefusalReason {}

/// The compiled-in default safe set, applied when a manifest's
/// `lease_policy.operations_allowed` is empty. Picked so that the
/// "I have no special policy" case doesn't accidentally permit
/// destructive operations.
const DEFAULT_SAFE_OPS: &[&str] = &[
    "assign", "unassign", "balloon", "start", "shutdown",
];

/// Inspect an incoming event and decide whether to apply it.
///
/// `lookup_profile` is a closure so callers can supply different
/// resolution strategies (in-memory wanted-state map, SQLite query,
/// etc.) without coupling the refusal layer to a specific store.
///
/// Returns `Ok(())` to permit, `Err(RefusalReason)` to reject.
pub fn may_apply<F>(
    issuer: &NodeId,
    body: &EventBody,
    lookup_profile: F,
) -> Result<(), RefusalReason>
where
    F: Fn(&str) -> Option<Profile>,
{
    // Lease + heartbeat events are not manifest-scoped — they're
    // gated by the chain's lease invariants instead. Pass through.
    let manifest_id = match body {
        EventBody::ClaimLease { .. }
        | EventBody::ReleaseLease { .. }
        | EventBody::Heartbeat { .. } => return Ok(()),
        EventBody::Assign      { manifest_id, .. } => manifest_id,
        EventBody::Unassign    { manifest_id }     => manifest_id,
        EventBody::Balloon     { manifest_id, .. } => manifest_id,
    };

    // For Assign, the manifest may not exist yet — it's in this very
    // event. Try to use the embedded TOML if present; only Assign
    // has it inline. For the others, we need the resolver to know.
    let profile = match body {
        EventBody::Assign { manifest_toml, .. } => {
            // Don't fail on parse errors here — the wanted-state
            // apply path already enforces that. If parse fails, we
            // permit (the apply itself will reject).
            Profile::from_str(manifest_toml).ok()
        }
        _ => lookup_profile(manifest_id),
    };

    let Some(profile) = profile else {
        // No policy to consult and nothing inline. Refuse —
        // safer than implicitly permitting on missing data.
        return Err(RefusalReason::UnknownManifest {
            manifest_id: manifest_id.clone(),
        });
    };

    // (1) controller allowlist
    let lp = &profile.lease_policy;
    if !lp.controllers_allowed.is_empty()
        && !lp.controllers_allowed.iter().any(|c|
            c == issuer.as_str() || c == issuer.hex_suffix())
    {
        return Err(RefusalReason::ControllerNotAllowed {
            issuer: issuer.clone(),
            manifest_id: manifest_id.clone(),
        });
    }

    // (2) operation allowlist
    let op = op_name(body);
    let allowed = if lp.operations_allowed.is_empty() {
        DEFAULT_SAFE_OPS.iter().any(|s| *s == op)
    } else {
        lp.operations_allowed.iter().any(|s| s == op)
    };
    if !allowed {
        return Err(RefusalReason::OperationNotAllowed {
            op: op.into(),
            manifest_id: manifest_id.clone(),
        });
    }

    Ok(())
}

/// Stable string label per EventBody variant — what users put in
/// `lease_policy.operations_allowed`. Pin: must match the documented
/// list in [`DEFAULT_SAFE_OPS`] + the manifest schema docs.
fn op_name(body: &EventBody) -> &'static str {
    match body {
        EventBody::ClaimLease   { .. } => "claim_lease",
        EventBody::ReleaseLease { .. } => "release_lease",
        EventBody::Assign       { .. } => "assign",
        EventBody::Unassign     { .. } => "unassign",
        EventBody::Balloon      { .. } => "balloon",
        EventBody::Heartbeat    { .. } => "heartbeat",
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> NodeId { NodeId::new("ctrl-deadbeefcafebabe") }
    fn other_ctrl() -> NodeId { NodeId::new("rogue-aaaaaaaaaaaaaaaa") }

    fn profile(controllers: &[&str], ops: &[&str]) -> Profile {
        let ctrl_list = controllers.iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>().join(", ");
        let op_list = ops.iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>().join(", ");
        let toml = format!(r#"
[profile]
name = "x"
type = "appliance"
[resources]
memory_active = "1G"
memory_idle = "256M"
memory_minimum = "128M"
vcpus_active = 1
vcpus_idle = 1
vcpus_minimum = 1
[storage]
root = {{ kind = "qcow2", path = "/tmp/x.qcow2" }}
[lease_policy]
controllers_allowed = [{ctrl_list}]
operations_allowed = [{op_list}]
"#);
        Profile::from_str(&toml).expect("test profile must parse")
    }

    #[test]
    fn lease_events_pass_through_without_manifest_lookup() {
        // ClaimLease / ReleaseLease / Heartbeat are not manifest-
        // scoped — they MUST not invoke the resolver. Pin: passing
        // a panicking resolver is fine.
        let body = EventBody::ClaimLease {
            target_node: NodeId::new("agent-1111111111111111"),
        };
        may_apply(&ctrl(), &body, |_| panic!("must not be called")).unwrap();

        let body = EventBody::Heartbeat {
            free_memory_mb: 0, free_vcpus: 0,
            running: vec![], last_applied_hash: None,
        };
        may_apply(&ctrl(), &body, |_| panic!("must not be called")).unwrap();
    }

    #[test]
    fn empty_controller_allowlist_permits_anyone() {
        // No allowlist = any lease holder may issue. The chain still
        // enforces lease holdership; we don't double-enforce here.
        let p = profile(&[], &["balloon"]);
        let body = EventBody::Balloon { manifest_id: "x".into(), target_mb: 1024 };
        may_apply(&other_ctrl(), &body, |_| Some(p.clone())).unwrap();
    }

    #[test]
    fn explicit_controller_allowlist_rejects_others() {
        // ctrl-deadbeefcafebabe is allowed; rogue-aaaa is not.
        let p = profile(&["ctrl-deadbeefcafebabe"], &["balloon"]);
        let body = EventBody::Balloon { manifest_id: "x".into(), target_mb: 1024 };
        let err = may_apply(&other_ctrl(), &body, |_| Some(p.clone())).unwrap_err();
        assert!(matches!(err, RefusalReason::ControllerNotAllowed { .. }));
    }

    #[test]
    fn allowlist_accepts_hex_only_form() {
        // Operators may list controllers by hex suffix (preferred —
        // role hint can change). Pin: matching by hex_suffix works.
        let p = profile(&["deadbeefcafebabe"], &["balloon"]);
        let body = EventBody::Balloon { manifest_id: "x".into(), target_mb: 1024 };
        may_apply(&ctrl(), &body, |_| Some(p.clone())).unwrap();
    }

    #[test]
    fn empty_op_list_uses_default_safe_set() {
        // Default safe set: assign, unassign, balloon, start, shutdown.
        // destroy is NOT in the default — must be opted in.
        let p = profile(&[], &[]);
        // balloon: in default safe set — allowed.
        may_apply(&ctrl(), &EventBody::Balloon {
            manifest_id: "x".into(), target_mb: 1024,
        }, |_| Some(p.clone())).unwrap();
    }

    #[test]
    fn explicit_op_list_excludes_balloon_when_not_listed() {
        // Operator wrote `operations_allowed = ["assign"]`. Balloon
        // is missing — must be refused even though it's normally safe.
        let p = profile(&[], &["assign"]);
        let body = EventBody::Balloon { manifest_id: "x".into(), target_mb: 1024 };
        let err = may_apply(&ctrl(), &body, |_| Some(p.clone())).unwrap_err();
        assert!(matches!(err, RefusalReason::OperationNotAllowed { op, .. } if op == "balloon"));
    }

    #[test]
    fn unknown_manifest_is_refused_not_silently_permitted() {
        // Resolver returns None for the manifest; we can't check
        // policy. Pin: refuse rather than apply blindly.
        let body = EventBody::Balloon { manifest_id: "ghost".into(), target_mb: 1024 };
        let err = may_apply(&ctrl(), &body, |_| None).unwrap_err();
        assert!(matches!(err, RefusalReason::UnknownManifest { .. }));
    }

    #[test]
    fn assign_uses_inline_manifest_toml_for_policy() {
        // Assign carries the manifest inline — refusal layer must
        // parse it directly, not call the resolver. This is what
        // makes "first-time assignment" work.
        let toml = r#"
[profile]
name = "fresh"
type = "appliance"
[resources]
memory_active = "1G"
memory_idle = "256M"
memory_minimum = "128M"
vcpus_active = 1
vcpus_idle = 1
vcpus_minimum = 1
[storage]
root = { kind = "qcow2", path = "/tmp/fresh.qcow2" }
[lease_policy]
controllers_allowed = ["other-aaaaaaaaaaaaaaaa"]
"#;
        let body = EventBody::Assign {
            manifest_id: "fresh".into(),
            manifest_toml: toml.into(),
        };
        // ctrl is NOT in the inline manifest's allowlist — must refuse.
        let err = may_apply(&ctrl(), &body, |_| panic!("must not call resolver"))
            .unwrap_err();
        assert!(matches!(err, RefusalReason::ControllerNotAllowed { .. }));
    }
}
