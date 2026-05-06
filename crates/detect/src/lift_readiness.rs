//! Lift-specific probes — answers questions the bootstrapper needs
//! before it can rewrite the boot path and create the Ubuntu domU.
//!
//! Distinct from [`crate::Detection`] which is the general "where am I"
//! report; this one is "would lifting *this* machine work, and what does
//! the manifest for the resulting Ubuntu domU look like?"
//!
//! Probes:
//!   - root / boot partition device paths (for disk passthrough)
//!   - LUKS underneath / or /boot (kills pygrub if /boot is encrypted)
//!   - filesystem types (pygrub only handles ext{2,3,4} and vfat)
//!   - hibernation configuration (must be off before lift)
//!   - IOMMU groups + which group the GPU is in (passthrough planning)
//!   - GRUB flavor (UEFI vs BIOS, signed vs unsigned)

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct LiftReadiness {
    /// Device backing `/`, e.g. `/dev/nvme0n1p2` or `/dev/mapper/cryptroot`.
    pub root_source: Option<String>,
    /// Device backing `/boot` if it has its own mount; `None` if /boot is
    /// part of /.
    pub boot_source: Option<String>,
    pub boot_separate: bool,
    pub root_on_luks: bool,
    pub boot_on_luks: bool,
    pub root_fs: Option<String>,
    pub boot_fs: Option<String>,
    /// True if `/sys/power/resume` is set to a real device. Means the
    /// system is configured to resume from hibernation; user must do a
    /// full poweroff (not hibernate) before lift.
    pub hibernation_configured: bool,
    pub resume_device: Option<String>,
    pub iommu_groups: Vec<IommuGroup>,
    /// IOMMU group containing the (or a) display controller.
    pub gpu_group: Option<u32>,
    pub grub_flavor: Vec<String>,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IommuGroup {
    pub id: u32,
    pub devices: Vec<PciDevice>,
}

#[derive(Debug, Clone)]
pub struct PciDevice {
    pub addr: String,
    /// Raw class code from sysfs, e.g. `0x030000` for VGA.
    pub class_hex: String,
    /// Human label for the class, e.g. "VGA / display".
    pub class_label: String,
    /// `vendor:device` in PCI-ID form, e.g. `8086:46a6`. None if sysfs
    /// is unreadable.
    pub vendor_device: Option<String>,
}

impl LiftReadiness {
    pub fn run() -> Self {
        let root_source = mount_source("/");
        let boot_source = mount_source("/boot");
        let boot_separate = boot_source.is_some() && boot_source != root_source;
        let effective_boot = if boot_separate { boot_source.clone() } else { root_source.clone() };

        let root_on_luks = root_source.as_deref().map(is_on_luks).unwrap_or(false);
        let boot_on_luks = effective_boot.as_deref().map(is_on_luks).unwrap_or(false);
        let root_fs = root_source.as_deref().and_then(fstype_of);
        let boot_fs = effective_boot.as_deref().and_then(fstype_of);

        let resume_device = read_resume_device();
        let hibernation_configured = resume_device.is_some();

        let iommu_groups = enumerate_iommu_groups();
        // 0x03xxxx = display controller class (VGA, 3D, other display)
        let gpu_group = iommu_groups.iter()
            .find(|g| g.devices.iter().any(|d| d.class_hex.starts_with("0x03")))
            .map(|g| g.id);

        let grub_flavor = detect_grub_flavor();

        let mut r = LiftReadiness {
            root_source, boot_source, boot_separate,
            root_on_luks, boot_on_luks,
            root_fs, boot_fs,
            hibernation_configured, resume_device,
            iommu_groups, gpu_group,
            grub_flavor,
            warnings: vec![], blockers: vec![],
        };
        r.classify();
        r
    }

    fn classify(&mut self) {
        if self.boot_on_luks {
            self.warnings.push(
                "/boot lives on LUKS; pygrub cannot read it. Lift must \
                 ship explicit kernel+initrd in dom0 instead of bootloader=pygrub.".into());
        }
        if self.root_on_luks {
            self.warnings.push(
                "/ lives on LUKS; the Ubuntu domU will prompt for the \
                 passphrase at boot the same way bare-metal does (in the \
                 domU console). No change to passthrough strategy.".into());
        }
        if let Some(fs) = &self.boot_fs
            && !matches!(fs.as_str(), "ext2" | "ext3" | "ext4" | "vfat")
        {
            self.warnings.push(format!(
                "/boot filesystem is {fs}; pygrub may not handle it. \
                 Prefer explicit kernel+initrd if uncertain."));
        }
        if self.hibernation_configured {
            self.warnings.push(
                "hibernation is configured (/sys/power/resume is set). \
                 Before lifting, do a full poweroff — NOT hibernate. A \
                 pending hibernation image would corrupt the FS when the \
                 lifted Ubuntu boots as a domU.".into());
        }
        if let Some(gpu_grp) = self.gpu_group {
            let group = self.iommu_groups.iter().find(|g| g.id == gpu_grp);
            if let Some(group) = group {
                for dev in &group.devices {
                    // 0x01 = mass storage controller
                    if dev.class_hex.starts_with("0x01") {
                        self.blockers.push(format!(
                            "GPU IOMMU group {gpu_grp} contains storage controller \
                             {} ({}). Passing the GPU to Ubuntu would also pass the \
                             disk; dom0 would lose its root.",
                            dev.addr, dev.class_label));
                    }
                    // 0x0c03 = USB controller
                    if dev.class_hex.starts_with("0x0c03") {
                        self.warnings.push(format!(
                            "GPU IOMMU group {gpu_grp} contains USB controller {} — \
                             all USB on that controller follows the GPU into Ubuntu \
                             (likely fine; dom0 is headless).", dev.addr));
                    }
                    // 0x02 = network controller
                    if dev.class_hex.starts_with("0x02") {
                        self.warnings.push(format!(
                            "GPU IOMMU group {gpu_grp} contains network device {} — \
                             dom0 loses that NIC. Ensure another NIC is available \
                             to dom0 or you'll lose remote access.", dev.addr));
                    }
                }
            }
        } else if !self.iommu_groups.is_empty() {
            self.warnings.push(
                "no display controller found in any IOMMU group. The \
                 Ubuntu domU will get virtual framebuffer only; no native \
                 graphics.".into());
        } else {
            self.warnings.push(
                "no IOMMU groups visible. Either IOMMU is disabled in \
                 firmware/cmdline, or this kernel doesn't expose them. \
                 GPU passthrough will not work until this is fixed.".into());
        }
        if self.grub_flavor.is_empty() {
            self.warnings.push(
                "no GRUB package detected (grub-pc / grub-efi-amd64). \
                 Lift will install grub-xen but you should confirm which \
                 grub-* the system currently boots through.".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Probes

fn mount_source(mount: &str) -> Option<String> {
    let out = run_capture("findmnt", &["-no", "SOURCE", mount])?;
    let s = out.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

fn fstype_of(dev: &str) -> Option<String> {
    let out = run_capture("lsblk", &["-no", "FSTYPE", dev])?;
    let s = out.lines().next().unwrap_or("").trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// `lsblk -s` walks the tree UPWARD from `dev` to its parents. If any
/// ancestor reports `crypto_LUKS`, this mount sits on encrypted storage.
fn is_on_luks(dev: &str) -> bool {
    let Some(out) = run_capture("lsblk", &["-sno", "FSTYPE", dev]) else { return false };
    out.lines().any(|l| l.trim() == "crypto_LUKS")
}

/// `/sys/power/resume` holds the dev_t of the configured resume device
/// (e.g. `"259:2"`). The kernel writes `"0:0"` (or empty) when none is
/// configured.
fn read_resume_device() -> Option<String> {
    let s = fs::read_to_string("/sys/power/resume").ok()?;
    let s = s.trim();
    if s.is_empty() || s == "0:0" { None } else { Some(s.to_string()) }
}

fn enumerate_iommu_groups() -> Vec<IommuGroup> {
    let dir = Path::new("/sys/kernel/iommu_groups");
    let Ok(entries) = fs::read_dir(dir) else { return vec![] };
    let mut groups: BTreeMap<u32, Vec<PciDevice>> = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(id) = name.to_string_lossy().parse::<u32>() else { continue };
        let dev_dir = entry.path().join("devices");
        let Ok(devs) = fs::read_dir(&dev_dir) else { continue };
        let mut list = vec![];
        for d in devs.flatten() {
            let addr = d.file_name().to_string_lossy().into_owned();
            let class_hex = read_first_line(
                &format!("/sys/bus/pci/devices/{addr}/class"))
                .unwrap_or_else(|| "0x000000".into());
            let class_label = pci_class_label(&class_hex);
            let vendor_device = read_pci_ids(&addr);
            list.push(PciDevice { addr, class_hex, class_label, vendor_device });
        }
        list.sort_by(|a, b| a.addr.cmp(&b.addr));
        groups.insert(id, list);
    }
    groups.into_iter().map(|(id, devices)| IommuGroup { id, devices }).collect()
}

fn read_first_line(path: &str) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    Some(s.lines().next().unwrap_or("").trim().to_string())
}

fn read_pci_ids(addr: &str) -> Option<String> {
    let v = read_first_line(&format!("/sys/bus/pci/devices/{addr}/vendor"))?;
    let d = read_first_line(&format!("/sys/bus/pci/devices/{addr}/device"))?;
    Some(format!("{}:{}",
        v.trim_start_matches("0x"),
        d.trim_start_matches("0x")))
}

/// PCI class is `0xCCSSPP` — class / subclass / programming interface.
/// We translate the well-known ones; everything else falls back to a
/// coarse top-byte label.
fn pci_class_label(class_hex: &str) -> String {
    let cs = class_hex.trim_start_matches("0x");
    let top: String = cs.chars().take(2).collect();
    let mid: String = cs.chars().take(4).collect();
    let label = match (top.as_str(), mid.as_str()) {
        (_, "0100") => "SCSI controller",
        (_, "0101") => "IDE controller",
        (_, "0104") => "RAID controller",
        (_, "0106") => "SATA controller",
        (_, "0108") => "NVMe controller",
        (_, "0200") => "Ethernet controller",
        (_, "0280") => "Wi-Fi / wireless",
        (_, "0300") => "VGA / display",
        (_, "0302") => "3D / GPU",
        (_, "0403") => "Audio device",
        (_, "0604") => "PCI bridge",
        (_, "0c03") => "USB controller",
        (_, "0c05") => "SMBus",
        (_, "0c80") => "Serial bus",
        ("01", _)   => "storage controller",
        ("02", _)   => "network controller",
        ("03", _)   => "display controller",
        ("0c", _)   => "serial-bus controller",
        _           => "device",
    };
    label.to_string()
}

fn detect_grub_flavor() -> Vec<String> {
    let Some(out) = run_capture("dpkg", &["-l"]) else { return vec![] };
    let interesting = ["grub-pc", "grub-efi-amd64", "grub-efi-amd64-signed",
                       "grub-efi-arm64", "grub-common"];
    let mut found = vec![];
    for line in out.lines() {
        if !line.starts_with("ii") { continue }
        let pkg = line.split_whitespace().nth(1).unwrap_or("");
        if interesting.contains(&pkg) {
            found.push(pkg.to_string());
        }
    }
    found
}

fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() { return None }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lift_readiness_runs_without_panic() {
        let _ = LiftReadiness::run();
    }

    #[test]
    fn classify_blocks_when_storage_in_gpu_group() {
        let mut r = baseline();
        r.iommu_groups = vec![IommuGroup {
            id: 1,
            devices: vec![
                PciDevice { addr: "0000:00:02.0".into(), class_hex: "0x030000".into(),
                            class_label: "VGA / display".into(), vendor_device: None },
                PciDevice { addr: "0000:00:1f.2".into(), class_hex: "0x010601".into(),
                            class_label: "SATA controller".into(), vendor_device: None },
            ],
        }];
        r.gpu_group = Some(1);
        r.classify();
        assert!(r.blockers.iter().any(|b| b.contains("storage controller")),
                "blockers: {:?}", r.blockers);
    }

    #[test]
    fn classify_warns_on_luks_boot() {
        let mut r = baseline();
        r.boot_on_luks = true;
        r.classify();
        assert!(r.warnings.iter().any(|w| w.contains("pygrub cannot read")));
    }

    #[test]
    fn classify_warns_on_hibernation() {
        let mut r = baseline();
        r.hibernation_configured = true;
        r.classify();
        assert!(r.warnings.iter().any(|w| w.contains("hibernation")));
    }

    #[test]
    fn classify_warns_on_usb_in_gpu_group() {
        let mut r = baseline();
        r.iommu_groups = vec![IommuGroup {
            id: 2,
            devices: vec![
                PciDevice { addr: "0000:00:02.0".into(), class_hex: "0x030000".into(),
                            class_label: "VGA / display".into(), vendor_device: None },
                PciDevice { addr: "0000:00:14.0".into(), class_hex: "0x0c0330".into(),
                            class_label: "USB controller".into(), vendor_device: None },
            ],
        }];
        r.gpu_group = Some(2);
        r.classify();
        assert!(r.warnings.iter().any(|w| w.contains("USB controller")),
                "warnings: {:?}", r.warnings);
        assert!(r.blockers.is_empty(), "USB in GPU group should be a warning, not blocker");
    }

    #[test]
    fn classify_warns_on_no_iommu_at_all() {
        let mut r = baseline();
        r.iommu_groups = vec![];
        r.gpu_group = None;
        r.classify();
        assert!(r.warnings.iter().any(|w| w.contains("no IOMMU groups")));
    }

    #[test]
    fn pci_class_label_recognises_common_classes() {
        assert_eq!(pci_class_label("0x030000"), "VGA / display");
        assert_eq!(pci_class_label("0x0c0330"), "USB controller");
        assert_eq!(pci_class_label("0x010802"), "NVMe controller");
        assert_eq!(pci_class_label("0x010601"), "SATA controller");
        assert_eq!(pci_class_label("0x020000"), "Ethernet controller");
    }

    #[test]
    fn pci_class_label_falls_back_for_unknown() {
        assert_eq!(pci_class_label("0xff0000"), "device");
        assert_eq!(pci_class_label("0x019900"), "storage controller");
    }

    fn baseline() -> LiftReadiness {
        LiftReadiness {
            root_source: Some("/dev/nvme0n1p2".into()),
            boot_source: None,
            boot_separate: false,
            root_on_luks: false,
            boot_on_luks: false,
            root_fs: Some("ext4".into()),
            boot_fs: Some("ext4".into()),
            hibernation_configured: false,
            resume_device: None,
            iommu_groups: vec![],
            gpu_group: None,
            grub_flavor: vec!["grub-efi-amd64".into()],
            warnings: vec![], blockers: vec![],
        }
    }
}
