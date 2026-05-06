//! Lift planner — pure functions that turn host facts into a proposed
//! dom0/domU split.
//!
//! Why this exists: rotten-apple is OSS and runs on arbitrary x86_64
//! hardware (4-core mini PCs, 64-core workstations, hybrid laptops,
//! uniform servers). Hardcoding "dom0 = 1.5 GB / 2 E-cores" only works
//! on the developer's exact machine. Instead, the bootstrapper feeds
//! [`Detection`] + [`CpuTopology`] into [`plan`] and gets back a
//! [`LiftPlan`] sized to *this* host.
//!
//! Rules (deliberately simple, easy to override):
//!   - dom0 RAM = `clamp(round64(total * 3%), 768 MB, 4096 MB)`
//!     - dom0 is a broker, not a workload; it doesn't need much
//!     - 64 MB rounding keeps numbers nice in `xl info`
//!     - floor 768 MB so libxl + xenstored + orchestrator aren't tight
//!     - ceiling 4096 MB so a 256 GB workstation doesn't waste 8 GB
//!   - dom0 vCPUs:
//!     - hybrid CPU: dom0 takes E-cores (capped at 4), domU takes P-cores
//!     - uniform CPU: dom0 takes `clamp(total / 8, 1, 4)` lowest-indexed
//!       cores, domU takes the rest
//!   - domU RAM: `total - dom0 - 256 MB safety reserve`
//!   - domU idle (ballooned-down): `active / 2`
//!   - domU minimum (refuse-to-shrink-below): `max(active / 8, 2048)`
//!
//! All numbers are recommendations. The user can override in the manifest.

use crate::{Detection, topology::CpuTopology};

#[derive(Debug, Clone)]
pub struct Dom0Plan {
    pub vcpus: u32,
    pub cpu_pin: Vec<u32>,
    pub memory_mb: u64,
    pub rationale: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DomUPlan {
    pub vcpus: u32,
    pub cpu_pin: Vec<u32>,
    pub memory_active_mb: u64,
    pub memory_idle_mb: u64,
    pub memory_minimum_mb: u64,
    pub rationale: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LiftPlan {
    pub dom0: Dom0Plan,
    pub ubuntu_domu: DomUPlan,
}

const DOM0_MIN_MB: u64 = 768;
const DOM0_MAX_MB: u64 = 4096;
const DOM0_FRACTION_PCT: u64 = 3;
const DOM0_VCPU_CAP: u32 = 4;
const DOMU_SAFETY_RESERVE_MB: u64 = 256;
const DOMU_MIN_FLOOR_MB: u64 = 2048;

pub fn plan(detection: &Detection, topo: &CpuTopology) -> LiftPlan {
    let total_mb = detection.mem_total_kb / 1024;
    let dom0_mb = clamp_dom0_mb(total_mb);

    let (dom0_pin, domu_pin, cpu_note) = split_cpus(topo);

    let dom0 = Dom0Plan {
        vcpus: dom0_pin.len() as u32,
        cpu_pin: dom0_pin.clone(),
        memory_mb: dom0_mb,
        rationale: vec![
            format!("dom0 RAM = clamp(round64({} MB * {}%), {}, {}) = {} MB",
                    total_mb, DOM0_FRACTION_PCT, DOM0_MIN_MB, DOM0_MAX_MB, dom0_mb),
            cpu_note.clone(),
        ],
    };

    let domu_active = total_mb
        .saturating_sub(dom0_mb)
        .saturating_sub(DOMU_SAFETY_RESERVE_MB);
    let domu_idle = (domu_active / 2).max(DOMU_MIN_FLOOR_MB).min(domu_active);
    let domu_min  = (domu_active / 8).max(DOMU_MIN_FLOOR_MB).min(domu_active);

    let ubuntu_domu = DomUPlan {
        vcpus: domu_pin.len() as u32,
        cpu_pin: domu_pin,
        memory_active_mb: domu_active,
        memory_idle_mb: domu_idle,
        memory_minimum_mb: domu_min,
        rationale: vec![
            format!("active = total({}) - dom0({}) - safety({}) = {} MB",
                    total_mb, dom0_mb, DOMU_SAFETY_RESERVE_MB, domu_active),
            format!("idle (orchestrator balloons down when desktop unfocused) = \
                     max(active/2, {}) = {} MB", DOMU_MIN_FLOOR_MB, domu_idle),
            format!("minimum (refuse to shrink below) = max(active/8, {}) = {} MB",
                    DOMU_MIN_FLOOR_MB, domu_min),
        ],
    };

    LiftPlan { dom0, ubuntu_domu }
}

fn clamp_dom0_mb(total_mb: u64) -> u64 {
    let raw = total_mb * DOM0_FRACTION_PCT / 100;
    let rounded = (raw / 64) * 64;
    rounded.clamp(DOM0_MIN_MB, DOM0_MAX_MB)
}

/// Returns `(dom0_pin, domu_pin, rationale_string)`.
fn split_cpus(topo: &CpuTopology) -> (Vec<u32>, Vec<u32>, String) {
    if topo.is_hybrid {
        let cap = (topo.e_cores.len() as u32).min(DOM0_VCPU_CAP) as usize;
        let dom0: Vec<u32> = topo.e_cores.iter().take(cap).copied().collect();
        let domu: Vec<u32> = topo.p_cores.clone();
        let note = format!(
            "hybrid CPU: dom0 on first {} E-core(s) {:?}, domU on all P-cores {:?}",
            cap, dom0, domu);
        (dom0, domu, note)
    } else {
        let n = (topo.logical_cpus / 8).clamp(1, DOM0_VCPU_CAP) as usize;
        let n = n.min(topo.all_cpus.len().saturating_sub(1).max(1));
        let dom0: Vec<u32> = topo.all_cpus.iter().take(n).copied().collect();
        let domu: Vec<u32> = topo.all_cpus.iter().skip(n).copied().collect();
        let note = format!(
            "uniform CPU: dom0 on {} lowest-indexed core(s) {:?}, domU on rest {:?}",
            n, dom0, domu);
        (dom0, domu, note)
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecureBoot;

    fn det(total_kb: u64) -> Detection {
        Detection {
            distro_id: "ubuntu".into(),
            distro_version: "25.10".into(),
            kernel: "test".into(),
            arch: "x86_64".into(),
            is_uefi: true,
            secure_boot: SecureBoot::Disabled,
            cpu_count: 0,
            mem_total_kb: total_kb,
            has_intel_iommu: true,
            has_amd_iommu: false,
            iommu_in_cmdline: true,
            nvidia_proprietary: false,
            xen_already_installed: false,
            running_under_xen: false,
            grub_default_raw: None,
            boot_free_mb: 1024,
            root_free_mb: 100_000,
            initramfs_path: None,
            initramfs_size_mb: 0,
            warnings: vec![],
            blockers: vec![],
        }
    }

    fn topo_hybrid(p: Vec<u32>, e: Vec<u32>) -> CpuTopology {
        let mut all = p.clone();
        all.extend(e.iter().copied());
        all.sort();
        CpuTopology {
            logical_cpus: all.len() as u32,
            p_cores: p,
            e_cores: e,
            all_cpus: all,
            is_hybrid: true,
        }
    }

    fn topo_uniform(n: u32) -> CpuTopology {
        let all: Vec<u32> = (0..n).collect();
        CpuTopology {
            logical_cpus: n,
            p_cores: vec![],
            e_cores: vec![],
            all_cpus: all,
            is_hybrid: false,
        }
    }

    #[test]
    fn plan_alder_lake_64gb_matches_design_intent() {
        // 12900H: 6P (12 SMT) + 8E = 20 logical; 64 GB
        let t = topo_hybrid(
            (0..12).collect(),    // 12 P-vCPUs (6 cores * 2 SMT)
            (12..20).collect(),   // 8 E-cores
        );
        let p = plan(&det(64 * 1024 * 1024), &t);
        // 64 GB * 3% = 1966 MB → round to 1920, clamped within [768, 4096]
        assert_eq!(p.dom0.memory_mb, 1920);
        // dom0 takes first 4 E-cores (cap = DOM0_VCPU_CAP)
        assert_eq!(p.dom0.cpu_pin, vec![12, 13, 14, 15]);
        assert_eq!(p.dom0.vcpus, 4);
        // domU gets all 12 P-vCPUs
        assert_eq!(p.ubuntu_domu.vcpus, 12);
        assert_eq!(p.ubuntu_domu.cpu_pin, (0..12).collect::<Vec<_>>());
        // domU active = 65536 - 1920 - 256 = 63360
        assert_eq!(p.ubuntu_domu.memory_active_mb, 63360);
    }

    #[test]
    fn plan_small_uniform_8gb_4cores() {
        // 4-core mini PC, 8 GB
        let t = topo_uniform(4);
        let p = plan(&det(8 * 1024 * 1024), &t);
        // 8 GB * 3% = 245 MB → clamped up to floor 768
        assert_eq!(p.dom0.memory_mb, 768);
        // total/8 = 0 → max 1
        assert_eq!(p.dom0.cpu_pin, vec![0]);
        assert_eq!(p.dom0.vcpus, 1);
        assert_eq!(p.ubuntu_domu.cpu_pin, vec![1, 2, 3]);
        // active = 8192 - 768 - 256 = 7168
        assert_eq!(p.ubuntu_domu.memory_active_mb, 7168);
    }

    #[test]
    fn plan_large_uniform_64core_caps_dom0_vcpus() {
        // 64-core uniform server
        let t = topo_uniform(64);
        let p = plan(&det(128 * 1024 * 1024), &t);
        // 128 GB * 3% = 3932 MB → round to 3904, within bounds
        assert_eq!(p.dom0.memory_mb, 3904);
        // 64 / 8 = 8, capped at 4
        assert_eq!(p.dom0.vcpus, 4);
        assert_eq!(p.dom0.cpu_pin, vec![0, 1, 2, 3]);
        assert_eq!(p.ubuntu_domu.vcpus, 60);
    }

    #[test]
    fn plan_huge_ram_caps_dom0_at_4gb() {
        // 256 GB workstation
        let t = topo_uniform(16);
        let p = plan(&det(256 * 1024 * 1024), &t);
        assert_eq!(p.dom0.memory_mb, DOM0_MAX_MB);
    }

    #[test]
    fn plan_floors_domu_minimum_at_2gb() {
        let t = topo_uniform(4);
        let p = plan(&det(8 * 1024 * 1024), &t);
        assert!(p.ubuntu_domu.memory_minimum_mb >= DOMU_MIN_FLOOR_MB);
    }

    #[test]
    fn rationale_is_populated() {
        let t = topo_hybrid(vec![0, 1], vec![2, 3]);
        let p = plan(&det(16 * 1024 * 1024), &t);
        assert!(!p.dom0.rationale.is_empty());
        assert!(!p.ubuntu_domu.rationale.is_empty());
        assert!(p.dom0.rationale.iter().any(|l| l.contains("hybrid")));
    }
}
