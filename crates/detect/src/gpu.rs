//! GPU detection — auto, no config knobs.
//!
//! Walks `/sys/bus/pci/devices/*` looking for VGA/3D-class devices,
//! determines which one is currently driving the host display by
//! resolving `/sys/class/graphics/fb0/device` to a BDF, and computes
//! a [`GpuRole`] for each device. Re-runs every boot — there's no
//! state on disk that could go stale.
//!
//! Topology cases this is meant to cover without special-cases:
//!   - laptop iGPU + Optimus dGPU (the dGPU has no display cable)
//!   - workstation with no iGPU and 2+ dGPUs
//!   - SoC / APU like Steam Deck (only one GPU, and it's the host)
//!   - cheap laptop with iGPU only
//!
//! In every case the framebuffer GPU gets [`GpuRole::Framebuffer`]
//! ("sacred — driving the host display"); other GPUs are
//! [`GpuRole::Leasable`] when they sit alone in an IOMMU group, or
//! [`GpuRole::Locked`] when they share a group with the framebuffer
//! or with platform devices we can't yank.
//!
//! `Locked` also covers the "IOMMU not enabled in dom0 cmdline" case —
//! the iommu_groups directory is empty, every non-framebuffer GPU
//! becomes Locked with `LockReason::IommuOff`. The cockpit's lease
//! flow turns that into a one-keypress "enable + reboot" prompt
//! rather than asking the user to know about kernel cmdlines.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    pub bdf: String,                  // "0000:01:00.0"
    pub vendor: GpuVendor,
    pub device_name: String,          // best-effort, may be empty
    pub vendor_id: u16,
    pub device_id: u16,
    pub iommu_group: Option<u32>,
    pub current_driver: Option<String>,
    pub role: GpuRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Intel,
    Nvidia,
    Amd,
    Other(u16),
}

impl GpuVendor {
    pub fn from_id(vendor_id: u16) -> Self {
        match vendor_id {
            0x8086 => GpuVendor::Intel,
            0x10de => GpuVendor::Nvidia,
            0x1002 | 0x1022 => GpuVendor::Amd,
            other  => GpuVendor::Other(other),
        }
    }

    pub fn label(self) -> String {
        match self {
            GpuVendor::Intel  => "Intel".to_string(),
            GpuVendor::Nvidia => "NVIDIA".to_string(),
            GpuVendor::Amd    => "AMD".to_string(),
            GpuVendor::Other(id) => format!("vendor 0x{id:04x}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuRole {
    /// Currently driving dom0's host display. Sacred — never detached.
    Framebuffer,
    /// Sits alone (or with siblings/bridges) in an IOMMU group; safe
    /// to bind to xen-pciback and pass through to a guest.
    Leasable,
    /// Cannot be passed through. The reason is one of:
    ///   - IOMMU not enabled in dom0 cmdline
    ///   - shares its IOMMU group with the framebuffer GPU
    ///   - shares its IOMMU group with a non-GPU platform device
    Locked(LockReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockReason {
    /// `/sys/kernel/iommu_groups/` is empty — IOMMU off in dom0 cmdline.
    /// Recoverable: bootstrapper can add `intel_iommu=on` / `amd_iommu=on`
    /// and reboot.
    IommuOff,
    /// Shares IOMMU group with the GPU currently driving dom0's display.
    /// Detaching would yank the host's screen.
    SharedWithFramebuffer,
    /// Shares IOMMU group with a non-GPU platform device (chipset, USB
    /// controller, etc.). Common on consumer boards with bad ACS support.
    /// Quirk-able with `pcie_acs_override=` but that's a security tradeoff
    /// we won't enable silently.
    SharedWithPlatform,
}

impl GpuRole {
    pub fn label(&self) -> &'static str {
        match self {
            GpuRole::Framebuffer => "framebuffer",
            GpuRole::Leasable    => "leasable",
            GpuRole::Locked(_)   => "locked",
        }
    }
}

/// Best-effort enumeration. Empty Vec on a host that has no GPU at all
/// (very rare — usually a server with serial console). Caller treats
/// "no leasable" identically to "no GPUs".
pub fn enumerate_gpus() -> Vec<GpuDevice> {
    enumerate_gpus_under(Path::new("/sys"))
}

/// Test seam — walks `<sysroot>/bus/pci/devices/` and friends so the
/// suite can fixture a fake sysfs.
pub fn enumerate_gpus_under(sysroot: &Path) -> Vec<GpuDevice> {
    let pci_root = sysroot.join("bus/pci/devices");
    let mut found: Vec<GpuDevice> = Vec::new();
    let Ok(entries) = fs::read_dir(&pci_root) else { return found };

    let framebuffer_bdf = framebuffer_bdf_under(sysroot);

    for entry in entries.flatten() {
        let path = entry.path();
        // The directory name is the BDF.
        let Some(bdf) = path.file_name().and_then(|s| s.to_str()) else { continue };
        if !is_gpu_class(&path) { continue }
        let (vendor_id, device_id) = match read_pci_ids(&path) {
            Some(v) => v,
            None    => continue,
        };
        let iommu_group = read_iommu_group(&path);
        let current_driver = read_current_driver(&path);
        let device_name = read_pci_device_name(&path).unwrap_or_default();

        let role = compute_role(
            bdf, framebuffer_bdf.as_deref(),
            iommu_group, sysroot,
        );

        found.push(GpuDevice {
            bdf: bdf.to_string(),
            vendor: GpuVendor::from_id(vendor_id),
            device_name,
            vendor_id,
            device_id,
            iommu_group,
            current_driver,
            role,
        });
    }

    // Stable ordering: framebuffer first, then leasable, then locked.
    // Within each bucket, sort by BDF for determinism.
    found.sort_by_key(|d| {
        let bucket = match d.role {
            GpuRole::Framebuffer => 0,
            GpuRole::Leasable    => 1,
            GpuRole::Locked(_)   => 2,
        };
        (bucket, d.bdf.clone())
    });
    found
}

fn is_gpu_class(pci_dev: &Path) -> bool {
    // PCI class is a 6-hex-digit code (24 bits). High byte 0x03 is
    // "Display controller"; all sub-classes (VGA, 3D, other display)
    // count.
    let Ok(s) = fs::read_to_string(pci_dev.join("class")) else { return false };
    let s = s.trim().trim_start_matches("0x");
    let Ok(class) = u32::from_str_radix(s, 16) else { return false };
    (class >> 16) & 0xff == 0x03
}

fn read_pci_ids(pci_dev: &Path) -> Option<(u16, u16)> {
    let v = parse_hex_u16(&fs::read_to_string(pci_dev.join("vendor")).ok()?)?;
    let d = parse_hex_u16(&fs::read_to_string(pci_dev.join("device")).ok()?)?;
    Some((v, d))
}

fn parse_hex_u16(raw: &str) -> Option<u16> {
    let s = raw.trim().trim_start_matches("0x");
    u16::from_str_radix(s, 16).ok()
}

fn read_iommu_group(pci_dev: &Path) -> Option<u32> {
    let link = fs::read_link(pci_dev.join("iommu_group")).ok()?;
    link.file_name()?
        .to_str()?
        .parse::<u32>()
        .ok()
}

fn read_current_driver(pci_dev: &Path) -> Option<String> {
    let link = fs::read_link(pci_dev.join("driver")).ok()?;
    Some(link.file_name()?.to_str()?.to_string())
}

/// `lspci`-quality device name without the lspci dep. We read the
/// ID strings from /usr/share/hwdata/pci.ids when available; if not,
/// fall back to "vendor:device" hex. The hwdata file is huge but we
/// only scan it once during detection.
fn read_pci_device_name(_pci_dev: &Path) -> Option<String> {
    // v0: leave empty; cockpit displays "<vendor> <BDF>" anyway. A
    // future enhancement can pull pci.ids; not worth the complexity now.
    None
}

/// Read the BDF currently backing /dev/fb0 (or fb1, fb2 — first hit).
/// Returns None on headless hosts.
fn framebuffer_bdf_under(sysroot: &Path) -> Option<String> {
    let graphics = sysroot.join("class/graphics");
    let Ok(entries) = fs::read_dir(&graphics) else { return None };
    let mut fbs: Vec<_> = entries.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("fb").then_some(name)
        })
        .collect();
    fbs.sort();  // fb0 first — that's the active console
    for fb in fbs {
        if let Ok(target) = fs::read_link(graphics.join(&fb).join("device"))
            && let Some(bdf) = target.file_name().and_then(|n| n.to_str())
        {
            return Some(bdf.to_string());
        }
    }
    None
}

fn compute_role(
    bdf: &str,
    framebuffer_bdf: Option<&str>,
    iommu_group: Option<u32>,
    sysroot: &Path,
) -> GpuRole {
    if Some(bdf) == framebuffer_bdf {
        return GpuRole::Framebuffer;
    }

    let Some(group) = iommu_group else {
        // No IOMMU group → IOMMU is off in dom0 cmdline OR this host
        // has no IOMMU at all. Either way, we can't isolate the device.
        return GpuRole::Locked(LockReason::IommuOff);
    };

    // Walk the group's other devices. Lease-safe if the group contains
    // only this GPU + function siblings (e.g. its HDMI audio device) +
    // PCIe bridges. Anything else (chipset, USB, SATA in the same group)
    // is SharedWithPlatform.
    let group_dir = sysroot.join(format!("kernel/iommu_groups/{group}/devices"));
    let Ok(entries) = fs::read_dir(&group_dir) else {
        // Group exists but we can't enumerate it — treat as IommuOff
        // rather than blindly leasing.
        return GpuRole::Locked(LockReason::IommuOff);
    };
    let my_root = bdf_root(bdf);
    for entry in entries.flatten() {
        let other = entry.file_name().to_string_lossy().into_owned();
        if other == bdf { continue }
        if Some(other.as_str()) == framebuffer_bdf {
            return GpuRole::Locked(LockReason::SharedWithFramebuffer);
        }
        // Function siblings of this GPU (same bus:device, different
        // function): allowed. They get pulled through together.
        if bdf_root(&other) == my_root { continue }
        // PCIe bridges (class 0x0604xx) are allowed — xen-pciback
        // handles them transparently.
        let other_dev = sysroot.join("bus/pci/devices").join(&other);
        if is_pci_bridge(&other_dev) { continue }
        // Anything else co-located is a no-go.
        return GpuRole::Locked(LockReason::SharedWithPlatform);
    }
    GpuRole::Leasable
}

/// "0000:01:00.0" → "0000:01:00" — strips the function digit so we
/// can recognise function siblings (HDMI audio, USB-C controller on
/// some GPUs, …) as part of the same physical card.
fn bdf_root(bdf: &str) -> &str {
    bdf.rsplit_once('.').map(|(root, _)| root).unwrap_or(bdf)
}

fn is_pci_bridge(pci_dev: &Path) -> bool {
    let Ok(s) = fs::read_to_string(pci_dev.join("class")) else { return false };
    let s = s.trim().trim_start_matches("0x");
    let Ok(class) = u32::from_str_radix(s, 16) else { return false };
    // 0x06xxxx is "Bridge"; 0x0604xx is "PCI-to-PCI bridge".
    (class >> 16) & 0xff == 0x06
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// Set up a fake sysfs tree. Returns the tempdir; structure:
    ///   <root>/bus/pci/devices/<BDF>/{class,vendor,device}
    ///   <root>/bus/pci/devices/<BDF>/iommu_group   (symlink → group dir)
    ///   <root>/bus/pci/devices/<BDF>/driver        (symlink → driver dir)
    ///   <root>/kernel/iommu_groups/<id>/devices/<BDF>  (symlink)
    ///   <root>/class/graphics/fb0/device           (symlink → BDF dir)
    fn fake_sysfs() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bus/pci/devices")).unwrap();
        fs::create_dir_all(dir.path().join("kernel/iommu_groups")).unwrap();
        fs::create_dir_all(dir.path().join("class/graphics")).unwrap();
        dir
    }

    fn add_gpu(
        root: &Path, bdf: &str, vendor_id: u16, device_id: u16,
        class_hex: &str, group: Option<u32>, driver: Option<&str>,
    ) {
        let dev = root.join("bus/pci/devices").join(bdf);
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("class"), format!("{class_hex}\n")).unwrap();
        fs::write(dev.join("vendor"), format!("0x{vendor_id:04x}\n")).unwrap();
        fs::write(dev.join("device"), format!("0x{device_id:04x}\n")).unwrap();
        if let Some(g) = group {
            let gd = root.join(format!("kernel/iommu_groups/{g}/devices"));
            fs::create_dir_all(&gd).unwrap();
            symlink(&dev, gd.join(bdf)).unwrap();
            symlink(root.join(format!("kernel/iommu_groups/{g}")),
                    dev.join("iommu_group")).unwrap();
        }
        if let Some(d) = driver {
            let drv_dir = root.join(format!("bus/pci/drivers/{d}"));
            fs::create_dir_all(&drv_dir).unwrap();
            symlink(&drv_dir, dev.join("driver")).unwrap();
        }
    }

    fn set_framebuffer(root: &Path, bdf: &str) {
        let fb = root.join("class/graphics/fb0");
        fs::create_dir_all(&fb).unwrap();
        symlink(root.join("bus/pci/devices").join(bdf), fb.join("device"))
            .unwrap();
    }

    #[test]
    fn enumerate_empty_when_no_pci_devices() {
        let dir = fake_sysfs();
        assert!(enumerate_gpus_under(dir.path()).is_empty());
    }

    #[test]
    fn laptop_igpu_plus_dgpu_classifies_correctly() {
        // Mirrors this dev host: Intel Iris Xe + NVIDIA RTX A3000 Laptop.
        let dir = fake_sysfs();
        let root = dir.path();
        add_gpu(root, "0000:00:02.0", 0x8086, 0x46a6, "0x030000",
                Some(0), Some("i915"));
        add_gpu(root, "0000:01:00.0", 0x10de, 0x24bb, "0x030000",
                Some(1), Some("nvidia"));
        set_framebuffer(root, "0000:00:02.0");

        let gpus = enumerate_gpus_under(root);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].bdf, "0000:00:02.0");
        assert_eq!(gpus[0].vendor, GpuVendor::Intel);
        assert_eq!(gpus[0].role, GpuRole::Framebuffer);
        assert_eq!(gpus[1].bdf, "0000:01:00.0");
        assert_eq!(gpus[1].vendor, GpuVendor::Nvidia);
        assert_eq!(gpus[1].role, GpuRole::Leasable);
    }

    #[test]
    fn iommu_off_locks_non_framebuffer_gpus() {
        // No iommu_groups dir entries → IOMMU off in cmdline.
        let dir = fake_sysfs();
        let root = dir.path();
        add_gpu(root, "0000:00:02.0", 0x8086, 0x46a6, "0x030000",
                None, Some("i915"));
        add_gpu(root, "0000:01:00.0", 0x10de, 0x24bb, "0x030000",
                None, Some("nvidia"));
        set_framebuffer(root, "0000:00:02.0");

        let gpus = enumerate_gpus_under(root);
        assert_eq!(gpus[0].role, GpuRole::Framebuffer);
        assert_eq!(gpus[1].role, GpuRole::Locked(LockReason::IommuOff));
    }

    #[test]
    fn shared_with_framebuffer_is_locked() {
        // Pathological board where Intel iGPU and a discrete card
        // share an IOMMU group — happens on cheap consumer boards.
        let dir = fake_sysfs();
        let root = dir.path();
        add_gpu(root, "0000:00:02.0", 0x8086, 0x46a6, "0x030000",
                Some(7), Some("i915"));
        add_gpu(root, "0000:01:00.0", 0x10de, 0x24bb, "0x030000",
                Some(7), Some("nvidia"));
        set_framebuffer(root, "0000:00:02.0");

        let gpus = enumerate_gpus_under(root);
        assert_eq!(gpus[1].role,
            GpuRole::Locked(LockReason::SharedWithFramebuffer));
    }

    #[test]
    fn function_sibling_does_not_lock_gpu() {
        // GPU + its HDMI-audio function are in the same group. That's
        // expected and shouldn't lock the GPU — they pass through together.
        let dir = fake_sysfs();
        let root = dir.path();
        add_gpu(root, "0000:00:02.0", 0x8086, 0x46a6, "0x030000",
                Some(0), Some("i915"));
        add_gpu(root, "0000:01:00.0", 0x10de, 0x24bb, "0x030000",
                Some(1), Some("nvidia"));
        // HDMI audio function on the same physical card. Class
        // 0x040300 is "HDMI Audio", lives at the same bus:dev with
        // function .1.
        let dev = root.join("bus/pci/devices/0000:01:00.1");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("class"), "0x040300\n").unwrap();
        fs::write(dev.join("vendor"), "0x10de\n").unwrap();
        fs::write(dev.join("device"), "0x228b\n").unwrap();
        let gd = root.join("kernel/iommu_groups/1/devices");
        symlink(&dev, gd.join("0000:01:00.1")).unwrap();
        symlink(root.join("kernel/iommu_groups/1"),
                dev.join("iommu_group")).unwrap();
        set_framebuffer(root, "0000:00:02.0");

        let gpus = enumerate_gpus_under(root);
        let nv = gpus.iter().find(|g| g.bdf == "0000:01:00.0").unwrap();
        assert_eq!(nv.role, GpuRole::Leasable,
            "GPU + HDMI audio sibling must stay leasable");
    }

    #[test]
    fn soc_single_gpu_has_no_leasable() {
        // Steam Deck / cheap laptop topology: only one GPU, and it's
        // the framebuffer. Vec of 1, role=Framebuffer, leasable count=0.
        let dir = fake_sysfs();
        let root = dir.path();
        add_gpu(root, "0000:00:01.0", 0x1002, 0x163f, "0x030000",
                Some(0), Some("amdgpu"));
        set_framebuffer(root, "0000:00:01.0");

        let gpus = enumerate_gpus_under(root);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].role, GpuRole::Framebuffer);
        assert_eq!(gpus[0].vendor, GpuVendor::Amd);
        assert_eq!(leasable_count(&gpus), 0);
    }

    #[test]
    fn dual_dgpu_workstation_picks_framebuffer_from_fb0() {
        // No iGPU. Two RTX 4090s. Whichever drives fb0 is the
        // framebuffer; the other is leasable.
        let dir = fake_sysfs();
        let root = dir.path();
        add_gpu(root, "0000:01:00.0", 0x10de, 0x2684, "0x030000",
                Some(1), Some("nvidia"));
        add_gpu(root, "0000:02:00.0", 0x10de, 0x2684, "0x030000",
                Some(2), Some("nvidia"));
        set_framebuffer(root, "0000:02:00.0");

        let gpus = enumerate_gpus_under(root);
        let fb = gpus.iter().find(|g| g.role == GpuRole::Framebuffer).unwrap();
        assert_eq!(fb.bdf, "0000:02:00.0");
        let lease = gpus.iter()
            .find(|g| matches!(g.role, GpuRole::Leasable)).unwrap();
        assert_eq!(lease.bdf, "0000:01:00.0");
    }

    #[test]
    fn vendor_from_id_known_cases() {
        assert_eq!(GpuVendor::from_id(0x8086), GpuVendor::Intel);
        assert_eq!(GpuVendor::from_id(0x10de), GpuVendor::Nvidia);
        assert_eq!(GpuVendor::from_id(0x1002), GpuVendor::Amd);
        assert_eq!(GpuVendor::from_id(0x1022), GpuVendor::Amd);
        assert_eq!(GpuVendor::from_id(0x1234), GpuVendor::Other(0x1234));
    }

    /// Helper used by tests + future callers (cockpit overlay).
    fn leasable_count(gpus: &[GpuDevice]) -> usize {
        gpus.iter().filter(|g| matches!(g.role, GpuRole::Leasable)).count()
    }
}
