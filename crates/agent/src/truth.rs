//! Observed-state ("truth") snapshot.
//!
//! What the agent currently sees in libxl. Decoupled from any
//! specific backend so the reconcile loop is testable without Xen
//! running. The orchestratord's libxl actor populates this; the
//! agent's reconcile compares it against [`WantedState`] to produce
//! a [`ReconcilePlan`].

use std::collections::BTreeMap;

/// Coarse domain state the reconciler cares about. Backend-specific
/// states (paused, dying, etc.) collapse into one of these for the
/// reconcile decision; the actuator can re-fetch finer-grained state
/// when needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainState {
    /// Domain exists in libxl AND is consuming CPU (running or
    /// blocked-on-IO; we treat both as "live").
    Running,
    /// Domain exists but is paused, dying, or in transition. Agent
    /// generally leaves these alone for one cycle and re-evaluates.
    Transient,
    /// Backend has no record of this domain.
    Absent,
}

/// One observed domain. The `manifest_id` is what links it to a
/// `WantedEntry` — populated from the domain's xenstore key
/// `vm/{uuid}/rotten-apple/manifest_id` (or equivalent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruthEntry {
    /// Backend's domain id (libxl: u32 cast as string for portability).
    pub domain_id: String,
    /// Manifest id that produced this domain. None = orphan (manually
    /// created via xl outside rotten-apple, OR rotten-apple-created
    /// before xenstore tagging shipped). Reconciler leaves orphans
    /// alone — destroying them would race against operator intent.
    pub manifest_id: Option<String>,
    /// Coarse state.
    pub state: DomainState,
    /// Current memory (MB). For balloon-target diff against wanted.
    pub memory_mb: u64,
    /// Current vCPU count.
    pub vcpus: u32,
}

/// All observations from one libxl poll. Sorted by domain_id for
/// deterministic iteration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Truth {
    pub domains: BTreeMap<String, TruthEntry>,
}

impl Truth {
    pub fn new() -> Self { Self::default() }

    pub fn insert(&mut self, e: TruthEntry) {
        self.domains.insert(e.domain_id.clone(), e);
    }

    /// Look up by manifest_id. None if no domain reports that id.
    pub fn find_by_manifest(&self, manifest_id: &str) -> Option<&TruthEntry> {
        self.domains.values().find(|d|
            d.manifest_id.as_deref() == Some(manifest_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_by_manifest_returns_first_match() {
        let mut t = Truth::new();
        t.insert(TruthEntry {
            domain_id: "1".into(),
            manifest_id: Some("ci".into()),
            state: DomainState::Running,
            memory_mb: 1024, vcpus: 2,
        });
        t.insert(TruthEntry {
            domain_id: "2".into(),
            manifest_id: None, // orphan
            state: DomainState::Running,
            memory_mb: 2048, vcpus: 4,
        });
        let hit = t.find_by_manifest("ci").unwrap();
        assert_eq!(hit.domain_id, "1");
        assert_eq!(t.find_by_manifest("ghost"), None);
    }
}
