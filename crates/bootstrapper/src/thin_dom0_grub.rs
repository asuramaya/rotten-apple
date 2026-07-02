//! ThinDom0 GRUB entry generator.
//!
//! `xen-system-amd64` ships `/etc/grub.d/20_linux_xen` which generates the
//! "Ubuntu, with Xen hypervisor" entry. We sit alongside it as
//! `/etc/grub.d/41_rotten_apple_thindom0` (executable script). update-grub
//! runs every script in /etc/grub.d in lexical order and concatenates
//! their stdout into grub.cfg, so 41_ lands AFTER 10_linux (bare-metal
//! Ubuntu) and 20_linux_xen (FatDom0 lift) — sacred bare-metal entry
//! stays first/default.
//!
//! Paths inside a menuentry are relative to whatever partition GRUB
//! `search`-locked as the root partition. On Ubuntu installs with a
//! separate /boot partition that's the /boot fs; on installs without
//! one, it's the rootfs and paths gain a /boot prefix. We compute this
//! at install time and bake the right paths into the script.
//!
//! The dom0 here is initrd-only — the cpio.gz IS the rootfs, extracted
//! into tmpfs by the kernel at boot, with /init running PID 1 from
//! inside it. No `root=` cmdline arg is needed; the tmpfs is already
//! mounted by the time /init runs. We do pass
//! `rotten_apple.user_root_uuid=<UUID>` so the dom0's /init knows which
//! device to mount as /host (so cockpit can launch the user-desktop
//! guest with the user's existing rootfs as its disk).

use std::path::{Path, PathBuf};

use crate::thin_dom0_manifest::UserDesktopDisplayMode;

/// Inputs to the GRUB script generator. Pure data — no I/O. Decoupled
/// from `ThinDom0Plan` because the script needs paths *relative to the
/// boot partition*, which the plan doesn't carry directly (it carries
/// host absolute paths).
#[derive(Debug, Clone)]
pub struct GrubEntryInputs {
    /// Display name of the menu entry, e.g. "rotten-apple ThinDom0".
    pub entry_name: String,
    /// UUID of the partition that GRUB will `search --set=root` to.
    /// Usually the /boot partition; the rootfs UUID if /boot is on /.
    pub boot_fs_uuid: String,
    /// Filename of the Xen image RELATIVE to the boot partition root.
    /// e.g. "xen-4.20-amd64.gz" for a separate /boot, or
    /// "boot/xen-4.20-amd64.gz" for /boot-on-rootfs.
    pub xen_image: PathBuf,
    /// Filename of the dom0 kernel relative to the boot partition root.
    /// e.g. "rotten-apple/vmlinuz" for separate /boot, or
    /// "boot/rotten-apple/vmlinuz" for /boot-on-rootfs.
    pub dom0_kernel: PathBuf,
    /// Filename of the dom0 initrd relative to the boot partition root.
    pub dom0_initrd: PathBuf,
    /// Xen hypervisor sizing — same numbers the planner emits.
    pub dom0_mem_mb: u64,
    pub dom0_vcpus: u32,
    /// UUID of the user's existing root partition. Passed on the dom0
    /// kernel cmdline so the dom0 init can locate the user's filesystem
    /// to share with the user-desktop guest.
    pub user_root_uuid: String,
    /// Physical CPU index dom0 should be pinned to post-boot (the
    /// first E-core on hybrid hosts). Passed on the dom0 kernel
    /// cmdline as `rotten_apple.dom0_pinned_cpu=N`; /init drops it
    /// in /run for cockpit to consume via `xl vcpu-pin`. None on
    /// uniform-core hosts (no pinning needed).
    pub dom0_pinned_cpu: Option<u32>,
    /// PCI BDF of the framebuffer GPU to hand to xen-pciback at boot
    /// (e.g. "0000:00:02.0" for the Intel iGPU on this Optimus laptop).
    /// When present, the dom0 kernel cmdline carries
    /// `xen-pciback.hide=(<bdf>)` so dom0 binds the device to
    /// xen-pciback at boot and libxl can attach it to the user-desktop
    /// guest at create time. Without this, libxl's pcidevs entry fails
    /// with "device not assignable" — exactly the silent-no-display
    /// failure mode reported on 2026-05-07. None for headless hosts
    /// (no GPU to passthrough).
    pub framebuffer_gpu_bdf: Option<String>,
    /// Display mode for the user-desktop guest. In `ParavirtOnly` (the
    /// production default) dom0 KEEPS the framebuffer GPU so cockpit is
    /// visible on the laptop panel via the native KMS driver — so we must
    /// NOT emit `hide_pci` (binding the GPU to xen-pciback would unbind it
    /// from i915 and black the screen). Only `PassthroughIfAvailable` emits
    /// the hide hint, to lease the GPU to the guest.
    pub display_mode: UserDesktopDisplayMode,
}

/// Render the executable shell script that GRUB's update-grub will
/// invoke. Output ends with a newline. The script uses the standard
/// `tail -n +N $0` self-print idiom from /etc/grub.d/40_custom.
pub fn render_grub_script(g: &GrubEntryInputs) -> String {
    // Path strings — relative to the boot partition. Lossy() is fine
    // here because GRUB doesn't accept non-utf8 paths anyway.
    let xen    = path_with_leading_slash(&g.xen_image);
    let kernel = path_with_leading_slash(&g.dom0_kernel);
    let initrd = path_with_leading_slash(&g.dom0_initrd);

    let GrubEntryInputs {
        entry_name, boot_fs_uuid, dom0_mem_mb, dom0_vcpus, user_root_uuid,
        dom0_pinned_cpu, framebuffer_gpu_bdf, display_mode, ..
    } = g;
    let pin_arg = match dom0_pinned_cpu {
        Some(cpu) => format!(" rotten_apple.dom0_pinned_cpu={cpu}"),
        None => String::new(),
    };
    // PCI passthrough hint for the framebuffer GPU. We deliberately do
    // NOT use `xen-pciback.hide=(BDF)` (the kernel-blessed cmdline
    // syntax) because the parentheses get interpreted as shell
    // metacharacters by the initramfs/init shell parser, panicking
    // PID 1 with "syntax error unexpected '('" → kernel reboot →
    // bootloop. This is well-documented across Alpine/Arch/Debian
    // forums (search 2026-05-07). Instead we pass our OWN cmdline
    // hint without parens; /init parses it and does the sysfs bind
    // (`/sys/bus/pci/drivers/pciback/new_slot` + `bind`) at runtime
    // after modprobe xen_pciback. Same end state (device assignable
    // before guest start), no shell-injection landmine.
    // Only lease the GPU to the guest in passthrough mode. In ParavirtOnly
    // (production default) dom0 keeps the framebuffer GPU so cockpit is
    // visible on the panel — emitting hide_pci here would bind it to
    // xen-pciback, unbind i915, and black the screen.
    let pciback_hide_arg = match (display_mode, framebuffer_gpu_bdf) {
        (UserDesktopDisplayMode::PassthroughIfAvailable, Some(bdf)) =>
            format!(" rotten_apple.hide_pci={bdf}"),
        _ => String::new(),
    };

    // Two menuentries land:
    //   1. <entry_name>                            — normal path, autostart enabled.
    //                                                ThinDom0 cmdline is NOT a copy of FatDom0's:
    //                                                the cpio.gz rootfs has no plymouth and no
    //                                                LUKS volume to unlock, so `quiet
    //                                                plymouth.use-mode=text` just silences
    //                                                /init's stage() output for nothing. Use
    //                                                `loglevel=7` so kernel + /init progress
    //                                                reaches the laptop screen via fbcon, with
    //                                                CONFIG_DRM_SIMPLEDRM=y picking up the
    //                                                EFI/GOP framebuffer the firmware set up.
    //   2. <entry_name> (recovery, no autostart)   — bring-up/debug path. Same shape plus `sync_console=true` on the
    //                                                Xen line so Xen flushes its own console
    //                                                synchronously, and rotten_apple.no_autostart=1
    //                                                so cockpit doesn't take the screen with iGPU
    //                                                passthrough — useful when validating boot.
    //
    // Why NO `nomodeset` and NO `earlyprintk=vga` on EITHER entry: in Xen
    // PV dom0, the kernel does NOT have direct access to VGA hardware
    // (Xen owns it, hands dom0 a paravirtualised view). Both
    // `earlyprintk=vga` and the bare VGA_CONSOLE driver `nomodeset` falls
    // back to write to VGA RAM at 0xB8000, which Xen PV does not expose
    // — so they silently produce zero output. SIMPLEDRM is the visible
    // path under PV dom0 because it grabs the EFI/GOP framebuffer the
    // firmware set up before Xen took over (which Xen DOES leave mapped
    // for dom0). Setting `nomodeset` disables SIMPLEDRM and gives us a
    // black screen even with `loglevel=7` — that was the 2026-05-07 bug.
    //
    // Why no `dom0=pvh`: journal of the last working FatDom0 boot showed
    // `Hypervisor detected: Xen PV` — PV is what works on this hardware
    // out of the box. Forcing PVH on Ubuntu kernel 6.17 silently hung
    // dom0 on real boots ("black screen, no input"). Default = PV.
    //
    // GRUB's parser inside a menuentry block does NOT accept `#` lines —
    // it tries to execute them as commands and fails update-grub with
    // "syntax error". Explanatory comments live ABOVE the menuentry
    // block (top-level GRUB scope, where `#` is a real comment) only.
    format!(r#"#!/bin/sh
# Generated by rotten-apple bootstrapper. Do not edit by hand — re-run
# `sudo rotten-apple lift` to regenerate.
#
# This script appends 'rotten-apple ThinDom0' menuentries to grub.cfg.
# It runs AFTER /etc/grub.d/10_linux (which produces the sacred
# bare-metal Ubuntu entry) so the user can always pick stock Ubuntu.
exec tail -n +13 $0
# ---- everything below this line is what update-grub captures --------

# rotten-apple ThinDom0 — primary entry, autostart enabled. Cockpit
# launches the user-desktop guest 5 seconds after first paint.
menuentry '{entry_name}' --class xen --class gnu-linux --class gnu --class os --class rotten-apple {{
    insmod gzio
    insmod part_gpt
    insmod ext2
    search --no-floppy --fs-uuid --set=root {boot_fs_uuid}
    echo 'Loading Xen ...'
    multiboot2 {xen} placeholder dom0_mem={dom0_mem_mb}M,max:{dom0_mem_mb}M dom0_max_vcpus={dom0_vcpus} iommu=verbose,no-igfx console=vga loglvl=all guest_loglvl=all
    echo 'Loading dom0 kernel ...'
    module2 {kernel} placeholder console=tty0 loglevel=7 iommu=pt intel_iommu=on amd_iommu=on rotten_apple.user_root_uuid={user_root_uuid}{pin_arg}{pciback_hide_arg}
    echo 'Loading dom0 rootfs (initrd) ...'
    module2 {initrd}
}}

# rotten-apple ThinDom0 (recovery, no autostart) — same kernel + cpio,
# but MAXIMALLY OBSERVABLE so we can actually SEE where dom0 dies:
#   * set gfxpayload=keep  — hand GRUB's live framebuffer to Xen so Xen's
#     console=vga is visible (without this, Xen draws to a dead fb and the
#     last thing on screen is GRUB's "Loading dom0 rootfs"; 2026-06-15 the
#     lift had wiped a manual gfxpayload edit, re-blinding us).
#   * dom0 console=hvc0 earlyprintk=xen — route the dom0 KERNEL's console
#     through Xen's PV console (the visible vga one) instead of grabbing
#     the framebuffer directly with tty0 (which goes black under Xen PV
#     before i915 loads). This is what lets dom0's unpack/init/mount
#     messages reach a screen we can read.
#   * Xen noreboot — HALT on a dom0/Xen panic so the crash text freezes
#     on screen instead of resetting the machine and erasing the evidence.
#   * rotten_apple.no_autostart=1 — cockpit skips the iGPU passthrough
#     auto-launch (keeps the screen with dom0).
# Use for the FIRST boot of any new cmdline shape. The normal entry keeps
# console=tty0 (the real panel path) — this entry is the diagnostic lens.
menuentry '{entry_name} (recovery, no autostart)' --class xen --class gnu-linux --class gnu --class os --class rotten-apple-recovery {{
    insmod gzio
    insmod part_gpt
    insmod ext2
    search --no-floppy --fs-uuid --set=root {boot_fs_uuid}
    echo 'Loading Xen (recovery) ...'
    set gfxpayload=keep
    multiboot2 {xen} placeholder dom0_mem={dom0_mem_mb}M,max:{dom0_mem_mb}M dom0_max_vcpus={dom0_vcpus} iommu=verbose,no-igfx console=vga sync_console=true loglvl=all guest_loglvl=all noreboot
    echo 'Loading dom0 kernel (recovery) ...'
    module2 {kernel} placeholder console=hvc0 earlyprintk=xen loglevel=7 iommu=pt intel_iommu=on amd_iommu=on rotten_apple.user_root_uuid={user_root_uuid}{pin_arg}{pciback_hide_arg} rotten_apple.no_autostart=1
    echo 'Loading dom0 rootfs (initrd) ...'
    module2 {initrd}
}}
"#)
}

/// `Path::display()` for paths we're about to emit into a GRUB cmdline,
/// guaranteeing they start with '/'. Most callers pass relative paths
/// like "rotten-apple/vmlinuz"; we normalise to "/rotten-apple/vmlinuz".
fn path_with_leading_slash(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.starts_with('/') {
        s.into_owned()
    } else {
        format!("/{s}")
    }
}

// ---------------------------------------------------------------------------
// Boot partition discovery — separate from rendering so tests are pure.

/// Walk `/proc/self/mountinfo` to find the partition mounted at /boot.
/// Returns `None` if /boot isn't a separate mount (i.e. /boot lives on
/// the rootfs); callers fall back to the rootfs UUID and prepend "boot/"
/// to the kernel/initrd paths.
pub fn boot_mount_device_from_mountinfo(mountinfo: &str) -> Option<PathBuf> {
    for line in mountinfo.lines() {
        let mut parts = line.split(" - ");
        let pre  = parts.next().unwrap_or("");
        let post = parts.next();
        if pre.split_whitespace().nth(4) != Some("/boot") {
            continue;
        }
        let post = post?;
        let mut after = post.split_whitespace();
        let _fstype = after.next()?;
        let device  = after.next()?;
        return Some(PathBuf::from(device));
    }
    None
}

/// Pick a Xen image filename out of `/boot/` (or whatever sysroot is
/// passed). The Debian/Ubuntu xen-system-* package drops files like
/// `xen-4.20-amd64.gz` next to a `xen-4.20-amd64.config` and
/// `xen-4.20-amd64.efi`. We want the .gz form (multiboot2 boots it).
/// Returns the filename only (no leading dir); callers join with the
/// boot partition path for the GRUB script.
pub fn discover_xen_image_under(boot_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    for entry in std::fs::read_dir(boot_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name_s = name.to_string_lossy().to_string();
        if !(name_s.starts_with("xen-") && name_s.ends_with(".gz")) {
            continue;
        }
        // Sort lexically — for "xen-4.20-amd64.gz" vs "xen-4.21-amd64.gz"
        // string ordering picks the higher version. Good enough until
        // someone ships xen-10.x.
        match &best {
            Some((cur, _)) if cur.as_str() >= name_s.as_str() => {}
            _ => best = Some((name_s.clone(), PathBuf::from(name_s))),
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thin_dom0::THINDOM0_DOM0_MEM_MB;

    fn sample_inputs() -> GrubEntryInputs {
        GrubEntryInputs {
            entry_name: "rotten-apple ThinDom0".to_string(),
            boot_fs_uuid: "2048f8be-403d-4cb7-b79a-c821a0daa609".to_string(),
            xen_image: PathBuf::from("xen-4.20-amd64.gz"),
            dom0_kernel: PathBuf::from("rotten-apple/vmlinuz"),
            dom0_initrd: PathBuf::from("rotten-apple/thin-dom0.cpio.gz"),
            dom0_mem_mb: THINDOM0_DOM0_MEM_MB,
            dom0_vcpus: 1,
            user_root_uuid: "9881a7d1-66a6-463d-9433-a7117badbc35".to_string(),
            dom0_pinned_cpu: Some(12),
            framebuffer_gpu_bdf: Some("0000:00:02.0".to_string()),
            // Passthrough fixture: exercises the hide_pci path. Production
            // defaults to ParavirtOnly (see dom0_cmdline_omits_pci_hide_in_paravirt).
            display_mode: UserDesktopDisplayMode::PassthroughIfAvailable,
        }
    }

    #[test]
    fn dom0_cmdline_omits_pci_hide_in_paravirt() {
        // ParavirtOnly: dom0 must KEEP the framebuffer GPU (cockpit visible
        // on the panel), so NO hide_pci even though a GPU was detected —
        // emitting it would bind the iGPU to xen-pciback and black the screen.
        let mut g = sample_inputs();
        g.display_mode = UserDesktopDisplayMode::ParavirtOnly;
        let s = render_grub_script(&g);
        for line in s.lines().filter(|l| l.contains("module2 /rotten-apple/vmlinuz")) {
            assert!(!line.contains("rotten_apple.hide_pci"),
                    "ParavirtOnly must NOT hide the framebuffer GPU: {line}");
        }
    }

    #[test]
    fn script_starts_with_shebang_and_self_print() {
        // GRUB scripts in /etc/grub.d MUST be executable shell. Pin the
        // shebang and the `tail -n +N $0` idiom that emits the menuentry
        // body without re-emitting the header comments.
        let s = render_grub_script(&sample_inputs());
        assert!(s.starts_with("#!/bin/sh\n"),
                "missing shebang: {s:?}");
        assert!(s.contains("exec tail -n +"),
                "missing self-print idiom (tail -n +N $0)");
    }

    #[test]
    fn script_includes_search_set_root_with_uuid() {
        // Without --set=root with the boot UUID, GRUB doesn't know which
        // partition to read xen.gz from. Pin that the boot UUID is
        // baked into the entry.
        let s = render_grub_script(&sample_inputs());
        assert!(
            s.contains("search --no-floppy --fs-uuid --set=root \
                        2048f8be-403d-4cb7-b79a-c821a0daa609"),
            "search line missing or wrong: {s}"
        );
    }

    #[test]
    fn script_loads_xen_kernel_initrd_in_order() {
        // Xen multiboot2 must come first; dom0 kernel and initrd as
        // module2. Pin the order — GRUB cares.
        let s = render_grub_script(&sample_inputs());
        let xen_pos    = s.find("multiboot2 ").expect("multiboot2 line");
        let kernel_pos = s.find("module2 /rotten-apple/vmlinuz").expect("kernel module2");
        let initrd_pos = s.find("module2 /rotten-apple/thin-dom0.cpio.gz").expect("initrd module2");
        assert!(xen_pos < kernel_pos,
                "xen multiboot2 must come before dom0 kernel module2");
        assert!(kernel_pos < initrd_pos,
                "dom0 kernel must come before dom0 initrd");
    }

    #[test]
    fn xen_cmdline_drops_dom0_vcpus_pin() {
        // With dom0_max_vcpus=1, dom0_vcpus_pin would pin to physical
        // CPU 0 (a P-core on hybrid Intel) — exactly what we want to
        // leave for the user-desktop guest. Pin its absence.
        let s = render_grub_script(&sample_inputs());
        let xen_line = s.lines().find(|l| l.contains("multiboot2"))
            .expect("xen multiboot2 line");
        assert!(!xen_line.contains("dom0_vcpus_pin"),
                "Xen cmdline must NOT include dom0_vcpus_pin: {xen_line}");
        assert!(xen_line.contains("dom0_max_vcpus=1"),
                "Xen cmdline must cap to 1 vcpu: {xen_line}");
    }

    #[test]
    fn dom0_cmdline_carries_pinned_cpu_hint() {
        // /init reads /proc/cmdline for rotten_apple.dom0_pinned_cpu=N
        // and stashes it for cockpit to consume. Pin: the cmdline
        // carries it when the plan supplies a CPU.
        let s = render_grub_script(&sample_inputs());
        assert!(s.contains("rotten_apple.dom0_pinned_cpu=12"),
                "dom0 cmdline must carry the pinned CPU hint: {s}");
    }

    #[test]
    fn dom0_cmdline_omits_pinned_cpu_when_none() {
        // Uniform-core hosts: no pin needed. Pin: the kernel cmdline
        // doesn't grow a stray empty parameter. Check the cmdline line
        // itself rather than the whole script — the explanatory comment
        // block above the menuentry legitimately mentions the variable
        // name as part of its docstring.
        let mut g = sample_inputs();
        g.dom0_pinned_cpu = None;
        let s = render_grub_script(&g);
        let cmdline = s.lines()
            .find(|l| l.contains("module2 /rotten-apple/vmlinuz"))
            .expect("dom0 kernel module2 line");
        assert!(!cmdline.contains("rotten_apple.dom0_pinned_cpu="),
                "no pin hint on the cmdline when dom0_pinned_cpu=None: {cmdline}");
    }

    #[test]
    fn dom0_cmdline_includes_iommu_and_user_root_uuid() {
        // Same flag rationale as crates/bootstrapper/src/lib.rs's
        // grub_xen_cmdline_snippet — without these the dom0 kernel
        // can't see IOMMU groups and the GPU lease path is unreachable.
        // `iommu=pt` (passthrough) matches bare-metal Ubuntu's working
        // cmdline; `iommu=on` (force) caused dom0 framebuffer to never
        // come up under Xen PV on the Dell Precision (2026-05-08).
        // user_root_uuid tells the dom0 /init which fs to mount as /host.
        let s = render_grub_script(&sample_inputs());
        assert!(s.contains("iommu=pt intel_iommu=on amd_iommu=on"),
                "dom0 cmdline missing iommu flags: {s}");
        assert!(s.contains("rotten_apple.user_root_uuid=\
                            9881a7d1-66a6-463d-9433-a7117badbc35"),
                "dom0 cmdline missing user_root_uuid: {s}");
    }

    #[test]
    fn primary_dom0_cmdline_is_diagnostic_friendly_for_thindom0() {
        // ThinDom0 is NOT FatDom0: the cpio rootfs has no plymouth (so
        // `plymouth.use-mode=text` is dead text) and no LUKS volume (so
        // there is no prompt to make visible). With `quiet`, /init's
        // stage() output goes nowhere and any kernel issue between Xen
        // handoff and userspace is invisible — that produced the
        // "thindom gets lost no luks prompt to enter" report (2026-05-07).
        //
        // Fix: drop quiet+plymouth-text, keep `loglevel=7`. NO `nomodeset`
        // — under Xen PV dom0 the kernel can't talk to VGA hw directly
        // (Xen owns it), so `nomodeset` disables SIMPLEDRM (the only
        // visible-output path in PV dom0, which uses the EFI/GOP
        // framebuffer the firmware set up) and we get a black screen.
        // PV mode is still the rule: no `dom0=pvh`.
        let s = render_grub_script(&sample_inputs());

        let xen_lines: Vec<&str> = s.lines()
            .filter(|l| l.contains("multiboot2")).collect();
        let xen_primary = xen_lines.first().expect("primary xen line");
        assert!(!xen_primary.contains("dom0=pvh"),
                "primary Xen cmdline must NOT force PVH (PV is what works \
                 on this hardware): {xen_primary}");
        assert!(!xen_primary.contains("sync_console=true"),
                "primary Xen cmdline must NOT carry sync_console=true \
                 (recovery handles that): {xen_primary}");
        assert!(xen_primary.contains("console=vga"),
                "primary Xen cmdline must keep console=vga: {xen_primary}");

        let dom0_lines: Vec<&str> = s.lines()
            .filter(|l| l.contains("module2 /rotten-apple/vmlinuz")).collect();
        let dom0_primary = dom0_lines.first().expect("primary dom0 line");
        assert!(!dom0_primary.contains("quiet"),
                "primary dom0 cmdline must NOT carry `quiet` — it silences \
                 /init's stage() output and any pre-userspace kernel issue \
                 (2026-05-07 thindom-gets-lost regression): {dom0_primary}");
        assert!(!dom0_primary.contains("plymouth.use-mode=text"),
                "primary dom0 cmdline must NOT carry plymouth flags — the \
                 thindom cpio has no plymouth binary: {dom0_primary}");
        assert!(dom0_primary.contains("loglevel=7"),
                "primary dom0 cmdline must carry loglevel=7 so kernel \
                 progress is visible during boot: {dom0_primary}");
        assert!(!dom0_primary.contains("nomodeset"),
                "primary dom0 cmdline must NOT carry `nomodeset` — under \
                 Xen PV dom0 it disables SIMPLEDRM (our visible-output \
                 path) without giving us a working VGA fallback (Xen owns \
                 VGA in PV mode): {dom0_primary}");
        assert!(!dom0_primary.contains("earlyprintk=vga"),
                "primary dom0 cmdline must NOT carry earlyprintk=vga — \
                 in Xen PV dom0 the kernel can't write directly to VGA \
                 RAM, so this produces zero output: {dom0_primary}");
    }

    #[test]
    fn recovery_entry_carries_pv_safe_diagnostic_flags() {
        // Recovery is the safety net. It pins the flags that DO work
        // under Xen PV dom0 (sync_console=true on the Xen line so Xen
        // flushes its own console synchronously; no_autostart so
        // cockpit doesn't grab the screen via iGPU passthrough) and
        // explicitly avoids the flags that LOOK diagnostic but produce
        // black-screen under PV (earlyprintk=vga and nomodeset both
        // depend on direct VGA hardware access dom0 doesn't have).
        let s = render_grub_script(&sample_inputs());

        let xen_recovery = s.lines()
            .filter(|l| l.contains("multiboot2"))
            .nth(1).expect("recovery xen line (second multiboot2)");
        assert!(xen_recovery.contains("sync_console=true"),
                "recovery Xen cmdline must carry sync_console=true: {xen_recovery}");

        let dom0_recovery = s.lines()
            .filter(|l| l.contains("module2 /rotten-apple/vmlinuz"))
            .nth(1).expect("recovery dom0 line (second module2)");
        assert!(dom0_recovery.contains("loglevel=7"),
                "recovery dom0 cmdline must carry loglevel=7: {dom0_recovery}");
        assert!(dom0_recovery.contains("rotten_apple.no_autostart=1"),
                "recovery dom0 cmdline must carry no_autostart=1: {dom0_recovery}");
        assert!(!dom0_recovery.contains("quiet"),
                "recovery dom0 cmdline must NOT carry quiet \
                 (we want to SEE the logs): {dom0_recovery}");
        assert!(!dom0_recovery.contains("earlyprintk=vga"),
                "recovery dom0 cmdline must NOT carry earlyprintk=vga — \
                 under Xen PV dom0 it writes to VGA RAM the kernel can't \
                 access, producing zero output (2026-05-07 lesson): \
                 {dom0_recovery}");
        assert!(!dom0_recovery.contains("nomodeset"),
                "recovery dom0 cmdline must NOT carry nomodeset — under \
                 Xen PV dom0 it disables SIMPLEDRM (our only visible-\
                 output path) without a working VGA fallback: \
                 {dom0_recovery}");
    }

    #[test]
    fn renders_two_menuentries_one_with_no_autostart_flag() {
        // First-boot safety: a recovery entry skips autostart so the
        // user can verify cockpit on tty1 BEFORE iGPU passthrough
        // takes the screen away. Pin both entries are present and
        // only the recovery one carries the rotten_apple.no_autostart
        // cmdline flag.
        let s = render_grub_script(&sample_inputs());
        let normal_count = s.matches("menuentry 'rotten-apple ThinDom0'").count();
        let recovery_count =
            s.matches("menuentry 'rotten-apple ThinDom0 (recovery, no autostart)'").count();
        assert_eq!(normal_count, 1, "must have exactly one primary entry: {s}");
        assert_eq!(recovery_count, 1, "must have exactly one recovery entry: {s}");

        // Find the two dom0 module2 lines and pin which one has the flag.
        let mut module2_lines = s.lines()
            .filter(|l| l.contains("module2 /rotten-apple/vmlinuz"));
        let primary = module2_lines.next().expect("primary module2 line");
        let recovery = module2_lines.next().expect("recovery module2 line");
        assert!(!primary.contains("rotten_apple.no_autostart"),
                "primary entry must NOT carry no_autostart flag: {primary}");
        assert!(recovery.contains("rotten_apple.no_autostart=1"),
                "recovery entry must carry rotten_apple.no_autostart=1: {recovery}");
    }

    #[test]
    fn dom0_cmdline_carries_pci_hide_hint_no_parens() {
        // We pass the framebuffer GPU BDF on the dom0 cmdline so /init
        // can sysfs-bind it to xen-pciback at runtime — making the
        // device assignable before cockpit launches the user-desktop
        // guest. CRITICAL: do NOT use `xen-pciback.hide=(BDF)` (the
        // kernel's native syntax) because the parentheses get parsed
        // as shell metacharacters by initramfs/init, panicking PID 1
        // with "syntax error unexpected '('" → bootloop (real failure
        // observed 2026-05-07). Use our own param name (no parens).
        let s = render_grub_script(&sample_inputs());
        for line in s.lines().filter(|l| l.contains("module2 /rotten-apple/vmlinuz")) {
            assert!(line.contains("rotten_apple.hide_pci=0000:00:02.0"),
                    "dom0 cmdline must carry rotten_apple.hide_pci hint \
                     for /init to sysfs-bind: {line}");
            assert!(!line.contains("xen-pciback.hide=("),
                    "dom0 cmdline must NOT use xen-pciback.hide=(...) \
                     syntax — the parens panic init (2026-05-07 \
                     bootloop): {line}");
        }
    }

    #[test]
    fn dom0_cmdline_omits_pci_hide_when_no_gpu_bdf() {
        // Headless hosts (no framebuffer GPU) — don't emit the hint.
        let mut g = sample_inputs();
        g.framebuffer_gpu_bdf = None;
        let s = render_grub_script(&g);
        for line in s.lines().filter(|l| l.contains("module2 /rotten-apple/vmlinuz")) {
            assert!(!line.contains("rotten_apple.hide_pci"),
                    "headless host must NOT emit hide_pci hint: {line}");
            assert!(!line.contains("xen-pciback.hide"),
                    "headless host must NOT emit xen-pciback.hide either: {line}");
        }
    }

    #[test]
    fn dom0_cmdline_drops_splash_and_hvc0() {
        // Lessons learned in v0.0.4 (LUKS visibility) and v0.0.3 (post-
        // LUKS black screen): no `splash`, no `console=hvc0`. Pin both.
        let s = render_grub_script(&sample_inputs());
        // Find the dom0 module2 line specifically — `splash` could
        // legitimately appear in a comment; the cmdline must not have it.
        let cmdline_line = s.lines()
            .find(|l| l.contains("module2 /rotten-apple/vmlinuz"))
            .expect("dom0 kernel module2 line");
        assert!(!cmdline_line.contains("splash"),
                "dom0 cmdline must not contain splash: {cmdline_line}");
        assert!(!cmdline_line.contains("console=hvc0"),
                "dom0 cmdline must not pin hvc0 console: {cmdline_line}");
        assert!(cmdline_line.contains("console=tty0"),
                "dom0 cmdline must pin tty0 console: {cmdline_line}");
    }

    #[test]
    fn menuentry_body_has_no_hash_comments() {
        // GRUB's parser inside a menuentry block treats `#` as a command,
        // not a comment, and fails update-grub with "syntax error". This
        // happened on a real install (2026-05-06): the script had `#`
        // explanations between echo+multiboot2 and update-grub bombed
        // at the line containing the comment. Pin: every line between
        // the menuentry { and its matching } is empty, a non-comment
        // command, or a closing brace.
        let s = render_grub_script(&sample_inputs());
        let mut in_menu = false;
        for line in s.lines() {
            let trimmed = line.trim_start();
            if line.contains("menuentry '") && line.contains('{') {
                in_menu = true;
                continue;
            }
            if trimmed == "}" {
                in_menu = false;
                continue;
            }
            if in_menu {
                assert!(!trimmed.starts_with('#'),
                        "menuentry body must not contain `#` comment lines \
                         (GRUB parses them as commands and breaks update-grub): \
                         {line:?}");
            }
        }
    }

    #[test]
    fn paths_get_leading_slash() {
        // Inputs are relative to the boot partition root. The renderer
        // is responsible for prepending '/' so GRUB's path parser
        // anchors them at the partition root.
        let mut g = sample_inputs();
        g.xen_image = PathBuf::from("xen-4.20-amd64.gz");
        g.dom0_kernel = PathBuf::from("rotten-apple/vmlinuz");
        let s = render_grub_script(&g);
        assert!(s.contains("multiboot2 /xen-4.20-amd64.gz"),
                "xen path missing leading slash: {s}");
        assert!(s.contains("module2 /rotten-apple/vmlinuz"),
                "kernel path missing leading slash: {s}");
    }

    #[test]
    fn paths_with_leading_slash_already_unchanged() {
        // If a caller passes "/xen.gz" we shouldn't end up with "//xen.gz".
        let mut g = sample_inputs();
        g.xen_image = PathBuf::from("/xen-4.20-amd64.gz");
        let s = render_grub_script(&g);
        assert!(s.contains("multiboot2 /xen-4.20-amd64.gz"));
        assert!(!s.contains("multiboot2 //xen"),
                "double-slash leak: {s}");
    }

    #[test]
    fn entry_name_class_includes_rotten_apple() {
        // Custom CSS class lets future grub themes style our entry
        // distinctly. Pin its presence so themers have a stable hook.
        let s = render_grub_script(&sample_inputs());
        assert!(s.contains("--class rotten-apple"),
                "entry should carry --class rotten-apple: {s}");
    }

    #[test]
    fn boot_mount_detects_separate_partition() {
        // Real-host shape from this laptop: separate /boot partition
        // backed by /dev/nvme0n1p2 (or similar).
        let info = "\
22 25 0:21 / /sys rw - sysfs sysfs rw
25 1 259:2 / / rw - ext4 /dev/mapper/cryptroot rw
40 25 259:1 / /boot rw - ext4 /dev/nvme0n1p2 rw
";
        let dev = boot_mount_device_from_mountinfo(info).unwrap();
        assert_eq!(dev, PathBuf::from("/dev/nvme0n1p2"));
    }

    #[test]
    fn boot_mount_returns_none_when_boot_on_rootfs() {
        // Single-partition installs (Ubuntu Server defaults often) have
        // /boot living on the rootfs — no separate mount line. We must
        // return None so callers know to fall back to the rootfs UUID.
        let info = "\
22 25 0:21 / /sys rw - sysfs sysfs rw
25 1 259:2 / / rw - ext4 /dev/sda1 rw
";
        assert!(boot_mount_device_from_mountinfo(info).is_none());
    }

    #[test]
    fn discover_xen_image_picks_dot_gz() {
        // The xen-system-amd64 package drops .config, .efi, .gz next to
        // each other. multiboot2 boots the .gz; we must pick that.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("xen-4.20-amd64.config"), b"").unwrap();
        std::fs::write(tmp.path().join("xen-4.20-amd64.efi"),    b"").unwrap();
        std::fs::write(tmp.path().join("xen-4.20-amd64.gz"),     b"").unwrap();
        std::fs::write(tmp.path().join("vmlinuz-6.17.0"),        b"").unwrap();
        let pick = discover_xen_image_under(tmp.path()).unwrap();
        assert_eq!(pick, PathBuf::from("xen-4.20-amd64.gz"));
    }

    #[test]
    fn discover_xen_image_picks_higher_version() {
        // Two Xen versions installed (mid-upgrade). Pick the higher
        // version so the user gets the new one without manual selection.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("xen-4.20-amd64.gz"), b"").unwrap();
        std::fs::write(tmp.path().join("xen-4.21-amd64.gz"), b"").unwrap();
        let pick = discover_xen_image_under(tmp.path()).unwrap();
        assert_eq!(pick, PathBuf::from("xen-4.21-amd64.gz"));
    }

    #[test]
    fn discover_xen_image_returns_none_if_absent() {
        // No Xen installed (yet) — caller must surface a clear error
        // rather than us inventing a nonexistent path.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("vmlinuz-6.17.0"), b"").unwrap();
        assert!(discover_xen_image_under(tmp.path()).is_none());
    }
}
