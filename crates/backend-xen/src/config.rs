//! `libxl_domain_config` builder.
//!
//! Translates a hypervisor-agnostic [`Profile`] (plus a chosen
//! [`XenDomainMode`]) into a populated `libxl_domain_config` ready to
//! hand to `libxl_domain_create_new`.
//!
//! Two-layer split, so the field-by-field translation can be unit-tested
//! without real Xen:
//!
//!   1. [`DomainConfigPlan`] — a pure-Rust value capturing every decision
//!      we make from the profile + mode (name, mem, vcpus, disks, nics,
//!      bootloader). No FFI. Heavily tested.
//!   2. [`OwnedDomainConfig`] — the unsafe materialization of a plan into
//!      a real `libxl_domain_config`. Calls `libxl_*_init` for each
//!      sub-struct, `libc::strdup`s every C-string field (libxl frees on
//!      dispose), `libc::calloc`s device arrays. Drop calls
//!      `libxl_domain_config_dispose`, which iterates and frees
//!      everything we allocated. This layer cannot be tested without a
//!      live `libxl_ctx`.
//!
//! Disk strategy is whole-disk via the libxl `phy:` backend (xen-blkback
//! in dom0, xen-blkfront in the guest); the storage controller stays
//! with dom0 so multiple guests can share the same physical disk.
//! Bootloader for PVH/PV is `pygrub` (libxl reads kernel from the guest's
//! own /boot at start time, so apt kernel updates inside the guest stay
//! live).

use crate::compat;
use crate::ctx::map_rc;
use crate::mode::XenDomainMode;
use crate::sys;
use rotten_apple_backend::{BackendError, Result};
use rotten_apple_manifest::Profile;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::raw::c_char;
use std::ptr;

// ---------------------------------------------------------------------------
// Plan — pure Rust, fully testable

#[derive(Debug, Clone)]
pub struct DomainConfigPlan {
    pub name: String,
    pub mode: XenDomainMode,
    pub max_vcpus: u32,
    pub target_memkb: u64,
    pub max_memkb: u64,
    /// `Some("pygrub")` for PV / PVH (Linux); `None` for HVM (firmware boot).
    pub bootloader: Option<String>,
    pub disks: Vec<DiskPlan>,
    pub nics: Vec<NicPlan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    /// Raw block device or raw file. Source is treated byte-for-byte.
    Raw,
    /// qcow2 file — needs the qemu disk backend to interpret the
    /// copy-on-write metadata. Cloud images (Ubuntu, Debian) ship as
    /// qcow2; instance overlays we create are qcow2.
    Qcow2,
    /// vhd / vhdx — Hyper-V era disk format. Same backend as qcow2.
    Vhd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskBackend {
    /// `phy:` — raw block device handed to xen-blkback in dom0.
    /// Lowest overhead, but only works on real block devices, not files.
    Phy,
    /// `qdisk:` — qemu emulates the disk for the guest. Required for
    /// qcow2 / vhd / raw-file. ~80 MB extra dom0 RAM per HVM guest
    /// (the qemu-dm process), so we only choose it when we must.
    Qdisk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskPlan {
    /// Bare host path — `/dev/nvme0n1`, no `phy:` prefix.
    pub source: String,
    /// Guest-side device name — `xvda`, `xvdb`, …
    pub vdev: String,
    pub readwrite: bool,
    pub format: DiskFormat,
    pub backend: DiskBackend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NicPlan {
    pub bridge: String,
    /// `VIF` for paravirt guests, `VIF_IOEMU` for HVM (so Windows sees an
    /// emulated NIC at boot).
    pub nictype_is_ioemu: bool,
}

impl DomainConfigPlan {
    pub fn from_profile(profile: &Profile, mode: XenDomainMode) -> Result<Self> {
        // ---- root disk ----
        let root_src = profile.storage.root.source.as_deref()
            .or(profile.storage.root.path.as_deref())
            .ok_or_else(|| BackendError::insufficient_resources(
                "storage.root.source or storage.root.path is required for the Xen backend"))?;
        let (root_format, root_backend) = pick_format_backend(&profile.storage.root.kind);
        let mut disks = vec![DiskPlan {
            source: strip_phy_prefix(root_src).to_string(),
            vdev: "xvda".into(),
            readwrite: !is_readonly_mode(&profile.storage.root.mode),
            format: root_format,
            backend: root_backend,
        }];

        // ---- extra disks ----
        for (i, extra) in profile.storage.extra_disks.iter().enumerate() {
            let src = extra.source.as_deref()
                .or(extra.path.as_deref())
                .ok_or_else(|| BackendError::insufficient_resources(
                    "extra_disks[].source or extra_disks[].path is required"))?;
            let (fmt, be) = pick_format_backend(&extra.kind);
            disks.push(DiskPlan {
                source: strip_phy_prefix(src).to_string(),
                vdev: format!("xvd{}", (b'b' + i as u8) as char),
                readwrite: !is_readonly_mode(&extra.mode),
                format: fmt,
                backend: be,
            });
        }

        // ---- nics ----
        let nictype_is_ioemu = matches!(mode, XenDomainMode::Hvm);
        let nics: Vec<NicPlan> = profile.network.interfaces.iter()
            .map(|_| NicPlan { bridge: default_bridge_name(), nictype_is_ioemu })
            .collect();

        // ---- bootloader ----
        let bootloader = match mode {
            XenDomainMode::Hvm => None,
            XenDomainMode::Pv | XenDomainMode::Pvh => Some("pygrub".into()),
        };

        let target_memkb = profile.resources.memory_active_bytes / 1024;

        Ok(DomainConfigPlan {
            name: profile.name().to_string(),
            mode,
            max_vcpus: profile.resources.vcpus_active,
            target_memkb,
            // We only balloon DOWN; max == target keeps the guest from
            // requesting growth beyond its declared active size.
            max_memkb: target_memkb,
            bootloader,
            disks,
            nics,
        })
    }
}

fn strip_phy_prefix(s: &str) -> &str {
    s.strip_prefix("phy:").unwrap_or(s)
}

/// Map manifest's `storage.kind` string to the libxl format + backend
/// pair that can read it. Cloud images and instance overlays are qcow2,
/// which MUST go through the qemu-disk backend (the phy backend would
/// hand the qcow2 header to the guest as if it were the boot sector).
/// Bare block devices stay on the phy backend — lowest overhead, no
/// qemu-dm process. Anything we don't recognise defaults to qcow2 over
/// qdisk because guessing wrong toward "raw block" silently corrupts
/// data while guessing wrong toward "qemu file" just costs ~80 MB.
fn pick_format_backend(kind: &str) -> (DiskFormat, DiskBackend) {
    match kind {
        "block" | "raw-block" | "phy" =>
            (DiskFormat::Raw, DiskBackend::Phy),
        "raw" | "raw-file" =>
            (DiskFormat::Raw, DiskBackend::Qdisk),
        "qcow2" =>
            (DiskFormat::Qcow2, DiskBackend::Qdisk),
        "vhd" | "vhdx" =>
            (DiskFormat::Vhd, DiskBackend::Qdisk),
        _ =>
            (DiskFormat::Qcow2, DiskBackend::Qdisk),
    }
}

fn is_readonly_mode(mode: &str) -> bool {
    mode == "ro" || mode == "read-only" || mode.starts_with("ro-")
}

fn default_bridge_name() -> String {
    default_bridge_name_from(std::path::Path::new("/sys/class/net"))
}

fn default_bridge_name_from(sys_class_net: &std::path::Path) -> String {
    // Prefer the intended Xen bridge when present. Otherwise, fall back
    // to any host bridge device rather than pinning libxl to a bridge
    // name the host never created.
    let xenbr0 = sys_class_net.join("xenbr0/bridge");
    if xenbr0.exists() {
        return "xenbr0".into();
    }

    let mut names: Vec<String> = std::fs::read_dir(sys_class_net)
        .ok()
        .into_iter()
        .flat_map(|it| it.filter_map(|entry| entry.ok()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            entry.path().join("bridge").exists().then_some(name)
        })
        .collect();
    names.sort();
    names.into_iter().next().unwrap_or_else(|| "xenbr0".into())
}

// ---------------------------------------------------------------------------
// Owned libxl_domain_config — materialization, drops via libxl_domain_config_dispose

/// Owns a populated `libxl_domain_config` plus a borrowed `*libxl_ctx`
/// used only at drop time. The contained config remains valid until
/// `OwnedDomainConfig` drops; pass `raw_mut()` to `libxl_domain_create_new`.
pub struct OwnedDomainConfig {
    inner: sys::libxl_domain_config,
    /// Not owned. We just need a ctx to perform any libxl-context-bound
    /// operations during the build, but `libxl_domain_config_dispose`
    /// itself takes only the config pointer.
    _ctx: *mut sys::libxl_ctx,
}

impl OwnedDomainConfig {
    /// Materialize a [`DomainConfigPlan`] into a libxl-owned graph.
    ///
    /// # Safety
    ///
    /// `ctx` must be a valid `*libxl_ctx` returned by
    /// `libxl_ctx_alloc`. The returned `OwnedDomainConfig` borrows nothing
    /// from `ctx` for storage but uses it transiently for bitmap alloc.
    pub unsafe fn new(
        ctx: *mut sys::libxl_ctx,
        plan: &DomainConfigPlan,
    ) -> Result<Self> {
        // The entire body is unsafe ops on raw FFI; one umbrella block
        // keeps signatures clean. SAFETY arguments inline.
        unsafe {
            // Build the graph step-by-step; on any failure, dispose what we've
            // built and return the typed error. dispose is safe to call on an
            // init'd-but-partially-populated config because every sub-struct
            // we touch was init'd to a known-good state by libxl_*_init first.
            let mut dc = MaybeUninit::<sys::libxl_domain_config>::uninit();
            sys::libxl_domain_config_init(dc.as_mut_ptr());
            let mut dc = dc.assume_init();

            let xen_type = match plan.mode {
                XenDomainMode::Pv  => sys::libxl_domain_type::LIBXL_DOMAIN_TYPE_PV,
                XenDomainMode::Pvh => sys::libxl_domain_type::LIBXL_DOMAIN_TYPE_PVH,
                XenDomainMode::Hvm => sys::libxl_domain_type::LIBXL_DOMAIN_TYPE_HVM,
            };

            // ---- c_info ----
            compat::set_create_info_type(&mut dc.c_info, xen_type);
            sys::libxl_uuid_generate(&mut dc.c_info.uuid);
            dc.c_info.name = match strdup(&plan.name) {
                Ok(p) => p,
                Err(e) => { sys::libxl_domain_config_dispose(&mut dc); return Err(e); }
            };

            // ---- b_info: must be init'd with the chosen type so the union
            // is in a consistent state. init_type sets type_; we mirror via
            // compat for paranoia and so callers stay symmetric.
            sys::libxl_domain_build_info_init_type(&mut dc.b_info, xen_type);
            compat::set_build_info_type(&mut dc.b_info, xen_type);
            dc.b_info.max_vcpus = plan.max_vcpus as i32;
            dc.b_info.target_memkb = plan.target_memkb;
            dc.b_info.max_memkb = plan.max_memkb;

            // avail_vcpus bitmap — every vcpu starts available; pinning
            // happens later via pin_vcpus().
            let rc = sys::libxl_bitmap_alloc(
                ctx, &mut dc.b_info.avail_vcpus, plan.max_vcpus as i32);
            if rc != 0 {
                sys::libxl_domain_config_dispose(&mut dc);
                return Err(map_rc(rc, "libxl_bitmap_alloc avail_vcpus"));
            }
            for i in 0..plan.max_vcpus {
                sys::libxl_bitmap_set(&mut dc.b_info.avail_vcpus, i as i32);
            }

            // bootloader (PV / PVH only — HVM uses firmware)
            if let Some(bl) = &plan.bootloader {
                dc.b_info.bootloader = match strdup(bl) {
                    Ok(p) => p,
                    Err(e) => { sys::libxl_domain_config_dispose(&mut dc); return Err(e); }
                };
            }

            // ---- disks ----
            if !plan.disks.is_empty() {
                let n = plan.disks.len();
                let arr = libc::calloc(n, std::mem::size_of::<sys::libxl_device_disk>())
                    as *mut sys::libxl_device_disk;
                if arr.is_null() {
                    sys::libxl_domain_config_dispose(&mut dc);
                    return Err(BackendError::insufficient_resources("calloc disks"));
                }
                for (i, d) in plan.disks.iter().enumerate() {
                    let slot = arr.add(i);
                    sys::libxl_device_disk_init(slot);
                    let s = &mut *slot;
                    s.pdev_path = match strdup(&d.source) {
                        Ok(p) => p,
                        Err(e) => {
                            // partial array — hand it to libxl so dispose frees what we made
                            dc.disks = arr;
                            dc.num_disks = (i + 1) as i32;
                            sys::libxl_domain_config_dispose(&mut dc);
                            return Err(e);
                        }
                    };
                    s.vdev = match strdup(&d.vdev) {
                        Ok(p) => p,
                        Err(e) => {
                            dc.disks = arr;
                            dc.num_disks = (i + 1) as i32;
                            sys::libxl_domain_config_dispose(&mut dc);
                            return Err(e);
                        }
                    };
                    s.backend = match d.backend {
                        DiskBackend::Phy => sys::libxl_disk_backend::LIBXL_DISK_BACKEND_PHY,
                        DiskBackend::Qdisk => sys::libxl_disk_backend::LIBXL_DISK_BACKEND_QDISK,
                    };
                    s.format = match d.format {
                        DiskFormat::Raw => sys::libxl_disk_format::LIBXL_DISK_FORMAT_RAW,
                        DiskFormat::Qcow2 => sys::libxl_disk_format::LIBXL_DISK_FORMAT_QCOW2,
                        DiskFormat::Vhd => sys::libxl_disk_format::LIBXL_DISK_FORMAT_VHD,
                    };
                    s.readwrite = if d.readwrite { 1 } else { 0 };
                    s.is_cdrom = 0;
                }
                dc.disks = arr;
                dc.num_disks = n as i32;
            }

            // ---- nics ----
            if !plan.nics.is_empty() {
                let n = plan.nics.len();
                let arr = libc::calloc(n, std::mem::size_of::<sys::libxl_device_nic>())
                    as *mut sys::libxl_device_nic;
                if arr.is_null() {
                    sys::libxl_domain_config_dispose(&mut dc);
                    return Err(BackendError::insufficient_resources("calloc nics"));
                }
                for (i, nic) in plan.nics.iter().enumerate() {
                    let slot = arr.add(i);
                    sys::libxl_device_nic_init(slot);
                    let s = &mut *slot;
                    s.bridge = match strdup(&nic.bridge) {
                        Ok(p) => p,
                        Err(e) => {
                            dc.nics = arr;
                            dc.num_nics = (i + 1) as i32;
                            sys::libxl_domain_config_dispose(&mut dc);
                            return Err(e);
                        }
                    };
                    s.nictype = if nic.nictype_is_ioemu {
                        sys::libxl_nic_type::LIBXL_NIC_TYPE_VIF_IOEMU
                    } else {
                        sys::libxl_nic_type::LIBXL_NIC_TYPE_VIF
                    };
                }
                dc.nics = arr;
                dc.num_nics = n as i32;
            }

            Ok(OwnedDomainConfig { inner: dc, _ctx: ctx })
        }
    }

    pub fn raw_mut(&mut self) -> *mut sys::libxl_domain_config {
        &mut self.inner
    }
}

impl Drop for OwnedDomainConfig {
    fn drop(&mut self) {
        // libxl walks the device arrays, calls each *_dispose on every
        // element, then frees both the arrays and the strings inside
        // c_info / b_info. We allocated everything via libc::calloc /
        // libc::strdup which is what libxl expects.
        unsafe { sys::libxl_domain_config_dispose(&mut self.inner); }
    }
}

// ---------------------------------------------------------------------------
// strdup helper

fn strdup(s: &str) -> Result<*mut c_char> {
    let cs = CString::new(s).map_err(|_| BackendError::internal(
        format!("string contains interior NUL: {s:?}")))?;
    let p = unsafe { libc::strdup(cs.as_ptr()) };
    if p.is_null() {
        return Err(BackendError::insufficient_resources("strdup"));
    }
    Ok(p)
}

#[allow(dead_code)]
fn null_cstring() -> *mut c_char { ptr::null_mut() }

// ---------------------------------------------------------------------------
// Tests — only the pure DomainConfigPlan layer; OwnedDomainConfig requires
// real libxl_ctx and is exercised end-to-end on a Xen host.

#[cfg(test)]
mod tests {
    use super::*;
    use rotten_apple_manifest::Profile;

    fn load(toml_str: &str) -> Profile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), toml_str).unwrap();
        Profile::load(f.path()).unwrap()
    }

    fn ubuntu_profile() -> Profile {
        load(r#"
            [profile]
            name = "ubuntu-domu"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "8G"
            memory_idle = "4G"
            memory_minimum = "2G"
            vcpus_active = 4
            vcpus_idle = 2
            vcpus_minimum = 1
            [storage]
            root = { kind = "block", source = "/dev/nvme0n1", mode = "rw-exclusive" }
            [[network.interfaces]]
            name = "primary"
            mac = "auto"
            egress = "any"
            [gpu]
            mode = "passthrough"
            device = "0000:00:02.0"
            [tpm]
            mode = "none"
            [autostart]
        "#)
    }

    fn windows_profile() -> Profile {
        load(r#"
            [profile]
            name = "win-appliance"
            type = "appliance"
            [meta]
            [resources]
            memory_active = "4G"
            memory_idle = "2G"
            memory_minimum = "1G"
            vcpus_active = 2
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", source = "/var/lib/x.qcow2", mode = "rw-exclusive" }
            [[network.interfaces]]
            name = "primary"
            mac = "auto"
            egress = "any"
            [gpu]
            mode = "virtual"
            [tpm]
            mode = "swtpm"
            [autostart]
        "#)
    }

    #[test]
    fn plan_pvh_has_pygrub_and_paravirt_nic() {
        let p = ubuntu_profile();
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Pvh).unwrap();
        assert_eq!(plan.bootloader.as_deref(), Some("pygrub"));
        assert_eq!(plan.nics.len(), 1);
        assert!(!plan.nics[0].nictype_is_ioemu);
    }

    #[test]
    fn plan_hvm_has_no_bootloader_and_emulated_nic() {
        let p = windows_profile();
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Hvm).unwrap();
        assert_eq!(plan.bootloader, None);
        assert_eq!(plan.nics.len(), 1);
        assert!(plan.nics[0].nictype_is_ioemu);
    }

    #[test]
    fn plan_disk_strips_phy_prefix() {
        // build a profile inline with an explicit phy: source
        let p = load(r#"
            [profile]
            name = "x"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "1G"
            memory_idle = "1G"
            memory_minimum = "512M"
            vcpus_active = 1
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "block", source = "phy:/dev/sda", mode = "rw-exclusive" }
            [tpm]
            mode = "none"
            [autostart]
        "#);
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Pvh).unwrap();
        assert_eq!(plan.disks[0].source, "/dev/sda",
                   "phy: prefix should be stripped — libxl wants the bare path");
        assert_eq!(plan.disks[0].vdev, "xvda");
        assert!(plan.disks[0].readwrite);
        // kind="block" must map to the phy backend with raw format —
        // anything else corrupts data on a real block device.
        assert_eq!(plan.disks[0].backend, DiskBackend::Phy);
        assert_eq!(plan.disks[0].format, DiskFormat::Raw);
    }

    #[test]
    fn plan_qcow2_disk_uses_qdisk_backend() {
        // Cloud images and instance overlays are qcow2. They MUST go
        // through the qemu-disk backend; the phy backend would feed the
        // qcow2 header to the guest as if it were the boot sector and
        // the domain would never get past initial vcpu execution.
        let p = load(r#"
            [profile]
            name = "x"
            type = "appliance"
            [meta]
            [resources]
            memory_active = "2G"
            memory_idle = "1G"
            memory_minimum = "512M"
            vcpus_active = 1
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/var/lib/x.qcow2", mode = "rw-exclusive" }
            [tpm]
            mode = "none"
            [autostart]
        "#);
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Hvm).unwrap();
        assert_eq!(plan.disks[0].backend, DiskBackend::Qdisk,
            "qcow2 disks must use the qemu-disk backend");
        assert_eq!(plan.disks[0].format, DiskFormat::Qcow2);
    }

    #[test]
    fn pick_format_backend_known_kinds() {
        assert_eq!(pick_format_backend("block"),  (DiskFormat::Raw,   DiskBackend::Phy));
        assert_eq!(pick_format_backend("phy"),    (DiskFormat::Raw,   DiskBackend::Phy));
        assert_eq!(pick_format_backend("raw"),    (DiskFormat::Raw,   DiskBackend::Qdisk));
        assert_eq!(pick_format_backend("qcow2"),  (DiskFormat::Qcow2, DiskBackend::Qdisk));
        assert_eq!(pick_format_backend("vhd"),    (DiskFormat::Vhd,   DiskBackend::Qdisk));
        assert_eq!(pick_format_backend("vhdx"),   (DiskFormat::Vhd,   DiskBackend::Qdisk));
    }

    #[test]
    fn pick_format_backend_unknown_defaults_to_qdisk_qcow2() {
        // Defensive default: phy+raw on the wrong kind silently corrupts
        // a real block device. qdisk+qcow2 just fails to attach if the
        // file isn't qcow2 — recoverable, not destructive.
        assert_eq!(pick_format_backend("vmdk"), (DiskFormat::Qcow2, DiskBackend::Qdisk));
        assert_eq!(pick_format_backend(""),     (DiskFormat::Qcow2, DiskBackend::Qdisk));
    }

    #[test]
    fn plan_extra_disks_get_sequential_vdevs() {
        let p = load(r#"
            [profile]
            name = "x"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "1G"
            memory_idle = "1G"
            memory_minimum = "512M"
            vcpus_active = 1
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "block", source = "/dev/nvme0n1", mode = "rw-exclusive" }
            extra_disks = [
                { kind = "block", source = "/dev/sdb", mode = "rw-exclusive" },
                { kind = "block", source = "/dev/sdc", mode = "ro" },
            ]
            [tpm]
            mode = "none"
            [autostart]
        "#);
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Pvh).unwrap();
        assert_eq!(plan.disks.len(), 3);
        assert_eq!(plan.disks[0].vdev, "xvda");
        assert_eq!(plan.disks[1].vdev, "xvdb");
        assert_eq!(plan.disks[2].vdev, "xvdc");
        assert!(plan.disks[1].readwrite);
        assert!(!plan.disks[2].readwrite, "ro should map to readwrite=false");
    }

    #[test]
    fn plan_uses_active_resources_for_target() {
        let p = ubuntu_profile();
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Pvh).unwrap();
        assert_eq!(plan.max_vcpus, 4);
        // 8 GiB = 8388608 KiB
        assert_eq!(plan.target_memkb, 8 * 1024 * 1024);
        assert_eq!(plan.max_memkb, plan.target_memkb,
                   "we balloon down only — max == target");
    }

    #[test]
    fn plan_path_falls_back_when_source_absent() {
        let p = load(r#"
            [profile]
            name = "x"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "1G"
            memory_idle = "1G"
            memory_minimum = "512M"
            vcpus_active = 1
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/tmp/x.qcow2", mode = "rw-exclusive" }
            [tpm]
            mode = "none"
            [autostart]
        "#);
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Hvm).unwrap();
        assert_eq!(plan.disks.len(), 1);
        assert_eq!(plan.disks[0].source, "/tmp/x.qcow2");
    }

    #[test]
    fn plan_name_passed_through() {
        let p = ubuntu_profile();
        let plan = DomainConfigPlan::from_profile(&p, XenDomainMode::Pvh).unwrap();
        assert_eq!(plan.name, "ubuntu-domu");
    }

    #[test]
    fn default_bridge_prefers_xenbr0_when_present() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("xenbr0/bridge")).unwrap();
        std::fs::create_dir_all(root.path().join("virbr0/bridge")).unwrap();
        assert_eq!(default_bridge_name_from(root.path()), "xenbr0");
    }

    #[test]
    fn default_bridge_falls_back_to_first_detected_bridge() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("virbr0/bridge")).unwrap();
        assert_eq!(default_bridge_name_from(root.path()), "virbr0");
    }
}
