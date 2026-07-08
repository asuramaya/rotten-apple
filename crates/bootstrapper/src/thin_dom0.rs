//! ThinDom0 install plan — v0.0.7 destination shape.
//!
//! Architectural decision (2026-05-06, user-explicit, supersedes the
//! v0.0.1 "lift Ubuntu into dom0" expedient documented at lib.rs:1):
//!
//! Target shape is the canonical Xen layout: a thin dom0 (E-cores,
//! 2-4 GB, no DE, just busybox + Xen tooling + rotten-apple) that
//! orchestrates everything-as-guest, including the user's own Ubuntu
//! desktop. The v0.0.1 lift made the user's existing Ubuntu install
//! BECOME dom0; that gave us a working orchestrator end-to-end against
//! real Xen, but it has architectural problems:
//!
//!   1. dom0-as-desktop holds RAM hostage from the balloon driver.
//!   2. The iGPU is welded to dom0, so it can't be leased to a SteamOS
//!      guest.
//!   3. Any browser tab in dom0 has hypervisor privileges.
//!
//! The thin shape solves all three.
//!
//! NON-DESTRUCTIVE BY CONSTRUCTION (sacred Ubuntu image rule):
//!
//! The user's existing Ubuntu install must remain bootable as bare
//! metal at all times. The bootstrapper writes a separate dom0 image
//! to a NEW location and adds a NEW GRUB entry. It never modifies the
//! user's rootfs partition, fstab, kernel, or initramfs. The bare-metal
//! Ubuntu menuentry stays untouched (we verify this post-update-grub).
//!
//! Layout:
//!
//!   /boot/rotten-apple/vmlinuz                     ← dom0 kernel (copy of host's)
//!   /boot/rotten-apple/thin-dom0.cpio.gz           ← dom0 rootfs (lives in RAM at boot)
//!   /etc/default/grub.d/41-rotten-apple-thindom0.cfg
//!     └─ adds a custom menuentry that boots Xen + the above kernel +
//!        the cpio.gz as the dom0 rootfs.
//!   /var/lib/rotten-apple/                         ← persistent state, bind-mounted into dom0
//!   /etc/rotten-apple/user-desktop.toml            ← guest manifest auto-launched by cockpit
//!
//! Boot picture:
//!
//! ```text
//! GRUB
//!  |- Ubuntu (bare metal - sacred, untouched)
//!  `- rotten-apple ThinDom0
//!      -> Xen
//!      -> dom0 (initrd-only Linux, ~150 MB in RAM, our cockpit on tty1)
//!      -> cockpit can start the user-desktop guest:
//!          * disk = host's existing root partition (PHY backend, RAW format)
//!          * the user's existing kernel + initramfs + GNOME boots inside
//!          * display path = PV framebuffer for v0.1; Looking Glass later
//! ```
//!
//! This module owns the planning data structure and a planner that
//! derives it from the host. Actual rootfs build, GRUB write, and
//! manifest generation come in sibling modules (rootfs.rs, grub.rs,
//! user_desktop.rs — landing as separate commits). The planner is
//! testable today against fake mountinfo; real-host fields go through
//! commands (uname, blkid) which are split out so tests don't hit them.

use std::path::{Path, PathBuf};

use rotten_apple_detect::{CpuTopology, Detection};

use crate::thin_dom0_efi::{
    EspStageInputs, ensure_nvram_entry, esp_mount_device_from_mountinfo, render_xen_cfg,
    stage_esp,
};
use crate::thin_dom0_grub::{
    self, GrubEntryInputs, boot_mount_device_from_mountinfo, discover_xen_image_under,
};
use crate::thin_dom0_manifest::{UserDesktopInputs, UserDesktopDisplayMode, render_user_desktop_manifest};
use crate::thin_dom0_rootfs::{RootfsSpec, build_rootfs};

/// Hard-capped dom0 sizing for ThinDom0. The dom0 runs busybox +
/// xen-tools + the rotten-apple cockpit; idle footprint is still
/// small and one E-core is enough. The large allocation goes to the
/// user-desktop guest (THINDOM0_USER_DESKTOP_MEM_MB), not dom0 —
/// dom0 is the thin orchestrator, not a workload.
///
/// The catch is boot-time staging: the current ThinDom0 image carries
/// the full host /lib/modules tree, which makes the compressed initrd
/// ~250 MB and the loaded initrd region ~400 MB on this laptop. Xen
/// must hold the compressed blob, the unpacked tmpfs rootfs, and the
/// kernel/runtime overhead at the same time. 1 GiB failed in practice
/// with `Initramfs unpacking failed: write error` followed by
/// `No working init found` (2026-05-15 nested repro against the real
/// staged image). 2 GiB then also failed once firmware bundling grew the
/// cpio to ~264 MiB compressed / ~413 MiB unpacked (23k files): the SAME
/// `Initramfs unpacking failed: write error` → dom0 panics BEFORE /init,
/// no log, black screen — this was the multi-week "ThinDom0 won't boot"
/// blocker, misread as a framebuffer fault. QEMU-sim differential
/// (2026-06-12): 2048 panics, 3072 unpacks and reaches `xenstore ready=1`
/// + cockpit handoff. 4096 gives headroom over that floor so a little
/// more firmware/modules can't re-cross it. The 1 GiB steady-state ideal
/// ([[feedback_dom0_sizing]]) is a STEADY-STATE target; this is the
/// transient unpack peak — balloon dom0 down post-boot via `xl mem-set`.
pub const THINDOM0_DOM0_MEM_MB: u64 = 4096;
pub const THINDOM0_DOM0_VCPUS: u32 = 1;

#[derive(Debug, Clone)]
pub struct ThinDom0Plan {
    /// Where the dom0 kernel will be installed on the host.
    pub kernel_path: PathBuf,
    /// Where the dom0 initrd (= the entire dom0 rootfs as a cpio.gz)
    /// will be installed.
    pub initrd_path: PathBuf,
    /// Source kernel to copy into `kernel_path`. Defaults to the
    /// running kernel (`/boot/vmlinuz-$(uname -r)`).
    pub kernel_source: PathBuf,
    /// dom0 sizing — hard-capped to `THINDOM0_DOM0_*` constants. The
    /// orchestrator workload doesn't grow with host RAM, so we don't
    /// follow the v0.0.1 planner here.
    pub dom0_mem_mb: u64,
    pub dom0_vcpus: u32,
    /// Physical CPU index to pin dom0 to. On hybrid CPUs this is the
    /// first E-core (so dom0 never steals P-core cycles from the
    /// user-desktop guest). None on uniform CPUs (xen will round-
    /// robin, which is fine for a 1-vcpu dom0).
    pub dom0_pinned_cpu: Option<u32>,
    /// The user's existing root device — the LV / partition / LUKS
    /// node that holds the rootfs. Identifies "what to mount as /" for
    /// display purposes; the actual passthrough goes through
    /// `user_root_disk` so pygrub can read the guest's /boot from
    /// neighbouring partitions.
    pub user_root_device: PathBuf,
    /// UUID of `user_root_device`. Used to lock the GRUB entry to the
    /// right partition by-uuid (so it survives device renumbering).
    pub user_root_uuid: String,
    /// Parent block device (whole disk) backing `user_root_device`.
    /// e.g. /dev/nvme1n1 for an LV→LUKS→partition→disk chain. The
    /// user-desktop guest receives THIS through xen-blkback so pygrub
    /// can find /boot on a sibling partition. dom0 also reads this at
    /// install time to discover the boot/EFI partitions.
    pub user_root_disk: PathBuf,
    /// Filesystem type of the user's root (`ext4`, `btrfs`, `xfs`, …).
    /// Surfaced in the plan summary so the user sees what the guest
    /// will be booting against; not used for anything else yet.
    pub user_root_fstype: String,
    /// Where /etc/rotten-apple/ lives on the host. The dom0 mounts a
    /// bind from here so persistent state (manifests, lease registry,
    /// audit log) survives reboots.
    pub persistent_state_dir: PathBuf,
    /// GRUB menuentry name. Defaults to "rotten-apple ThinDom0".
    pub grub_entry_name: String,
    /// True when /boot is a separate filesystem from /. Drives whether
    /// GRUB paths get a "/boot" prefix and whether the boot UUID
    /// differs from the rootfs UUID.
    pub has_separate_boot: bool,
    /// UUID of the partition GRUB will `search --set=root` for. Equal
    /// to `user_root_uuid` when /boot is on /, or to the /boot
    /// partition's UUID otherwise.
    pub boot_fs_uuid: String,
    /// Filename (no directory) of the Xen image discovered in /boot,
    /// e.g. "xen-4.20-amd64.gz". Bare-metal xen-system-amd64 drops it.
    pub xen_image_basename: PathBuf,
    /// PCI BDF of the framebuffer GPU (the one driving fb0 — the
    /// iGPU on this user's Optimus laptop). Passed to the user-desktop
    /// guest manifest as the GPU to passthrough so the daily driver
    /// gets native graphics. None when no framebuffer GPU is detected
    /// (headless server) — the guest then falls back to PV.
    pub framebuffer_gpu_bdf: Option<String>,
    /// ESP block device (e.g. /dev/nvme0n1p1) when the host is UEFI
    /// AND a xen-*.efi image exists in /boot. Drives the native-EFI
    /// boot path (thin_dom0_efi): ESP staging + chainload GRUB entries
    /// + NVRAM entry. None on BIOS hosts or when the .efi is missing —
    /// GRUB then keeps the classic multiboot2 entries (which only
    /// break on GRUB 2.14 *EFI*; see thin_dom0_efi module docs).
    pub esp_device: Option<PathBuf>,
    /// vfat UUID of `esp_device` (e.g. "B6D5-2FF2"), for the GRUB
    /// chainload entries' `search --set=root`.
    pub esp_fs_uuid: Option<String>,
    /// If true, executors print steps without running them.
    pub dry_run: bool,
}

#[derive(Debug)]
pub enum ThinDom0Error {
    PreFlight(String),
}

impl std::fmt::Display for ThinDom0Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThinDom0Error::PreFlight(s) => write!(f, "pre-flight failed: {s}"),
        }
    }
}

impl std::error::Error for ThinDom0Error {}

pub type Result<T> = std::result::Result<T, ThinDom0Error>;

impl ThinDom0Plan {
    /// Probe the host and produce a plan. Reads `/proc/self/mountinfo`
    /// to find the user's root device, runs `uname -r` and `blkid` to
    /// fill the rest. Pure for the parts that don't need root.
    pub fn for_this_host(dry_run: bool) -> Result<Self> {
        let detection = Detection::run();
        let topo = CpuTopology::probe();
        let _ = rotten_apple_detect::plan(&detection, &topo);

        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|e| ThinDom0Error::PreFlight(
                format!("read /proc/self/mountinfo: {e}")))?;
        let root = parse_user_root_from_mountinfo(&mountinfo)?;

        let uuid = uuid_for_device(&root.device)?;
        let fstype = root.fstype.clone();

        let release = uname_release()?;
        let kernel_source = PathBuf::from(format!("/boot/vmlinuz-{release}"));
        if !kernel_source.exists() {
            return Err(ThinDom0Error::PreFlight(format!(
                "kernel image {} not found — is /boot mounted? \
                 (uname -r returned {release:?})",
                kernel_source.display())));
        }

        // /boot may or may not be a separate partition. When separate,
        // GRUB scripts use partition-relative paths (no "/boot" prefix)
        // and search for the /boot partition's UUID. When /boot is on
        // /, paths keep the "/boot" prefix and search uses the rootfs
        // UUID.
        let boot_dev = boot_mount_device_from_mountinfo(&mountinfo);
        let (has_separate_boot, boot_fs_uuid) = match boot_dev {
            Some(dev) => (true, uuid_for_device(&dev)?),
            None      => (false, uuid.clone()),
        };

        // Resolve to a stable /dev/disk/by-id/ path so the guest manifest
        // doesn't break when NVMe controller enumeration order changes
        // between reboots (which CAN swap nvme0n1 ↔ nvme1n1 on multi-
        // controller hosts — Linux + Windows split across two NVMe disks
        // is the canonical case here). Falls back to canonical path if
        // by-id isn't populated for some reason.
        let user_root_disk = stable_disk_path_for(&parent_disk_for(&root.device)?);

        let xen_image_basename = discover_xen_image_under(Path::new("/boot"))
            .ok_or_else(|| ThinDom0Error::PreFlight(
                "no xen-*.gz image found in /boot — is xen-system-amd64 \
                 installed? Run `apt install xen-system-amd64` first or \
                 `rotten-apple lift --execute` (the v0.0.1 lift handles \
                 the apt install for you).".to_string()))?;

        // Native-EFI boot path (thin_dom0_efi): needs BOTH a mounted
        // ESP and the packaged xen-*.efi sibling of the .gz. Either
        // missing → BIOS-style multiboot2 entries only. The .efi check
        // happens HERE (plan time) so grub_entry_inputs and execute()
        // can't disagree about which shape we're in.
        let esp_device = esp_mount_device_from_mountinfo(&mountinfo)
            .filter(|_| Path::new("/boot")
                .join(xen_image_basename.with_extension("efi")).exists());
        let esp_fs_uuid = match &esp_device {
            Some(dev) => Some(uuid_for_device(dev)?),
            None => None,
        };

        // ThinDom0 hard-caps: the dom0 is busybox + xen-tools + cockpit;
        // the planner sizes for "Ubuntu desktop in dom0" which needs
        // far more. Memory is floored at 2 GiB because the current
        // initrd no longer unpacks reliably under a 1 GiB cap.
        // On hybrid CPUs we pin to the first E-core so dom0 never
        // contends with the user-desktop guest's P-cores.
        let dom0_mem_mb = THINDOM0_DOM0_MEM_MB;
        let dom0_vcpus = THINDOM0_DOM0_VCPUS;
        // Pinning is meaningless if we're already inside a Xen domain
        // — /sys/devices/cpu_core/cpus reflects dom0's pinned view,
        // not bare-metal physical CPUs, so picking "first E-core"
        // here would lie. Skip the pin and tell the user.
        let dom0_pinned_cpu = if is_running_under_xen() {
            None
        } else {
            topo.e_cores.first().copied()
        };

        Ok(ThinDom0Plan {
            kernel_path: PathBuf::from("/boot/rotten-apple/vmlinuz"),
            initrd_path: PathBuf::from("/boot/rotten-apple/thin-dom0.cpio.gz"),
            kernel_source,
            dom0_mem_mb,
            dom0_vcpus,
            dom0_pinned_cpu,
            user_root_device: root.device,
            user_root_uuid: uuid,
            user_root_fstype: fstype,
            user_root_disk,
            persistent_state_dir: PathBuf::from("/var/lib/rotten-apple"),
            grub_entry_name: "rotten-apple ThinDom0".to_string(),
            has_separate_boot,
            boot_fs_uuid,
            xen_image_basename,
            framebuffer_gpu_bdf: framebuffer_gpu_bdf(),
            esp_device,
            esp_fs_uuid,
            dry_run,
        })
    }

    /// Build the inputs to `thin_dom0_grub::render_grub_script` from
    /// this plan. Computes partition-relative paths based on whether
    /// /boot is a separate filesystem.
    pub fn grub_entry_inputs(&self) -> GrubEntryInputs {
        // When /boot is a separate fs, paths in the GRUB entry are
        // relative to the /boot fs root (so "rotten-apple/vmlinuz",
        // not "boot/rotten-apple/vmlinuz"). When /boot is on /, they
        // need the "boot/" prefix to anchor at the rootfs.
        let strip_boot = |p: &Path| -> PathBuf {
            let s = p.to_string_lossy();
            let trimmed = s.trim_start_matches('/');
            match trimmed.strip_prefix("boot/") {
                Some(rest) if self.has_separate_boot => PathBuf::from(rest),
                _ => PathBuf::from(trimmed),
            }
        };
        let xen_image = if self.has_separate_boot {
            // xen-system-amd64 drops the .gz directly under /boot, so
            // partition-relative is just the basename.
            self.xen_image_basename.clone()
        } else {
            let mut p = PathBuf::from("boot");
            p.push(&self.xen_image_basename);
            p
        };
        GrubEntryInputs {
            entry_name: self.grub_entry_name.clone(),
            boot_fs_uuid: self.boot_fs_uuid.clone(),
            xen_image,
            dom0_kernel: strip_boot(&self.kernel_path),
            dom0_initrd: strip_boot(&self.initrd_path),
            dom0_mem_mb: self.dom0_mem_mb,
            dom0_vcpus: self.dom0_vcpus,
            user_root_uuid: self.user_root_uuid.clone(),
            dom0_pinned_cpu: self.dom0_pinned_cpu,
            framebuffer_gpu_bdf: self.framebuffer_gpu_bdf.clone(),
            // ParavirtOnly default: dom0 keeps the framebuffer GPU so cockpit
            // is visible on the laptop panel. Passthrough (lease the GPU to
            // the guest) is a later explicit step. See [[display_model]].
            display_mode: crate::thin_dom0_manifest::UserDesktopDisplayMode::ParavirtOnly,
            esp_fs_uuid: self.esp_fs_uuid.clone(),
        }
    }

    /// Render the executable shell script that the install will write
    /// to /etc/grub.d/41_rotten_apple_thindom0. Pure — no I/O.
    pub fn render_grub_script(&self) -> String {
        thin_dom0_grub::render_grub_script(&self.grub_entry_inputs())
    }

    /// Recovery path: skip apt + rootfs build + kernel copy. Only
    /// rewrite the GRUB script + run update-grub + verify both
    /// menuentries are present. Used when a previous `execute()` got
    /// far enough to land the cpio.gz but the GRUB step bombed (e.g.
    /// a renderer bug shipped a script that update-grub rejected).
    /// `execute()` is the default install path; this is for surgical
    /// recovery only.
    pub fn execute_grub_only(&self) -> Result<()> {
        eprintln!("==> rotten-apple ThinDom0 install: GRUB-ONLY recovery");
        eprintln!("    Assumes a prior `--execute` already wrote the cpio.gz,");
        eprintln!("    kernel, and host-side manifest. Only the GRUB step is");
        eprintln!("    redone. For a clean install, drop --grub-only.");
        eprintln!();

        let cpio = &self.initrd_path;
        let kernel = &self.kernel_path;
        if !cpio.exists() {
            return Err(ThinDom0Error::PreFlight(format!(
                "{} missing — run `sudo rotten-apple lift --execute` (without \
                 --grub-only) first to build the cpio.gz.",
                cpio.display())));
        }
        if !kernel.exists() {
            return Err(ThinDom0Error::PreFlight(format!(
                "{} missing — kernel was never copied. Run \
                 `sudo rotten-apple lift --execute` first.", kernel.display())));
        }
        let cpio_kb = std::fs::metadata(cpio).map(|m| m.len() / 1024).unwrap_or(0);
        eprintln!("    [verify cpio]   {} ({} KB) — OK",  cpio.display(),  cpio_kb);
        eprintln!("    [verify kernel] {} — OK", kernel.display());

        let grub_script_path = Path::new("/etc/grub.d/41_rotten_apple_thindom0");
        let grub_script = self.render_grub_script();
        if self.dry_run {
            eprintln!("    [grub script] would write {} ({} bytes, chmod 0755)",
                      grub_script_path.display(), grub_script.len());
            eprintln!("    [update-grub] would run update-grub");
            return Ok(());
        }
        std::fs::write(grub_script_path, &grub_script).map_err(|e|
            ThinDom0Error::PreFlight(format!(
                "write {}: {e}", grub_script_path.display())))?;
        chmod_executable(grub_script_path)?;
        eprintln!("    [grub script] → {}", grub_script_path.display());

        let out = std::process::Command::new("update-grub").output()
            .map_err(|e| ThinDom0Error::PreFlight(
                format!("spawn update-grub: {e}")))?;
        if !out.status.success() {
            return Err(ThinDom0Error::PreFlight(format!(
                "update-grub exit={}: {}",
                out.status, String::from_utf8_lossy(&out.stderr))));
        }
        eprintln!("    [update-grub] OK");

        verify_grub_dual_entry(&self.grub_entry_name)?;

        eprintln!();
        eprintln!("==> GRUB-only recovery complete.");
        eprintln!("    GRUB entries:");
        eprintln!("      · 'Ubuntu' (bare metal, untouched, default)");
        eprintln!("      · {:?} (new)", self.grub_entry_name);
        eprintln!("    Reboot and pick the ThinDom0 entry to boot into it.");
        Ok(())
    }

    /// Execute the install. Replaces the v0.0.1 lift end-to-end:
    ///   1. Build the dom0 cpio.gz (mmdebstrap + bake binary + /init).
    ///   2. Copy the host kernel to /boot/rotten-apple/vmlinuz.
    ///   3. Write the user-desktop guest manifest under /etc/rotten-apple/.
    ///   4. Drop /etc/grub.d/41_rotten_apple_thindom0, chmod +x.
    ///   5. update-grub.
    ///   6. Verify bare-metal Ubuntu entry survived AND new ThinDom0
    ///      entry is present.
    ///
    /// Bare-metal Ubuntu stays the GRUB default — user can always
    /// boot back to stock by picking it.
    pub fn execute(&self) -> Result<()> {
        // Running from inside an existing Xen domain (the v0.0.1 lift)
        // is a RE-LIFT: install-thindom0 patches the host's GRUB to
        // add a ThinDom0 entry alongside the FatDom0 entry. Everything
        // works except E-core pinning — /sys inside a domain reports
        // dom0's vcpus, not bare-metal physical CPUs, so dom0_pinned_cpu
        // is None and /init skips the pin. User can fix post-reboot
        // with `xl vcpu-pin Domain-0 0 <N>` from cockpit.
        let xen_relift = !self.dry_run && is_running_under_xen();
        if xen_relift {
            eprintln!("==> rotten-apple ThinDom0 install: RE-LIFT from inside Xen");
            eprintln!("    The new ThinDom0 GRUB entry will land alongside the");
            eprintln!("    existing FatDom0 entry (which stays as a fallback).");
            eprintln!("    dom0 E-core pin is SKIPPED — /sys/devices/cpu_atom/cpus");
            eprintln!("    inside a domain reports vcpus, not bare-metal CPUs.");
            eprintln!("    After ThinDom0 boots, pin dom0 manually:");
            eprintln!("      xl vcpu-pin Domain-0 0 <e-core-cpu-index>");
            eprintln!();
        } else {
            eprintln!("==> rotten-apple ThinDom0 install: {}",
                      if self.dry_run { "DRY RUN" } else { "EXECUTE" });
        }
        eprintln!("    user root:        {}", self.user_root_device.display());
        eprintln!("    parent disk:      {} (handed to user-desktop guest)",
                  self.user_root_disk.display());
        eprintln!("    dom0 kernel:      {} -> {}",
                  self.kernel_source.display(), self.kernel_path.display());
        eprintln!("    dom0 initrd:      {}", self.initrd_path.display());
        eprintln!("    GRUB entry:       {:?}", self.grub_entry_name);
        eprintln!();

        // Step 0: ensure the rootfs builder's tools are installed. The
        // user wants "ready to reboot" — no surprise dependency walls
        // mid-install. apt-get install is idempotent and quiet.
        if !self.dry_run {
            apt_install_dependencies()?;
        } else {
            eprintln!("    [apt] would `apt-get install -y mmdebstrap cpio`");
        }

        // Step 1: build the cpio.gz. The framebuffer GPU was detected
        // at plan time; install pipes it through to the user-desktop
        // manifest so the daily driver gets native graphics.
        if let Some(bdf) = &self.framebuffer_gpu_bdf {
            eprintln!("    [gpu] iGPU at {bdf} → user-desktop guest passthrough");
        } else {
            eprintln!("    [gpu] no framebuffer GPU detected → guest falls back to PV");
        }
        // Production default is ParavirtOnly: dom0 keeps the iGPU so cockpit
        // stays visible on the laptop panel and the guest renders to a PV
        // display. Passthrough (guest takes the physical iGPU, dom0 goes
        // headless, reached over SSH) is the eventual daily-driver target —
        // flipped here once visible boot is solid. (Decision 2026-06-03.)
        let user_desktop_manifest = render_user_desktop_manifest(self, &UserDesktopInputs {
            gpu_bdf: self.framebuffer_gpu_bdf.clone(),
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: rotten_apple_manifest::TpmMode::None,
            autostart_enabled: true,
        });
        // Second profile baked alongside: an always-visible paravirtual view
        // for passphrase / LUKS bring-up. Never autostarted — cockpit offers
        // it as the manual fallback when the primary guest can't surface its
        // own console.
        let user_desktop_visible_manifest = render_user_desktop_manifest(self, &UserDesktopInputs {
            gpu_bdf: self.framebuffer_gpu_bdf.clone(),
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: rotten_apple_manifest::TpmMode::None,
            autostart_enabled: false,
        });
        let cli_binary = std::env::current_exe()
            .map_err(|e| ThinDom0Error::PreFlight(
                format!("could not locate own binary path: {e}")))?;
        let release = uname_release()?;
        let authorized_ssh_keys = collect_user_ssh_keys();
        if authorized_ssh_keys.is_empty() {
            eprintln!("    [ssh] WARNING — no SSH public keys found for invoking user. \
                      \n           dom0 will be unreachable while the user-desktop guest holds \
                      \n           the iGPU. Add a key to ~/.ssh/authorized_keys (or generate \
                      \n           one with ssh-keygen) and re-run, or accept the limitation.");
        } else {
            let n = authorized_ssh_keys.lines().filter(|l| !l.trim().is_empty()).count();
            eprintln!("    [ssh] {n} authorized key(s) baked → dom0 reachable as `ssh root@<dom0-ip>` from guest");
        }
        let spec = RootfsSpec {
            rotten_apple_binary: cli_binary,
            user_desktop_manifest: user_desktop_manifest.clone(),
            user_desktop_visible_manifest,
            framebuffer_gpu_driver: framebuffer_gpu_driver(),
            authorized_ssh_keys,
            output_cpio: self.initrd_path.clone(),
            workdir: PathBuf::from("/var/cache/rotten-apple/dom0-rootfs"),
            suite: host_ubuntu_codename().unwrap_or_else(|| "noble".to_string()),
            xen_version: xen_version_from_image(&self.xen_image_basename)
                .unwrap_or_else(|| "4.20".to_string()),
            mirror: "http://archive.ubuntu.com/ubuntu".to_string(),
            kernel_release: release.clone(),
        };
        build_rootfs(&spec, self.dry_run).map_err(|e| ThinDom0Error::PreFlight(
            format!("build rootfs: {e}")))?;

        // Step 2: copy host kernel.
        copy_kernel(&self.kernel_source, &self.kernel_path, self.dry_run)?;

        // Step 2.5: native-EFI boot path — stage xen.efi + kernel +
        // rootfs + xen.cfg onto the ESP and ensure the NVRAM entry.
        // Runs on EVERY lift so the ESP copies track /boot/rotten-apple
        // (the 2026-07-08 manual bring-up staging went stale on the
        // very next lift — never again). Skipped on BIOS hosts.
        if let Some(esp_dev) = &self.esp_device {
            let esp_inputs = EspStageInputs {
                esp_mount: PathBuf::from("/boot/efi"),
                xen_efi_source: Path::new("/boot")
                    .join(self.xen_image_basename.with_extension("efi")),
                kernel_source: self.kernel_path.clone(),
                initrd_source: self.initrd_path.clone(),
            };
            let xen_cfg = render_xen_cfg(&self.grub_entry_inputs());
            stage_esp(&esp_inputs, &xen_cfg, self.dry_run)
                .map_err(|e| ThinDom0Error::PreFlight(e.to_string()))?;
            match partition_number_of(esp_dev) {
                Some(part) => {
                    let disk = parent_disk_for(esp_dev)?;
                    ensure_nvram_entry(&disk, part, self.dry_run);
                }
                None => eprintln!(
                    "    [nvram] cannot parse partition number from {} — \
                     skipping NVRAM entry (GRUB chainload entries still work)",
                    esp_dev.display()),
            }
        } else {
            eprintln!("    [esp] no ESP + xen-*.efi pair detected — BIOS-style \
                       multiboot2 GRUB entries only");
        }

        // Step 3: persist the user-desktop manifest on host (in addition to
        // baking it into the cpio). Lets the user `cat /etc/rotten-apple/...`
        // from their daily desktop without booting ThinDom0 — faster review.
        let host_manifest = Path::new("/etc/rotten-apple/user-desktop.toml");
        if self.dry_run {
            eprintln!("    [bake host manifest] would write {} ({} bytes)",
                      host_manifest.display(), user_desktop_manifest.len());
        } else {
            if let Some(parent) = host_manifest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| ThinDom0Error::PreFlight(
                    format!("mkdir {}: {e}", parent.display())))?;
            }
            std::fs::write(host_manifest, &user_desktop_manifest).map_err(|e|
                ThinDom0Error::PreFlight(
                    format!("write {}: {e}", host_manifest.display())))?;
            eprintln!("    [bake host manifest] → {}", host_manifest.display());
        }

        // Step 4: GRUB script.
        let grub_script_path = Path::new("/etc/grub.d/41_rotten_apple_thindom0");
        let grub_script = self.render_grub_script();
        if self.dry_run {
            eprintln!("    [grub script] would write {} ({} bytes, chmod 0755)",
                      grub_script_path.display(), grub_script.len());
        } else {
            std::fs::write(grub_script_path, &grub_script).map_err(|e|
                ThinDom0Error::PreFlight(
                    format!("write {}: {e}", grub_script_path.display())))?;
            chmod_executable(grub_script_path)?;
            eprintln!("    [grub script] → {}", grub_script_path.display());
        }

        // Step 5: update-grub.
        if self.dry_run {
            eprintln!("    [update-grub] would run update-grub");
        } else {
            let out = std::process::Command::new("update-grub").output()
                .map_err(|e| ThinDom0Error::PreFlight(
                    format!("spawn update-grub: {e}")))?;
            if !out.status.success() {
                return Err(ThinDom0Error::PreFlight(format!(
                    "update-grub exit={}: {}",
                    out.status, String::from_utf8_lossy(&out.stderr))));
            }
            eprintln!("    [update-grub] OK");
        }

        // Step 6: verify both entries are present in the new grub.cfg.
        if !self.dry_run {
            verify_grub_dual_entry(&self.grub_entry_name)?;
        }

        eprintln!();
        eprintln!("==> ThinDom0 install complete.");
        eprintln!("    GRUB entries:");
        eprintln!("      · 'Ubuntu' (bare metal, untouched, default)");
        eprintln!("      · {:?} (normal)", self.grub_entry_name);
        eprintln!("      · {:?} (recovery, no autostart)", self.grub_entry_name);
        eprintln!("    First boot: prefer '{} (recovery, no autostart)' for bring-up.",
                  self.grub_entry_name);
        eprintln!("    That lands in cockpit on tty1 without auto-firing the");
        eprintln!("    user-desktop guest. Once dom0 looks healthy, start the");
        eprintln!("    guest from cockpit using /etc/rotten-apple/user-desktop.toml.");
        eprintln!("    Recovery: pick 'Ubuntu' (no Xen) at GRUB to return to stock.");
        Ok(())
    }
}

fn copy_kernel(src: &Path, dst: &Path, dry_run: bool) -> Result<()> {
    if dry_run {
        eprintln!("    [kernel] would copy {} -> {}", src.display(), dst.display());
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ThinDom0Error::PreFlight(
            format!("mkdir {}: {e}", parent.display())))?;
    }
    std::fs::copy(src, dst).map_err(|e| ThinDom0Error::PreFlight(
        format!("copy {} -> {}: {e}", src.display(), dst.display())))?;
    eprintln!("    [kernel] {} -> {}", src.display(), dst.display());
    Ok(())
}

fn chmod_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path).map_err(|e| ThinDom0Error::PreFlight(
        format!("stat {}: {e}", path.display())))?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).map_err(|e| ThinDom0Error::PreFlight(
        format!("chmod 755 {}: {e}", path.display())))
}

fn verify_grub_dual_entry(thindom0_name: &str) -> Result<()> {
    let cfg = std::fs::read_to_string("/boot/grub/grub.cfg").map_err(|e|
        ThinDom0Error::PreFlight(format!("read /boot/grub/grub.cfg: {e}")))?;
    let has_bare_metal = cfg.matches("menuentry 'Ubuntu").count() >= 1;
    if !has_bare_metal {
        return Err(ThinDom0Error::PreFlight(
            "bare-metal Ubuntu menuentry missing from grub.cfg after \
             update-grub. Refusing to leave the system without a recovery \
             path. Restore from /etc/grub.d/ defaults and re-run.".into()));
    }
    let has_thindom0 = cfg.contains(&format!("menuentry '{thindom0_name}'"));
    if !has_thindom0 {
        return Err(ThinDom0Error::PreFlight(format!(
            "ThinDom0 menuentry {thindom0_name:?} missing from grub.cfg \
             after update-grub. Did 41_rotten_apple_thindom0 emit anything? \
             Try: `sh -x /etc/grub.d/41_rotten_apple_thindom0` to debug.")));
    }
    eprintln!("    [verify] OK — bare-metal Ubuntu + ThinDom0 entries both present");
    Ok(())
}

/// Collect SSH public keys that should be authorized to log in as root
/// on dom0. Reads:
///
///   1. $SUDO_USER's ~/.ssh/authorized_keys (most users have keys here
///      from prior `ssh-copy-id` to their own machine)
///   2. $SUDO_USER's ~/.ssh/id_*.pub (their own pubkeys — they can log
///      in to dom0 with the matching private key)
///
/// Returns the concatenated content suitable for /root/.ssh/authorized_keys.
/// Empty when nothing found — caller decides whether that's a hard
/// failure or a warning.
fn collect_user_ssh_keys() -> String {
    let user = std::env::var("SUDO_USER").ok()
        .filter(|u| !u.is_empty() && u != "root")
        .or_else(|| std::env::var("USER").ok());
    let home = match user.as_deref() {
        Some(u) => PathBuf::from(format!("/home/{u}")),
        None    => return String::new(),
    };
    if !home.exists() {
        return String::new();
    }
    let ssh = home.join(".ssh");
    let mut out = String::new();

    // authorized_keys verbatim — the user already curated who has access
    if let Ok(s) = std::fs::read_to_string(ssh.join("authorized_keys")) {
        out.push_str(&s);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    // *.pub — their own public keys, so they can ssh from this same
    // machine using their private key. Skip duplicates trivially by
    // checking substring presence.
    if let Ok(rd) = std::fs::read_dir(&ssh) {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if !name_s.ends_with(".pub") {
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(entry.path())
                && !out.contains(s.trim())
            {
                out.push_str(&s);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// True when the running system is itself a Xen domain (typically the
/// v0.0.1 FatDom0 lift). Used to skip operations whose results are
/// meaningless inside a domain — most importantly E-core pinning,
/// because /sys reflects dom0's pinned vcpu view, not bare-metal
/// physical CPUs. install-thindom0 still runs fine from inside Xen
/// (it just produces a cpio + GRUB entry), but the resulting boot
/// won't have an accurate dom0 pin until the user re-runs it from
/// bare-metal Ubuntu.
fn is_running_under_xen() -> bool {
    std::path::Path::new("/proc/xen").exists()
        || std::fs::read_to_string("/sys/hypervisor/type")
            .map(|s| s.trim() == "xen")
            .unwrap_or(false)
}

/// Pull the framebuffer GPU's PCI BDF out of the host's GPU
/// enumeration. The framebuffer GPU is the one currently driving
/// /sys/class/graphics/fb0 (= the laptop screen on this user's host
/// = the Intel iGPU at 0000:00:02.0). Returns None on a host with no
/// framebuffer GPU (server / no display attached).
fn framebuffer_gpu_bdf() -> Option<String> {
    use rotten_apple_detect::gpu::{GpuRole, enumerate_gpus};
    enumerate_gpus().into_iter()
        .find(|g| matches!(g.role, GpuRole::Framebuffer))
        .map(|g| g.bdf)
}

/// The kernel driver bound to the framebuffer GPU on this host (e.g.
/// "i915", "amdgpu", "nouveau"). Used to bundle exactly that driver's
/// firmware into the dom0 cpio so the panel lights — vendor-agnostic,
/// derived from the live machine. None when no framebuffer GPU is bound
/// to a driver (headless host, or driver not yet loaded).
fn framebuffer_gpu_driver() -> Option<String> {
    use rotten_apple_detect::gpu::{GpuRole, enumerate_gpus};
    enumerate_gpus().into_iter()
        .find(|g| matches!(g.role, GpuRole::Framebuffer))
        .and_then(|g| g.current_driver)
}

/// `apt-get install -y mmdebstrap cpio`. The rootfs builder needs both;
/// without them install-thindom0 stops mid-flight with a noisy error.
/// We run it before the rootfs build so the user gets a single apt
/// transaction at the start instead of two surprise prompts later.
fn apt_install_dependencies() -> Result<()> {
    eprintln!("    [apt] apt-get install -y mmdebstrap cpio");
    let out = std::process::Command::new("apt-get")
        .args(["install", "-y", "--no-install-recommends", "mmdebstrap", "cpio"])
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .map_err(|e| ThinDom0Error::PreFlight(format!("spawn apt-get: {e}")))?;
    if !out.status.success() {
        return Err(ThinDom0Error::PreFlight(format!(
            "apt-get install mmdebstrap cpio exit={}: {}",
            out.status, String::from_utf8_lossy(&out.stderr))));
    }
    Ok(())
}

/// The host's Ubuntu suite codename, read verbatim from /etc/os-release
/// (e.g. "resolute" for 26.04, "noble" for 24.04). We deliberately do NOT
/// allowlist codenames: the dom0 rootfs is bootstrapped from the SAME suite
/// the host runs, so whatever the host calls itself IS the right apt suite —
/// an allowlist just rots every six months and silently mis-bootstraps onto
/// an older suite (which is how a `noble` fallback tried to install Xen 4.20
/// packages that only exist on `resolute`). Returns None only when
/// /etc/os-release is missing or carries no codename.
fn host_ubuntu_codename() -> Option<String> {
    let os_release = std::fs::read_to_string("/etc/os-release").ok()?;
    let field = |key: &str| {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().trim_matches('"').trim().to_string())
            .filter(|v| !v.is_empty())
    };
    // VERSION_CODENAME is the apt suite; UBUNTU_CODENAME is an equivalent
    // fallback present even on some derivatives.
    field("VERSION_CODENAME=").or_else(|| field("UBUNTU_CODENAME="))
}

/// Extract the Xen major.minor (e.g. "4.20") from the discovered Xen image
/// basename ("xen-4.20-amd64.gz"). The dom0 rootfs must install the matching
/// `xen-utils-<ver>` / `libxenmisc<ver>`, so we derive the version from the
/// image the host actually boots rather than hardcoding it — same reason as
/// the suite above.
fn xen_version_from_image(basename: &Path) -> Option<String> {
    let name = basename.file_name()?.to_str()?;
    // "xen-4.20-amd64.gz" → "4.20"
    let ver = name.strip_prefix("xen-")?.split('-').next()?;
    (ver.contains('.') && ver.starts_with(|c: char| c.is_ascii_digit()))
        .then(|| ver.to_string())
}

/// One row of mountinfo, narrowed to the fields we need. Struct exists
/// so tests can construct fake input without a real /proc.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UserRoot {
    device: PathBuf,
    fstype: String,
}

/// Parse `/proc/self/mountinfo` and return the device backing `/`. The
/// mountinfo format (man 5 proc):
///
/// ```text
/// 36 35 98:0 /mnt1 /mnt/parent rw,relatime master:1 - ext3 /dev/root rw,errors=continue
/// │  │  │   │     │            │           │       │ │   │           └─ super options
/// │  │  │   │     │            │           │       │ │   └─ device
/// │  │  │   │     │            │           │       │ └─ fs type
/// │  │  │   │     │            │           │       └─ field separator
/// │  │  │   │     │            │           └─ optional fields ("master:1")
/// │  │  │   │     │            └─ mount options
/// │  │  │   │     └─ MOUNT POINT (we filter on this)
/// │  │  │   └─ root within the FS
/// │  │  └─ st_dev
/// │  └─ parent ID
/// └─ mount ID
/// ```
///
/// We pick the entry whose mount point is exactly "/" — the line whose
/// 5th field equals "/". Returns the device path and fs type.
fn parse_user_root_from_mountinfo(mountinfo: &str) -> Result<UserRoot> {
    for line in mountinfo.lines() {
        let mut parts = line.split(" - ");
        let pre = parts.next().unwrap_or("");
        let post = parts.next();

        let mp = pre.split_whitespace().nth(4);
        if mp != Some("/") {
            continue;
        }
        let post = post.ok_or_else(|| ThinDom0Error::PreFlight(
            format!("mountinfo line missing ' - ' separator: {line}")))?;
        let mut after = post.split_whitespace();
        let fstype = after.next()
            .ok_or_else(|| ThinDom0Error::PreFlight(
                format!("mountinfo line missing fstype: {line}")))?;
        let device = after.next()
            .ok_or_else(|| ThinDom0Error::PreFlight(
                format!("mountinfo line missing device: {line}")))?;
        return Ok(UserRoot {
            device: PathBuf::from(device),
            fstype: fstype.to_string(),
        });
    }
    Err(ThinDom0Error::PreFlight(
        "no entry with mount point '/' found in /proc/self/mountinfo".into()))
}

/// Walk the block-device dependency chain rooted at `dev` and return
/// the underlying physical disk. For an LV→LUKS→partition→disk chain
/// like /dev/mapper/ubuntu--vg-ubuntu--lv, returns /dev/nvme1n1. Uses
/// `lsblk --inverse -no PATH,TYPE` and picks the first row whose TYPE
/// is `disk`. lsblk reads /sys, no privileges needed.
fn parent_disk_for(dev: &Path) -> Result<PathBuf> {
    let dev_s = dev.to_str().ok_or_else(|| ThinDom0Error::PreFlight(
        format!("non-utf8 device path: {}", dev.display())))?;
    let out = std::process::Command::new("lsblk")
        .args(["--inverse", "-no", "PATH,TYPE", dev_s])
        .output()
        .map_err(|e| ThinDom0Error::PreFlight(
            format!("spawn lsblk: {e} (is util-linux installed?)")))?;
    if !out.status.success() {
        return Err(ThinDom0Error::PreFlight(format!(
            "lsblk exit={} for {}: {}",
            out.status, dev.display(),
            String::from_utf8_lossy(&out.stderr))));
    }
    parse_lsblk_for_disk(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| ThinDom0Error::PreFlight(format!(
            "no row of TYPE=disk in lsblk output for {} — exotic layout?",
            dev.display())))
}

/// Partition number of a partition device node, for `efibootmgr -p`.
/// Trailing decimal digits: /dev/nvme0n1p1 → 1, /dev/sda3 → 3. Whole
/// disks (/dev/nvme0n1) would mis-parse — but callers only pass ESP
/// partition nodes from mountinfo, and nvme disks end in `n<digit>`
/// which the `p<digits>` check below rejects.
fn partition_number_of(dev: &Path) -> Option<u32> {
    let s = dev.to_string_lossy();
    let digits: String = s.chars().rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>().chars().rev().collect();
    if digits.is_empty() || digits.len() == s.len() {
        return None;
    }
    let stem = &s[..s.len() - digits.len()];
    // nvme whole-disk nodes end in "n<digits>" (namespace); partitions
    // end in "p<digits>". Plain sd/vd disks end in a letter.
    if stem.ends_with('n') && stem.contains("nvme") {
        return None;
    }
    digits.parse().ok()
}

/// Pure parser for `lsblk --inverse -no PATH,TYPE <dev>` output.
/// Each row is two whitespace-separated columns. Pick the first row
/// whose TYPE column is exactly "disk".
fn parse_lsblk_for_disk(stdout: &str) -> Option<PathBuf> {
    for line in stdout.lines() {
        let mut cols = line.split_whitespace();
        let path = cols.next()?;
        let kind = cols.next()?;
        if kind == "disk" {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// Resolve a canonical block-device path (e.g. `/dev/nvme1n1`) to a
/// stable identifier under `/dev/disk/by-id/`. Critical for guest disk
/// passthrough on multi-NVMe hosts: NVMe controller enumeration order
/// is BIOS-firmware-dependent and can swap nvme0n1 ↔ nvme1n1 between
/// reboots, so a manifest hardcoding `/dev/nvme1n1` will eventually
/// hand the WRONG disk (e.g. Windows) to a Linux guest. The by-id
/// path is keyed on model + serial — only changes if the disk is
/// physically replaced. Pure: takes a list of (link, target) tuples
/// (caller reads the directory and resolves symlinks).
///
/// Picking strategy:
///   1. Filter to entries whose target equals `canonical`.
///   2. Filter to entries whose link basename starts with `nvme-` or
///      `wwn-` or `scsi-` (skip eui-style hex blobs and `-part*` rows).
///   3. Prefer entries that DON'T end with `_<digit>` (those are
///      duplicate per-namespace symlinks; the un-suffixed form is
///      the "primary" link).
///   4. Among remaining candidates, pick the lexicographically
///      smallest link path (deterministic).
///
/// Returns `Some(by-id path)` on success, `None` if no by-id entry
/// matches (caller falls back to `canonical`).
fn pick_stable_disk_path(canonical: &Path, by_id_entries: &[(PathBuf, PathBuf)])
    -> Option<PathBuf>
{
    let mut candidates: Vec<&Path> = by_id_entries.iter()
        .filter(|(_, target)| target == canonical)
        .map(|(link, _)| link.as_path())
        .filter(|link| {
            // Skip partition rows and exotic identifier styles.
            let name = link.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.contains("-part") { return false; }
            if name.starts_with("nvme-eui.") { return false; }
            name.starts_with("nvme-")
                || name.starts_with("wwn-")
                || name.starts_with("scsi-")
        })
        .collect();
    if candidates.is_empty() { return None; }
    // Prefer un-suffixed names (e.g. `nvme-Samsung_..._S7U6` over
    // `nvme-Samsung_..._S7U6_1` which is a duplicate namespace alias).
    let unsuffixed: Vec<&&Path> = candidates.iter()
        .filter(|p| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            // "ends with _<digit>" → name has at least 2 chars, second-to-last is '_', last is digit
            let bytes = name.as_bytes();
            !(bytes.len() >= 2
              && bytes[bytes.len() - 2] == b'_'
              && bytes[bytes.len() - 1].is_ascii_digit())
        })
        .collect();
    if !unsuffixed.is_empty() {
        candidates = unsuffixed.into_iter().copied().collect();
    }
    candidates.sort();
    candidates.first().map(|p| p.to_path_buf())
}

/// I/O wrapper: read `/dev/disk/by-id/`, resolve every symlink target,
/// and call `pick_stable_disk_path`. Falls back to `canonical` if the
/// directory is missing or no by-id entry matches.
fn stable_disk_path_for(canonical: &Path) -> PathBuf {
    let by_id = Path::new("/dev/disk/by-id");
    let entries: Vec<(PathBuf, PathBuf)> = match std::fs::read_dir(by_id) {
        Ok(rd) => rd.flatten()
            .filter_map(|entry| {
                let link = entry.path();
                let target = std::fs::canonicalize(&link).ok()?;
                Some((link, target))
            })
            .collect(),
        Err(_) => return canonical.to_path_buf(),
    };
    pick_stable_disk_path(canonical, &entries)
        .unwrap_or_else(|| canonical.to_path_buf())
}

fn uname_release() -> Result<String> {
    // Kernel PIN override. The dom0 kernel normally tracks the host's running
    // kernel (uname -r), but that's NOT guaranteed to boot as a Xen PV dom0:
    // the host's newest kernel can regress PV-dom0 support on a given machine.
    // CONFIRMED on this Dell (2026-06): 6.17.0-23 reached /init and logged
    // (init-20260522-000049.log); every 7.0.0-x boot since died BEFORE /init
    // (no log, black) — the lift silently switched dom0 to 7.0 when the host
    // kernel updated. `RA_DOM0_KERNEL=<release>` pins the dom0 kernel (and its
    // module set) to a known-good PV release, decoupled from uname -r.
    // See [[project_thindom0_boot_wedge]] / [[feedback_xen_pv_vs_pvh_dom0]].
    if let Ok(k) = std::env::var("RA_DOM0_KERNEL") {
        let k = k.trim();
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    let out = std::process::Command::new("uname").arg("-r").output()
        .map_err(|e| ThinDom0Error::PreFlight(format!("spawn uname -r: {e}")))?;
    if !out.status.success() {
        return Err(ThinDom0Error::PreFlight(format!(
            "uname -r exit={}: {}",
            out.status, String::from_utf8_lossy(&out.stderr))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve a block device path to its filesystem UUID by walking
/// /dev/disk/by-uuid/. udev populates this directory at boot with one
/// symlink per recognised filesystem; the symlink target points back at
/// the canonical device node. Reading it requires no privileges and no
/// subprocess (vs. blkid which on Ubuntu falls back to /run/blkid/blkid.tab
/// only when running as root). Returns the first UUID whose symlink
/// resolves to the same canonical path as `dev`.
fn uuid_for_device(dev: &Path) -> Result<String> {
    uuid_for_device_under(dev, Path::new("/dev/disk/by-uuid"))
}

fn uuid_for_device_under(dev: &Path, by_uuid_dir: &Path) -> Result<String> {
    let target = std::fs::canonicalize(dev).map_err(|e| ThinDom0Error::PreFlight(
        format!("canonicalize {}: {e}", dev.display())))?;
    let entries = std::fs::read_dir(by_uuid_dir).map_err(|e| ThinDom0Error::PreFlight(
        format!("read {}: {e}", by_uuid_dir.display())))?;
    for entry in entries.flatten() {
        let candidate = match std::fs::canonicalize(entry.path()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if candidate == target {
            let name = entry.file_name();
            let name_s = name.to_string_lossy().to_string();
            if !name_s.is_empty() {
                return Ok(name_s);
            }
        }
    }
    Err(ThinDom0Error::PreFlight(format!(
        "no UUID symlink in {} resolves to {} — is the device an \
         unrecognised filesystem, or did udev not populate by-uuid?",
        by_uuid_dir.display(), target.display())))
}

impl std::fmt::Display for ThinDom0Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "rotten-apple ThinDom0 install plan")?;
        writeln!(f, "──────────────────────────────────")?;
        writeln!(f, "dom0 kernel target:  {}", self.kernel_path.display())?;
        writeln!(f, "dom0 initrd target:  {}", self.initrd_path.display())?;
        writeln!(f, "dom0 kernel source:  {} (host's running kernel)",
                 self.kernel_source.display())?;
        writeln!(f, "dom0_mem:            {} MB (hard-cap)", self.dom0_mem_mb)?;
        writeln!(f, "dom0_max_vcpus:      {} (hard-cap)", self.dom0_vcpus)?;
        match self.dom0_pinned_cpu {
            Some(cpu) => writeln!(f, "dom0 vcpu pin:       physical CPU {cpu} (first E-core)")?,
            None if is_running_under_xen() => writeln!(f,
                "dom0 vcpu pin:       SKIPPED — already inside a Xen domain. \
                 \n                     Re-run from bare-metal Ubuntu for accurate E-core pinning.")?,
            None => writeln!(f, "dom0 vcpu pin:       none (uniform-core host)")?,
        }
        writeln!(f, "Xen image:           /boot/{}", self.xen_image_basename.display())?;
        match &self.framebuffer_gpu_bdf {
            Some(bdf) => writeln!(f, "framebuffer GPU:     {bdf} → user-desktop guest passthrough")?,
            None => writeln!(f, "framebuffer GPU:     none detected → guest gets PV framebuffer")?,
        }
        writeln!(f, "user root device:    {}",  self.user_root_device.display())?;
        writeln!(f, "  UUID:              {}",  self.user_root_uuid)?;
        writeln!(f, "  fstype:            {}",  self.user_root_fstype)?;
        writeln!(f, "  parent disk:       {} (whole-disk passthrough source)",
                 self.user_root_disk.display())?;
        if self.has_separate_boot {
            writeln!(f, "/boot partition:     separate, UUID {}", self.boot_fs_uuid)?;
        } else {
            writeln!(f, "/boot partition:     on rootfs (UUID {})", self.boot_fs_uuid)?;
        }
        writeln!(f, "persistent state:    {}",  self.persistent_state_dir.display())?;
        writeln!(f, "GRUB entry:          {:?}", self.grub_entry_name)?;
        writeln!(f, "dry-run:             {}",  self.dry_run)?;
        writeln!(f)?;
        writeln!(f, "What this WILL do (when executed):")?;
        writeln!(f, "  1. Build a thin-dom0 cpio.gz containing busybox + Xen tools")?;
        writeln!(f, "     + rotten-apple, sized ~150 MB. Boots entirely in RAM.")?;
        writeln!(f, "  2. Install the cpio.gz + a copy of the host's kernel under")?;
        writeln!(f, "     /boot/rotten-apple/.")?;
        writeln!(f, "  3. Drop /etc/grub.d/41_rotten_apple_thindom0 + run update-grub")?;
        writeln!(f, "     to add the menuentry {:?}.", self.grub_entry_name)?;
        writeln!(f, "  4. Generate /etc/rotten-apple/user-desktop.toml describing")?;
        writeln!(f, "     a guest that owns the user's existing root partition.")?;
        writeln!(f)?;
        writeln!(f, "What this WILL NOT do (sacred Ubuntu image rule):")?;
        writeln!(f, "  · No changes to the bare-metal Ubuntu menuentry.")?;
        writeln!(f, "  · No changes to the user's rootfs ({}), fstype {}.",
                 self.user_root_device.display(), self.user_root_fstype)?;
        writeln!(f, "  · No changes to the user's kernel or initramfs.")?;
        writeln!(f, "  · No partition table modifications.")?;
        writeln!(f)?;
        writeln!(f, "Recovery: at any GRUB menu, pick 'Ubuntu' (no Xen) to boot")?;
        writeln!(f, "stock bare-metal. The ThinDom0 entry is purely additive.")?;
        writeln!(f)?;
        writeln!(f, "Preview — /etc/grub.d/41_rotten_apple_thindom0:")?;
        writeln!(f, "─────────────────────────────────────────────")?;
        for line in self.render_grub_script().lines() {
            writeln!(f, "  {line}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MOUNTINFO: &str = "\
22 25 0:21 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
23 25 0:4 / /proc rw,nosuid,nodev,noexec,relatime shared:13 - proc proc rw
24 25 0:6 / /dev rw,nosuid shared:2 - devtmpfs udev rw,size=8123456k
25 1 259:2 / / rw,relatime shared:1 - ext4 /dev/mapper/ubuntu--vg-ubuntu--lv rw,errors=remount-ro
26 22 0:23 / /sys/kernel/security rw,nosuid shared:8 - securityfs securityfs rw
27 25 0:24 / /run rw,nosuid,nodev shared:5 - tmpfs tmpfs rw,size=...
";

    #[test]
    fn parses_lvm_root_from_mountinfo() {
        // Real shape from this laptop: LVM root over LUKS on Ubuntu 24.04.
        let r = parse_user_root_from_mountinfo(SAMPLE_MOUNTINFO).unwrap();
        assert_eq!(r.device, PathBuf::from("/dev/mapper/ubuntu--vg-ubuntu--lv"));
        assert_eq!(r.fstype, "ext4");
    }

    #[test]
    fn parses_btrfs_root() {
        // Btrfs hosts identify the root mount point the same way; only
        // fstype differs. Pin the fstype field is faithfully captured.
        let info = "1 1 0:1 / / rw - btrfs /dev/sda2 rw,subvol=@\n";
        let r = parse_user_root_from_mountinfo(info).unwrap();
        assert_eq!(r.fstype, "btrfs");
        assert_eq!(r.device, PathBuf::from("/dev/sda2"));
    }

    #[test]
    fn parses_partition_root() {
        // Plain partition (no LVM, no LUKS) — fastest path. Common on
        // newcomers running their first Ubuntu install.
        let info = "30 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n";
        let r = parse_user_root_from_mountinfo(info).unwrap();
        assert_eq!(r.device, PathBuf::from("/dev/sda1"));
    }

    #[test]
    fn skips_non_root_mounts() {
        // /proc, /sys, /run, et al. all sit alongside /. Pin: parser
        // doesn't accidentally pick the first line.
        let info = "\
22 25 0:21 / /sys rw - sysfs sysfs rw
25 1 259:2 / / rw - ext4 /dev/sda3 rw
27 25 0:24 / /run rw - tmpfs tmpfs rw
";
        let r = parse_user_root_from_mountinfo(info).unwrap();
        assert_eq!(r.device, PathBuf::from("/dev/sda3"));
    }

    #[test]
    fn errors_when_root_missing() {
        // If somehow / isn't in mountinfo, fail loud rather than picking
        // a bind-mount or tmpfs by accident.
        let info = "27 25 0:24 / /run rw - tmpfs tmpfs rw\n";
        let err = parse_user_root_from_mountinfo(info).unwrap_err();
        match err {
            ThinDom0Error::PreFlight(msg) => {
                assert!(msg.contains("no entry with mount point '/'"),
                        "unexpected message: {msg}");
            }
        }
    }

    #[test]
    fn errors_on_malformed_line() {
        // No ' - ' separator means we can't reach the device field.
        // This only matters for the line that actually mounts /; other
        // lines without a separator are simply skipped above.
        let info = "1 1 0:1 / / rw nosep ext4 /dev/sda1 rw\n";
        let err = parse_user_root_from_mountinfo(info).unwrap_err();
        match err {
            ThinDom0Error::PreFlight(msg) => {
                assert!(msg.contains("missing ' - ' separator")
                     || msg.contains("no entry with mount point '/'"),
                        "unexpected message: {msg}");
            }
        }
    }

    #[test]
    fn pick_stable_disk_path_prefers_unsuffixed_model_serial() {
        // Real shape from /dev/disk/by-id/ on a host with two NVMe disks
        // (Linux on Samsung 1.8T, Windows on Toshiba 953G). Both have
        // duplicate "_1" namespace aliases plus an exotic eui hex blob.
        // Pin: we pick the un-suffixed nvme- entry pointing to nvme1n1
        // (the Linux disk), and we ignore -part rows, eui, and _1 dups.
        let canonical = PathBuf::from("/dev/nvme1n1");
        let entries: Vec<(PathBuf, PathBuf)> = vec![
            // matches but is a partition row
            (PathBuf::from("/dev/disk/by-id/nvme-Samsung_SSD_990_EVO_Plus_2TB_S7U6NU0Y743256F-part1"),
             PathBuf::from("/dev/nvme1n1p1")),
            // matches and is the unsuffixed primary — should win
            (PathBuf::from("/dev/disk/by-id/nvme-Samsung_SSD_990_EVO_Plus_2TB_S7U6NU0Y743256F"),
             PathBuf::from("/dev/nvme1n1")),
            // matches but is the _1 namespace dup — should lose
            (PathBuf::from("/dev/disk/by-id/nvme-Samsung_SSD_990_EVO_Plus_2TB_S7U6NU0Y743256F_1"),
             PathBuf::from("/dev/nvme1n1")),
            // matches but is exotic eui hex — should lose (not human readable)
            (PathBuf::from("/dev/disk/by-id/nvme-eui.0025385751a1216d"),
             PathBuf::from("/dev/nvme1n1")),
            // doesn't match — wrong target
            (PathBuf::from("/dev/disk/by-id/nvme-KXG50ZNV1T02_NVMe_TOSHIBA_1024GB_58JS10DVT8LQ"),
             PathBuf::from("/dev/nvme0n1")),
        ];
        let pick = pick_stable_disk_path(&canonical, &entries).unwrap();
        assert_eq!(pick, PathBuf::from(
            "/dev/disk/by-id/nvme-Samsung_SSD_990_EVO_Plus_2TB_S7U6NU0Y743256F"));
    }

    #[test]
    fn pick_stable_disk_path_returns_none_when_no_matches() {
        // If by-id is empty or nothing points at our canonical device,
        // return None so the caller can fall back to the canonical path
        // rather than crashing the lift.
        let canonical = PathBuf::from("/dev/nvme1n1");
        assert!(pick_stable_disk_path(&canonical, &[]).is_none());
        // All entries point elsewhere
        let entries = vec![
            (PathBuf::from("/dev/disk/by-id/nvme-Other"),
             PathBuf::from("/dev/nvme0n1")),
        ];
        assert!(pick_stable_disk_path(&canonical, &entries).is_none());
    }

    #[test]
    fn pick_stable_disk_path_falls_back_to_eui_only_when_nothing_else() {
        // Fallback rule: if there's literally nothing model-named, we
        // STILL prefer no match over an eui-style hex blob. Caller will
        // fall back to the canonical /dev/nvmeXn1 path, which IS stable
        // within a single boot — better than a random-looking eui.
        let canonical = PathBuf::from("/dev/nvme0n1");
        let entries = vec![
            (PathBuf::from("/dev/disk/by-id/nvme-eui.deadbeef"),
             PathBuf::from("/dev/nvme0n1")),
        ];
        assert!(pick_stable_disk_path(&canonical, &entries).is_none(),
                "eui-only entries should not be picked — caller falls back");
    }

    #[test]
    fn pick_stable_disk_path_handles_scsi_and_wwn_styles() {
        // Non-NVMe disks (older SATA SSDs, virtual disks) use scsi- or
        // wwn- prefix. Pin both work so this isn't NVMe-only.
        let canonical = PathBuf::from("/dev/sda");
        let entries = vec![
            (PathBuf::from("/dev/disk/by-id/scsi-SATA_INTEL_SSDSC2BW12_BTHV1234"),
             PathBuf::from("/dev/sda")),
            (PathBuf::from("/dev/disk/by-id/wwn-0x500a07511a234567"),
             PathBuf::from("/dev/sda")),
        ];
        let pick = pick_stable_disk_path(&canonical, &entries).unwrap();
        // Lex sort: scsi- < wwn- alphabetically
        assert_eq!(pick.file_name().unwrap().to_str().unwrap(),
                   "scsi-SATA_INTEL_SSDSC2BW12_BTHV1234");
    }

    #[test]
    fn uuid_lookup_walks_by_uuid_dir() {
        // Build a fake /dev/disk/by-uuid/ pointing at a real file so
        // canonicalize() resolves; pin that the function picks the
        // matching UUID symlink and ignores the others.
        let tmp = tempfile::tempdir().unwrap();
        let device = tmp.path().join("real-device");
        std::fs::write(&device, b"").unwrap();
        let other = tmp.path().join("other-device");
        std::fs::write(&other, b"").unwrap();

        let by_uuid = tmp.path().join("by-uuid");
        std::fs::create_dir(&by_uuid).unwrap();
        std::os::unix::fs::symlink(&device, by_uuid.join("aaaa-1111")).unwrap();
        std::os::unix::fs::symlink(&other,  by_uuid.join("bbbb-2222")).unwrap();

        let uuid = uuid_for_device_under(&device, &by_uuid).unwrap();
        assert_eq!(uuid, "aaaa-1111");
    }

    #[test]
    fn uuid_lookup_errors_when_device_unknown() {
        // No symlink resolves to the device — confirm we surface a
        // PreFlight error rather than panicking or returning empty.
        let tmp = tempfile::tempdir().unwrap();
        let device = tmp.path().join("real-device");
        std::fs::write(&device, b"").unwrap();
        let by_uuid = tmp.path().join("by-uuid");
        std::fs::create_dir(&by_uuid).unwrap();
        let err = uuid_for_device_under(&device, &by_uuid).unwrap_err();
        match err {
            ThinDom0Error::PreFlight(msg) => assert!(msg.contains("no UUID symlink")),
        }
    }

    fn sample_plan() -> ThinDom0Plan {
        ThinDom0Plan {
            kernel_path: PathBuf::from("/boot/rotten-apple/vmlinuz"),
            initrd_path: PathBuf::from("/boot/rotten-apple/thin-dom0.cpio.gz"),
            kernel_source: PathBuf::from("/boot/vmlinuz-6.17.0-23-generic"),
            dom0_mem_mb: THINDOM0_DOM0_MEM_MB,
            dom0_vcpus: 1,
            dom0_pinned_cpu: Some(12),
            user_root_device: PathBuf::from("/dev/mapper/ubuntu--vg-ubuntu--lv"),
            user_root_uuid: "abcd-1234-cafe-babe".to_string(),
            user_root_fstype: "ext4".to_string(),
            user_root_disk: PathBuf::from("/dev/nvme1n1"),
            persistent_state_dir: PathBuf::from("/var/lib/rotten-apple"),
            grub_entry_name: "rotten-apple ThinDom0".to_string(),
            has_separate_boot: true,
            boot_fs_uuid: "boot-uuid-1111".to_string(),
            xen_image_basename: PathBuf::from("xen-4.20-amd64.gz"),
            framebuffer_gpu_bdf: Some("0000:00:02.0".to_string()),
            esp_device: Some(PathBuf::from("/dev/nvme0n1p1")),
            esp_fs_uuid: Some("B6D5-2FF2".to_string()),
            dry_run: true,
        }
    }

    #[test]
    fn lsblk_parser_picks_disk_from_lvm_chain() {
        // Real shape from the user's laptop: LV→LUKS→partition→disk.
        // The disk row is third in inverse order; we must find it by
        // TYPE column, not position.
        let out = "\
/dev/mapper/dm_crypt-0            crypt
/dev/mapper/ubuntu--vg-ubuntu--lv lvm
/dev/nvme1n1                      disk
/dev/nvme1n1p3                    part
";
        assert_eq!(parse_lsblk_for_disk(out),
                   Some(PathBuf::from("/dev/nvme1n1")));
    }

    #[test]
    fn lsblk_parser_picks_disk_from_plain_partition() {
        // Plain partition (no LVM, no LUKS) — disk is one row above
        // the partition. Pin: still picked by TYPE, not position.
        let out = "\
/dev/sda1 part
/dev/sda  disk
";
        assert_eq!(parse_lsblk_for_disk(out),
                   Some(PathBuf::from("/dev/sda")));
    }

    #[test]
    fn lsblk_parser_returns_none_when_no_disk_row() {
        // Odd output (loop device, ramdisk, etc.) — caller surfaces a
        // clear PreFlight error rather than guessing.
        let out = "/dev/loop0 loop\n";
        assert_eq!(parse_lsblk_for_disk(out), None);
    }

    #[test]
    fn lsblk_parser_skips_partitions_before_disk() {
        // Multiple partitions of the same disk show up before the disk
        // row in inverse order. Pin: the first DISK row wins, not the
        // first row.
        let out = "\
/dev/sda1 part
/dev/sda2 part
/dev/sda3 part
/dev/sda  disk
";
        assert_eq!(parse_lsblk_for_disk(out),
                   Some(PathBuf::from("/dev/sda")));
    }

    #[test]
    fn plan_summary_marks_sacred_ubuntu_as_untouched() {
        // The Display impl is the surface the user reads when they run
        // `rotten-apple plan-thindom0`. Pin: the summary explicitly says
        // the bare-metal entry and the user's rootfs are untouched —
        // this is the user-facing affordance for the sacred rule.
        let s = format!("{}", sample_plan());
        assert!(s.contains("No changes to the bare-metal Ubuntu menuentry"),
                "summary must promise the bare-metal entry is untouched");
        assert!(s.contains("No changes to the user's rootfs"),
                "summary must promise the user's rootfs is untouched");
        assert!(s.contains("/dev/mapper/ubuntu--vg-ubuntu--lv"),
                "summary must name the actual root device");
        assert!(s.contains("rotten-apple ThinDom0"),
                "summary must show the GRUB entry name");
    }

    #[test]
    fn plan_summary_includes_grub_script_preview() {
        // After the GRUB renderer landed, plan-thindom0 should show the
        // user the actual script that would be written. Pin presence of
        // the menuentry header and the search line so the preview can't
        // silently regress to "see /etc/grub.d/..." style placeholders.
        let s = format!("{}", sample_plan());
        assert!(s.contains("Preview — /etc/grub.d/41_rotten_apple_thindom0"),
                "summary must announce the GRUB preview");
        assert!(s.contains("menuentry 'rotten-apple ThinDom0'"),
                "preview must include the actual menuentry header");
        assert!(s.contains("search --no-floppy --fs-uuid --set=root boot-uuid-1111"),
                "preview must use the boot fs UUID, not the rootfs UUID");
    }

    #[test]
    fn grub_inputs_strip_boot_prefix_when_separate() {
        // /boot is a separate fs → GRUB sees files at "/rotten-apple/..."
        // not "/boot/rotten-apple/..." because GRUB's path is partition-
        // relative after `search --set=root`.
        let p = sample_plan(); // has_separate_boot = true
        let g = p.grub_entry_inputs();
        assert_eq!(g.dom0_kernel, PathBuf::from("rotten-apple/vmlinuz"));
        assert_eq!(g.dom0_initrd, PathBuf::from("rotten-apple/thin-dom0.cpio.gz"));
        assert_eq!(g.xen_image,   PathBuf::from("xen-4.20-amd64.gz"));
        assert_eq!(g.boot_fs_uuid, "boot-uuid-1111");
    }

    #[test]
    fn grub_inputs_keep_boot_prefix_when_unified() {
        // /boot lives on / → GRUB sees files at "/boot/rotten-apple/..."
        // and the search uses the rootfs UUID.
        let mut p = sample_plan();
        p.has_separate_boot = false;
        p.boot_fs_uuid = p.user_root_uuid.clone();
        let g = p.grub_entry_inputs();
        assert_eq!(g.dom0_kernel, PathBuf::from("boot/rotten-apple/vmlinuz"));
        assert_eq!(g.dom0_initrd, PathBuf::from("boot/rotten-apple/thin-dom0.cpio.gz"));
        assert_eq!(g.xen_image,   PathBuf::from("boot/xen-4.20-amd64.gz"));
        assert_eq!(g.boot_fs_uuid, p.user_root_uuid);
    }
}
