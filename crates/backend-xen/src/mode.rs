//! Xen domain-mode selection.
//!
//! The hypervisor-agnostic [`Profile`] never says "PV" or "PVH" or "HVM"
//! — those are Xen-specific implementation details. The backend decides
//! at `create_guest` time using whatever signals the profile + the host
//! provide. This is orchestration, not configuration.
//!
//! Rules (in priority order):
//!   1. TPM is requested (swtpm or hardware passthrough) → **HVM**
//!      PVH does not expose a vTPM device model. Windows guests need
//!      this; Linux guests asking for swtpm also need it.
//!   2. Root disk looks like a Linux installation (`/boot` partition
//!      contains `vmlinuz-*`) → **PVH**
//!      Modern paravirt Linux. No qemu-dm process in dom0.
//!   3. Otherwise → **HVM**
//!      Safe default. Works for any guest OS, costs ~80 MB extra in dom0
//!      for the per-guest qemu device model.
//!
//! PV is not selected automatically — Xen 4.x supports it but PVH is the
//! recommended path for Linux. A user who really wants PV can hand-craft
//! the config (not yet wired).

use rotten_apple_manifest::{Profile, TpmMode};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XenDomainMode {
    Pv,
    Pvh,
    Hvm,
}

impl std::fmt::Display for XenDomainMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            XenDomainMode::Pv  => "pv",
            XenDomainMode::Pvh => "pvh",
            XenDomainMode::Hvm => "hvm",
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModeSelection {
    pub mode: XenDomainMode,
    pub rationale: String,
}

/// Probe seam — production uses [`LsblkProbe`]; tests inject fakes.
pub trait DiskProbe {
    /// Best-effort: does the device at `source` carry a Linux installation?
    fn looks_linux(&self, source: &str) -> bool;
}

pub struct LsblkProbe;

impl DiskProbe for LsblkProbe {
    fn looks_linux(&self, source: &str) -> bool {
        // Strip libxl-style prefixes ("phy:", "file:") to get the bare path.
        let path = source.strip_prefix("phy:")
            .or_else(|| source.strip_prefix("file:"))
            .unwrap_or(source);
        // Only probe real block devices; refuse to touch arbitrary file paths.
        if !Path::new(path).exists() { return false }
        // Look at the FS type of every partition under this device. If any
        // is ext{2,3,4} it's almost certainly a Linux disk; the orchestrator
        // doesn't need to be more precise than that to pick PVH over HVM.
        let Ok(out) = Command::new("lsblk").args(["-no", "FSTYPE", path]).output()
        else { return false };
        if !out.status.success() { return false }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| matches!(l.trim(), "ext2" | "ext3" | "ext4" | "btrfs" | "xfs"))
    }
}

pub fn select_mode(profile: &Profile, probe: &dyn DiskProbe) -> ModeSelection {
    if !matches!(profile.tpm.mode, TpmMode::None) {
        return ModeSelection {
            mode: XenDomainMode::Hvm,
            rationale: format!(
                "tpm.mode = {:?}; PVH cannot expose vTPM device model, HVM can",
                profile.tpm.mode),
        };
    }

    if let Some(src) = profile.storage.root.source.as_deref()
        .or(profile.storage.root.path.as_deref())
        && probe.looks_linux(src)
    {
        return ModeSelection {
            mode: XenDomainMode::Pvh,
            rationale: format!(
                "root disk {src} carries a Linux filesystem; PVH is the \
                 modern paravirt path (no qemu-dm in dom0)"),
        };
    }

    ModeSelection {
        mode: XenDomainMode::Hvm,
        rationale: "no Linux signal in profile or root disk; defaulting to HVM \
                    (works for any guest OS, costs ~80 MB qemu-dm in dom0)".into(),
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use rotten_apple_manifest::Profile;

    struct FakeProbe { linux: bool }
    impl DiskProbe for FakeProbe {
        fn looks_linux(&self, _: &str) -> bool { self.linux }
    }

    fn linux_disk_profile() -> Profile {
        load(r#"
            [profile]
            name = "x"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "8G"
            memory_idle = "4G"
            memory_minimum = "2G"
            vcpus_active = 2
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "block", source = "/dev/nvme0n1p2", mode = "rw-exclusive" }
            [networking]
            interfaces = []
            [gpu]
            mode = "virtual"
            [tpm]
            mode = "none"
            [autostart]
        "#)
    }

    fn linux_disk_profile_path_only() -> Profile {
        load(r#"
            [profile]
            name = "x"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "8G"
            memory_idle = "4G"
            memory_minimum = "2G"
            vcpus_active = 2
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/var/lib/x.qcow2", mode = "rw-exclusive" }
            [networking]
            interfaces = []
            [gpu]
            mode = "virtual"
            [tpm]
            mode = "none"
            [autostart]
        "#)
    }

    fn windows_swtpm_profile() -> Profile {
        load(r#"
            [profile]
            name = "x"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "8G"
            memory_idle = "4G"
            memory_minimum = "2G"
            vcpus_active = 2
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/var/lib/x.qcow2", mode = "rw-exclusive" }
            [networking]
            interfaces = []
            [gpu]
            mode = "virtual"
            [tpm]
            mode = "swtpm"
            [autostart]
        "#)
    }

    fn load(toml_str: &str) -> Profile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), toml_str).unwrap();
        Profile::load(f.path()).unwrap()
    }

    #[test]
    fn tpm_swtpm_forces_hvm() {
        let p = windows_swtpm_profile();
        let s = select_mode(&p, &FakeProbe { linux: true });
        assert_eq!(s.mode, XenDomainMode::Hvm);
        assert!(s.rationale.contains("vTPM"));
    }

    #[test]
    fn linux_disk_picks_pvh() {
        let p = linux_disk_profile();
        let s = select_mode(&p, &FakeProbe { linux: true });
        assert_eq!(s.mode, XenDomainMode::Pvh);
        assert!(s.rationale.contains("Linux"));
    }

    #[test]
    fn linux_disk_path_only_picks_pvh() {
        let p = linux_disk_profile_path_only();
        let s = select_mode(&p, &FakeProbe { linux: true });
        assert_eq!(s.mode, XenDomainMode::Pvh);
    }

    #[test]
    fn unknown_disk_defaults_to_hvm() {
        let p = linux_disk_profile();
        let s = select_mode(&p, &FakeProbe { linux: false });
        assert_eq!(s.mode, XenDomainMode::Hvm);
        assert!(s.rationale.contains("defaulting to HVM"));
    }

    #[test]
    fn mode_display_is_lowercase() {
        assert_eq!(XenDomainMode::Pv.to_string(), "pv");
        assert_eq!(XenDomainMode::Pvh.to_string(), "pvh");
        assert_eq!(XenDomainMode::Hvm.to_string(), "hvm");
    }
}
