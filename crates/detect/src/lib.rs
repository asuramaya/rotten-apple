//! Host inspection.
//!
//! Reads the things the bootstrapper and orchestrator need to know:
//! distro, firmware, Secure Boot, CPU/RAM, IOMMU state, GPU drivers,
//! Xen package presence, GRUB default, /boot free space, initramfs.
//!
//! Returns a [`Detection`] with both raw fields and a `warnings` /
//! `blockers` list. The bootstrapper refuses to lift if `blockers` is
//! non-empty; otherwise it shows warnings and asks for confirmation.

use std::fs;
use std::path::Path;
use std::process::Command;

pub mod gpu;
pub mod host_specs;
pub mod lift_readiness;
pub mod planner;
pub mod topology;
pub use gpu::{GpuDevice, GpuRole, GpuVendor, LockReason, enumerate_gpus};
pub use host_specs::{HostSpecs, MemoryModule, GpuSpec};
pub use lift_readiness::{IommuGroup, LiftReadiness, PciDevice};
pub use planner::{Dom0Plan, DomUPlan, LiftPlan, plan};
pub use topology::CpuTopology;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecureBoot {
    Enabled,
    Disabled,
    Unknown,
}

impl std::fmt::Display for SecureBoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SecureBoot::Enabled  => "enabled",
            SecureBoot::Disabled => "disabled",
            SecureBoot::Unknown  => "unknown",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub distro_id: String,
    pub distro_version: String,
    pub kernel: String,
    pub arch: String,
    pub is_uefi: bool,
    pub secure_boot: SecureBoot,
    pub cpu_count: u32,
    pub mem_total_kb: u64,
    pub has_intel_iommu: bool,
    pub has_amd_iommu: bool,
    pub iommu_in_cmdline: bool,
    pub nvidia_proprietary: bool,
    pub xen_already_installed: bool,
    pub running_under_xen: bool,
    pub grub_default_raw: Option<String>,
    pub boot_free_mb: u64,
    pub root_free_mb: u64,
    pub initramfs_path: Option<String>,
    pub initramfs_size_mb: u64,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

impl Detection {
    /// Run all probes and produce a populated `Detection`. Pure-IO; safe
    /// to call from anywhere as a non-root user (some fields will simply
    /// be unknown/zero without privileges).
    pub fn run() -> Detection {
        let (distro_id, distro_version) = read_os_release();
        let (kernel, arch) = read_uname();
        let is_uefi = Path::new("/sys/firmware/efi").exists();
        let secure_boot = read_secure_boot();
        let cpu_count = num_cpus();
        let mem_total_kb = read_meminfo_total();
        let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
        let has_intel_iommu = read_cpuinfo_contains("GenuineIntel");
        let has_amd_iommu = read_cpuinfo_contains("AuthenticAMD");
        let iommu_in_cmdline = cmdline.contains("intel_iommu=on")
            || cmdline.contains("amd_iommu=on")
            || cmdline.contains("iommu=pt");
        let nvidia_proprietary = lsmod_has_nvidia();
        let xen_already_installed = dpkg_has_xen_hypervisor();
        let running_under_xen = Path::new("/proc/xen").is_dir() || cmdline.contains("xen");
        let grub_default_raw = read_grub_default();
        let boot_free_mb = df_free_mb("/boot");
        let root_free_mb = df_free_mb("/");
        let (initramfs_path, initramfs_size_mb) = find_initramfs(&kernel);

        let mut d = Detection {
            distro_id, distro_version, kernel, arch,
            is_uefi, secure_boot, cpu_count, mem_total_kb,
            has_intel_iommu, has_amd_iommu, iommu_in_cmdline,
            nvidia_proprietary, xen_already_installed, running_under_xen,
            grub_default_raw, boot_free_mb, root_free_mb,
            initramfs_path, initramfs_size_mb,
            warnings: vec![], blockers: vec![],
        };
        d.classify();
        d
    }

    /// Populate `warnings` and `blockers` from the raw fields. Pure
    /// function over `&mut self` — easy to test independently.
    fn classify(&mut self) {
        if self.arch != "x86_64" {
            self.blockers.push(format!(
                "unsupported architecture: {} (need x86_64)", self.arch));
        }
        if !matches!(self.distro_id.as_str(), "ubuntu" | "debian") {
            self.warnings.push(format!(
                "untested distro: {} {}", self.distro_id, self.distro_version));
        }
        if !self.is_uefi {
            self.warnings.push(
                "BIOS/legacy boot detected; lift only tested on UEFI".into());
        }
        if matches!(self.secure_boot, SecureBoot::Enabled) {
            self.warnings.push(
                "Secure Boot is enabled; Xen on Ubuntu is signed but MOK \
                 enrollment may be required on first boot".into());
        }
        if self.mem_total_kb > 0 && self.mem_total_kb < 16 * 1024 * 1024 {
            self.warnings.push(format!(
                "low RAM ({} MB); guests will be tight", self.mem_total_kb / 1024));
        }
        if self.nvidia_proprietary {
            self.warnings.push(
                "NVIDIA proprietary driver loaded; dom0 + nvidia.ko can have \
                 display issues — keep the bare-metal GRUB entry visible during \
                 first lift".into());
        }
        if self.xen_already_installed && !self.running_under_xen {
            self.warnings.push(
                "Xen hypervisor packages already installed but not booted via \
                 Xen; lift will reuse them".into());
        }
        if self.running_under_xen {
            self.blockers.push(
                "system is already running under Xen; nothing to lift".into());
        }
        // /boot space — Xen install + initramfs regen needs ~80 MB worst case.
        if self.boot_free_mb < 128 {
            if self.boot_free_mb < 50 {
                self.blockers.push(format!(
                    "/boot has only {} MB free; need ≥128 MB. Free space first \
                     (e.g. apt autoremove old kernels).", self.boot_free_mb));
            } else {
                self.warnings.push(format!(
                    "/boot is tight ({} MB free); recommend ≥128 MB.",
                    self.boot_free_mb));
            }
        }
        if self.root_free_mb < 500 {
            self.warnings.push(format!(
                "/ has only {} MB free; apt may struggle. Recommend ≥1 GB.",
                self.root_free_mb));
        }
        if self.initramfs_path.is_none() {
            self.warnings.push(format!(
                "no initramfs found for kernel {}; backup step will be skipped \
                 (recovery from a broken initramfs would require USB live-media)",
                self.kernel));
        }
    }
}

// ---------------------------------------------------------------------------
// Probes (each one isolated; absence of a tool/file is non-fatal)

fn read_os_release() -> (String, String) {
    let text = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut id = "unknown".to_string();
    let mut ver = "unknown".to_string();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            ver = v.trim_matches('"').to_string();
        }
    }
    (id, ver)
}

fn read_uname() -> (String, String) {
    let kernel = run_capture("uname", &["-r"]).unwrap_or_default();
    let arch   = run_capture("uname", &["-m"]).unwrap_or_default();
    (kernel.trim().to_string(), arch.trim().to_string())
}

fn read_secure_boot() -> SecureBoot {
    let Some(out) = run_both_streams("mokutil", &["--sb-state"]) else {
        return SecureBoot::Unknown;
    };
    if out.contains("SecureBoot enabled")  { SecureBoot::Enabled }
    else if out.contains("SecureBoot disabled") { SecureBoot::Disabled }
    else { SecureBoot::Unknown }
}

fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

fn read_meminfo_total() -> u64 {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            return rest.split_whitespace().next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

fn read_cpuinfo_contains(needle: &str) -> bool {
    fs::read_to_string("/proc/cpuinfo")
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

fn lsmod_has_nvidia() -> bool {
    let Some(out) = run_capture("lsmod", &[]) else { return false };
    out.lines().any(|line| {
        let mod_name = line.split_whitespace().next().unwrap_or("");
        mod_name == "nvidia" || mod_name.starts_with("nvidia_")
    })
}

fn dpkg_has_xen_hypervisor() -> bool {
    let Some(out) = run_capture("dpkg", &["-l"]) else { return false };
    // Match xen-system-* or xen-hypervisor-* (NOT grub-xen-* or libxen*).
    out.lines().any(|line| {
        if !line.starts_with("ii") { return false; }
        let pkg = line.split_whitespace().nth(1).unwrap_or("");
        pkg.starts_with("xen-system-") || pkg.starts_with("xen-hypervisor-")
    })
}

fn read_grub_default() -> Option<String> {
    let text = fs::read_to_string("/etc/default/grub").ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("GRUB_DEFAULT=") {
            return Some(v.to_string());
        }
    }
    None
}

fn df_free_mb(path: &str) -> u64 {
    // `df --output=avail -BM <path>` → "  Avail\n   250M\n"
    let Some(out) = run_capture("df", &["--output=avail", "-BM", path]) else {
        return 0;
    };
    out.lines().nth(1)
        .map(|l| l.trim().trim_end_matches('M').parse().unwrap_or(0))
        .unwrap_or(0)
}

fn find_initramfs(kernel: &str) -> (Option<String>, u64) {
    let candidate = format!("/boot/initrd.img-{kernel}");
    let p = Path::new(&candidate);
    if p.is_file() {
        let size_mb = fs::metadata(p).map(|m| m.len() / (1024 * 1024)).unwrap_or(0);
        (Some(candidate), size_mb)
    } else {
        (None, 0)
    }
}

// --- subprocess helpers ---

fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Some tools print to stderr (mokutil); merge both streams.
fn run_both_streams(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_runs_on_host_without_panicking() {
        // Smoke test: must not panic on the host this runs on.
        let d = Detection::run();
        assert!(!d.kernel.is_empty(), "kernel string should be populated");
        assert!(!d.arch.is_empty(), "arch should be populated");
        assert!(d.cpu_count >= 1, "at least one CPU");
    }

    #[test]
    fn classify_reports_blocker_for_non_x86() {
        let mut d = make_baseline();
        d.arch = "aarch64".into();
        d.classify();
        assert!(d.blockers.iter().any(|b| b.contains("unsupported architecture")),
                "blockers: {:?}", d.blockers);
    }

    #[test]
    fn classify_blocks_when_running_under_xen() {
        let mut d = make_baseline();
        d.running_under_xen = true;
        d.classify();
        assert!(d.blockers.iter().any(|b| b.contains("already running under Xen")));
    }

    #[test]
    fn classify_blocks_on_tiny_boot_partition() {
        let mut d = make_baseline();
        d.boot_free_mb = 30;
        d.classify();
        assert!(d.blockers.iter().any(|b| b.contains("/boot has only")));
    }

    #[test]
    fn classify_warns_on_tight_boot_partition() {
        let mut d = make_baseline();
        d.boot_free_mb = 80; // between 50 and 128
        d.classify();
        assert!(d.warnings.iter().any(|w| w.contains("/boot is tight")));
        assert!(d.blockers.is_empty());
    }

    fn make_baseline() -> Detection {
        Detection {
            distro_id: "ubuntu".into(),
            distro_version: "25.10".into(),
            kernel: "6.17.0-22-generic".into(),
            arch: "x86_64".into(),
            is_uefi: true,
            secure_boot: SecureBoot::Disabled,
            cpu_count: 8,
            mem_total_kb: 64 * 1024 * 1024,
            has_intel_iommu: true,
            has_amd_iommu: false,
            iommu_in_cmdline: true,
            nvidia_proprietary: false,
            xen_already_installed: false,
            running_under_xen: false,
            grub_default_raw: Some("0".into()),
            boot_free_mb: 1500,
            root_free_mb: 1_000_000,
            initramfs_path: Some("/boot/initrd.img-6.17.0-22-generic".into()),
            initramfs_size_mb: 41,
            warnings: vec![],
            blockers: vec![],
        }
    }
}
