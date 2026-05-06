//! CPU topology probe.
//!
//! Distinguishes Intel hybrid (P-cores vs E-cores, exposed under
//! `/sys/devices/cpu_core/cpus` and `/sys/devices/cpu_atom/cpus`) from
//! uniform CPUs. The planner uses this to decide which logical CPUs
//! dom0 pins to and which Ubuntu gets.
//!
//! Format of `/sys/devices/cpu_*/cpus` and `/sys/devices/system/cpu/online`
//! is the kernel "cpu list" format: `0-3,8-11`. We expand it to a flat
//! list of logical CPU IDs.

use std::fs;

#[derive(Debug, Clone, Default)]
pub struct CpuTopology {
    pub logical_cpus: u32,
    /// Logical CPU IDs assigned to performance cores. Empty on
    /// non-hybrid CPUs (AMD, pre-12th-gen Intel).
    pub p_cores: Vec<u32>,
    /// Logical CPU IDs assigned to efficiency cores. Empty on
    /// non-hybrid CPUs.
    pub e_cores: Vec<u32>,
    /// All online logical CPU IDs (sorted).
    pub all_cpus: Vec<u32>,
    pub is_hybrid: bool,
}

impl CpuTopology {
    pub fn probe() -> Self {
        let p_cores = read_cpulist_file("/sys/devices/cpu_core/cpus");
        let e_cores = read_cpulist_file("/sys/devices/cpu_atom/cpus");
        let is_hybrid = !p_cores.is_empty() && !e_cores.is_empty();
        let all_cpus = {
            let mut v = read_cpulist_file("/sys/devices/system/cpu/online");
            if v.is_empty() {
                // Fallback: probe by stat'ing /sys/devices/system/cpu/cpu*
                v = (0..std::thread::available_parallelism()
                            .map(|n| n.get() as u32).unwrap_or(1)).collect();
            }
            v
        };
        Self {
            logical_cpus: all_cpus.len() as u32,
            p_cores, e_cores, all_cpus, is_hybrid,
        }
    }
}

fn read_cpulist_file(path: &str) -> Vec<u32> {
    let s = fs::read_to_string(path).unwrap_or_default();
    parse_cpulist(s.trim())
}

/// Parses kernel cpu-list format (`"0-3,8-11"`) into a flat sorted list
/// of CPU IDs. Robust to whitespace, blank input, and malformed chunks
/// (skips them).
pub fn parse_cpulist(s: &str) -> Vec<u32> {
    let mut out = vec![];
    for chunk in s.split(',') {
        let chunk = chunk.trim();
        if chunk.is_empty() { continue }
        if let Some((a, b)) = chunk.split_once('-') {
            let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) else { continue };
            if a <= b {
                for v in a..=b { out.push(v); }
            }
        } else if let Ok(v) = chunk.parse::<u32>() {
            out.push(v);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_probe_returns_at_least_one_cpu() {
        let t = CpuTopology::probe();
        assert!(t.logical_cpus >= 1);
        assert!(!t.all_cpus.is_empty());
    }

    #[test]
    fn parse_cpulist_handles_single() {
        assert_eq!(parse_cpulist("3"), vec![3]);
    }

    #[test]
    fn parse_cpulist_handles_range() {
        assert_eq!(parse_cpulist("0-3"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn parse_cpulist_handles_mixed() {
        assert_eq!(parse_cpulist("0-3,8-11,15"),
                   vec![0, 1, 2, 3, 8, 9, 10, 11, 15]);
    }

    #[test]
    fn parse_cpulist_skips_garbage() {
        // "x" is malformed; "5-3" is reversed; both skipped silently.
        assert_eq!(parse_cpulist("0,x,5-3,7"), vec![0, 7]);
    }

    #[test]
    fn parse_cpulist_handles_empty() {
        assert_eq!(parse_cpulist(""), Vec::<u32>::new());
        assert_eq!(parse_cpulist("  "), Vec::<u32>::new());
    }
}
