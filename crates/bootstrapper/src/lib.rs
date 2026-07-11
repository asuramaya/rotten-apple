//! Phase-2 lift driver — v0.0.1 "host becomes dom0".
//!
//! Architectural decision (2026-05-04, user-explicit): the bottom layer
//! owns the hardware. Presenting hardware to guests (PV NICs, PV input,
//! PV framebuffer, GPU hot-swap) is the orchestrator's job, not the
//! lift's. Building those PV pipelines is months of work.
//!
//! For the first reboot we collapse the lift to its irreducible kernel:
//! the existing Ubuntu install BECOMES dom0. apt installs Xen, the
//! orchestrator runs as a systemd service inside that dom0, GRUB gets a
//! Xen entry alongside the bare-metal entry. Hardware "just works"
//! post-reboot because the same drivers run in the same kernel — only
//! the Xen layer is new.
//!
//! Trade-off: we don't get the "minimal dom0 / Ubuntu-as-domU" architecture
//! yet — that's the long-term goal but requires PV input + a viewer in
//! dom0 + Wi-Fi NAT plumbing first. v0.0.1 proves the orchestrator path
//! end-to-end against real Xen. Subsequent iterations build the PV
//! presentation layer; eventually the user's Ubuntu migrates to be a
//! domU and dom0 shrinks back to a broker.
//!
//! Steps:
//!   1. pre-flight (lift_readiness blockers refuse the lift)
//!   2. apt install xen-system-amd64 (pulls in the hypervisor + tools +
//!      the dom0-capable kernel ships in Ubuntu's stock kernel package)
//!   3. install orchestrator binary to /usr/local/bin
//!   4. install /etc/rotten-apple/active.toml
//!   5. install systemd unit + enable it
//!   6. tweak Xen cmdline via /etc/default/grub.d to set dom0_mem and
//!      dom0_max_vcpus (planner-derived from this host)
//!   7. update-grub (xen-system-amd64 added the Xen menuentry already)
//!   8. verify both bare-metal and Xen entries present
//!
//! Bare-metal Ubuntu stays the GRUB default for the first boot.

use std::path::Path;
use std::process::Command;

mod autowire;
pub use autowire::install_adjuncts;

pub mod boot_mode;
pub mod thin_dom0;
pub mod thin_dom0_efi;
pub mod thin_dom0_grub;
pub mod thin_dom0_manifest;
pub mod thin_dom0_rootfs;

// ---------------------------------------------------------------------------
// Error type for `install_system` and helpers below. Naming: still
// `LiftError` because `install` is conceptually a lightweight lift —
// the v0.0.1 LiftPlan that produced FatDom0 was removed in the
// 2026-05-06 ThinDom0 pivot; install-thindom0 owns the full install
// path now. This type stays for the binary-only install_system
// (used by `rotten-apple update`).

#[derive(Debug)]
pub enum LiftError {
    PreFlight(String),
    Command { step: &'static str, detail: String },
    Verify(String),
}

impl std::fmt::Display for LiftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiftError::PreFlight(s) => write!(f, "pre-flight failed: {s}"),
            LiftError::Command { step, detail } =>
                write!(f, "step '{step}' failed: {detail}"),
            LiftError::Verify(s)    => write!(f, "post-write verification failed: {s}"),
        }
    }
}

impl std::error::Error for LiftError {}

pub type Result<T> = std::result::Result<T, LiftError>;

// ---------------------------------------------------------------------------
// Lightweight install (no Xen touch — just put the binary on PATH).
//
// Exposed as `rotten-apple install` so the user can adopt the cockpit
// without committing to a full lift. Idempotent: running it twice has
// the same effect as once. Same code path the lift uses, so the install
// done at lift time is identical to what `install` produces.

pub fn install_system(cli_binary: &Path, dry_run: bool) -> Result<()> {
    if !cli_binary.exists() {
        return Err(LiftError::PreFlight(format!(
            "binary does not exist: {}", cli_binary.display())));
    }
    install_cli_binary(cli_binary, dry_run)?;
    install_desktop_launcher(dry_run)?;

    // Auto-glue: if the daemon and/or MCP server are sitting next to the
    // cli binary in the build output, install them too and register them
    // (systemd unit for the daemon, Claude Code config for MCP). Missing
    // adjuncts are skipped silently — the cli install never fails on
    // adjunct absence.
    let adjuncts = install_adjuncts(cli_binary, dry_run)?;

    // If the host is already set to boot straight into cockpit on tty1,
    // refresh that override from current code on every update so font
    // and systemd-shape fixes propagate without requiring a manual
    // `boot-mode cockpit` re-run.
    let cockpit_boot_refreshed =
        crate::boot_mode::refresh_cockpit_boot_override_if_present(dry_run)?;

    // GRUB cmdline drift: if a previous lift wrote
    // /etc/default/grub.d/40-rotten-apple.cfg, refresh it from the current
    // template and run update-grub if anything changed. This is how cmdline
    // bug fixes (the encrypted-disk-then-black-screen one, the iGPU
    // passthrough one) propagate to a host that already lifted, without
    // forcing a re-lift.
    let grub_changed = refresh_xen_cmdline_if_present(dry_run)?;

    // Post-install smoke test: the install promised the daemon would be
    // up. Verify by handshaking the socket. If this fails the user knows
    // immediately, before they discover it at cockpit-launch time.
    let smoke = if !dry_run && adjuncts.iter().any(|a| a.starts_with("orchestratord")) {
        Some(smoke_test_daemon())
    } else {
        None
    };

    eprintln!();
    eprintln!("==> install summary");
    eprintln!("    cli            → /usr/local/bin/rotten-apple");
    eprintln!("    desktop entry  → /usr/share/applications/rotten-apple.desktop");
    if !adjuncts.is_empty() {
        eprintln!("    auto-wired:");
        for a in &adjuncts {
            eprintln!("      · {a}");
        }
    }
    if cockpit_boot_refreshed {
        eprintln!("    cockpit boot   → tty1 override refreshed");
    }
    if grub_changed {
        eprintln!("    grub cmdline   → updated (reboot to apply)");
    }
    if let Some(report) = smoke {
        match report {
            Ok(s)  => eprintln!("    smoke test     → {s}"),
            Err(e) => {
                eprintln!("    smoke test     → FAILED: {e}");
                eprintln!();
                eprintln!("    The daemon is installed but the round-trip failed.");
                eprintln!("    Inspect: journalctl -u rotten-apple-orchestratord.service -n 60");
                return Err(LiftError::Verify(format!("post-install smoke: {e}")));
            }
        }
    }
    eprintln!();
    eprintln!("==> ready. Run `sudo rotten-apple cockpit` (or just `rotten-apple cockpit`");
    eprintln!("    once you're in the rotten-apple group, when group support lands).");
    Ok(())
}

/// Diff the on-disk Xen cmdline file against what the current bootstrapper
/// would write; rewrite + update-grub if drifted. Returns true when an
/// update happened. No-op if the file isn't present (host hasn't lifted).
fn refresh_xen_cmdline_if_present(dry_run: bool) -> Result<bool> {
    let path = Path::new("/etc/default/grub.d/40-rotten-apple.cfg");
    if !path.exists() {
        return Ok(false);
    }
    // Re-derive what we'd write today. The lift bakes dom0_mem and
    // dom0_max_vcpus from the host's planner — read them out of the
    // existing file so we don't accidentally re-plan with different
    // values on a host that's been running fine.
    let current = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    let (mem, mem_max, vcpus) = match parse_dom0_sizing_from_grub_snippet(&current) {
        Some(t) => t,
        None => return Ok(false),
    };
    let target = grub_xen_cmdline_snippet(mem, mem_max, vcpus);
    if current == target {
        return Ok(false);
    }
    if dry_run {
        eprintln!("    [grub refresh] would rewrite {} (drift detected)", path.display());
        return Ok(true);
    }
    std::fs::write(path, &target).map_err(|e|
        LiftError::Command { step: "refresh grub cmdline",
            detail: format!("write {}: {e}", path.display()) })?;
    let out = Command::new("update-grub").output().map_err(|e|
        LiftError::Command { step: "update-grub", detail: format!("spawn: {e}") })?;
    if !out.status.success() {
        return Err(LiftError::Command { step: "update-grub",
            detail: String::from_utf8_lossy(&out.stderr).into_owned() });
    }
    eprintln!("    [grub refresh] → {} updated; reboot to apply", path.display());
    Ok(true)
}

/// Pull `dom0_mem` (start), `max:` (live-expand ceiling), and
/// `dom0_max_vcpus` out of an existing grub.d snippet. Returns None when
/// the snippet doesn't match a recognized shape — in which case install
/// leaves it alone. Accepts both `M` and `G` suffixes for either size; the
/// returned values are always in MB.
fn parse_dom0_sizing_from_grub_snippet(s: &str) -> Option<(u64, u64, u32)> {
    let line = s.lines().find(|l| l.starts_with("GRUB_CMDLINE_XEN_DEFAULT="))?;
    // dom0_mem=<start>[MG],max:<max>[MG]
    let mem_field = line.split("dom0_mem=").nth(1)?
        .split_whitespace().next()?;
    let (start_tok, max_tok) = mem_field.split_once(",max:")?;
    let start_mb = parse_mem_token_to_mb(start_tok)?;
    let max_mb = parse_mem_token_to_mb(max_tok)?;
    let vcpus = line.split("dom0_max_vcpus=").nth(1)?
        .split_whitespace().next()?.parse::<u32>().ok()?;
    Some((start_mb, max_mb, vcpus))
}

/// Parse "<N>M" or "<N>G" → MB. Returns None on any other shape.
fn parse_mem_token_to_mb(tok: &str) -> Option<u64> {
    if let Some(n) = tok.strip_suffix('M') {
        n.parse::<u64>().ok()
    } else if let Some(n) = tok.strip_suffix('G') {
        n.parse::<u64>().ok().map(|g| g * 1024)
    } else {
        None
    }
}

/// Single source of truth for the Xen+dom0 GRUB cmdline. Used by the
/// install-time refresh path (refresh_xen_cmdline_if_present). Takes the
/// dom0 boot-target memory, the ballooning ceiling, and vcpu count
/// separately so the operator can run dom0 small at boot and grow it at
/// runtime via `xl mem-set Domain-0 <MB>` up to the ceiling.
pub(crate) fn grub_xen_cmdline_snippet(
    dom0_mem_mb: u64,
    dom0_mem_max_mb: u64,
    dom0_vcpus: u32,
) -> String {
    format!(r#"# Generated by rotten-apple bootstrapper.
# Sized from the existing on-disk file (drift-refreshed). Override by
# editing this file or /etc/default/grub directly, then `update-grub`.

# Xen hypervisor cmdline: dom0 footprint + visible console.
# `iommu=verbose,no-igfx`: enable IOMMU with verbose logging; exclude the
# Intel iGPU from IOMMU groups so dom0's i915 driver can drive it without
# Xen-side DMA translation. (Xen accepts `verbose` as an enable-flag —
# `iommu=verbose` is equivalent to `iommu=on,verbose`.)
# `dom0_mem={dom0_mem_mb}M,max:{dom0_mem_max_mb}M`: start at {dom0_mem_mb}M,
# allow ballooning up to {dom0_mem_max_mb}M via `xl mem-set Domain-0`.
GRUB_CMDLINE_XEN_DEFAULT="dom0_mem={dom0_mem_mb}M,max:{dom0_mem_max_mb}M dom0_max_vcpus={dom0_vcpus} dom0_vcpus_pin iommu=verbose,no-igfx console=vga loglvl=all guest_loglvl=all"

# Dom0 Linux kernel cmdline (XEN ENTRY ONLY — bare-metal entry untouched).
#
# Display caveat (2026-05-08): under Xen PV dom0, Xen consumes the EFI GOP
# handoff that bare-metal Linux uses to set up simpledrm. The dom0 kernel
# boots without an early framebuffer; tty0 has no display backend until
# i915 loads from the rootfs. LUKS prompts and early kernel printk render
# to a void. The pre-LUKS visibility problem is sidestepped via TPM2
# auto-unlock (see /etc/crypttab `tpm2-device=auto`); post-LUKS, gdm
# brings up its own framebuffer via i915 from the booted rootfs.
#
# `console=tty0`: target the physical console for any prompts that DO
# render (recovery shell, etc.). Last `console=` wins for /dev/console.
# Avoid `console=hvc0` — under this Xen 4.20 build it has caused boot
# deadlocks (2026-05-07).
# `quiet plymouth.use-mode=text`: keep boot quiet; plymouth in text mode
# as belt-and-braces if plymouth gets activated despite the splash drop.
# `iommu=pt intel_iommu=on amd_iommu=on`: passthrough mode (matches
# bare-metal Ubuntu's working cmdline, was `iommu=on` until 2026-05-08).
# Under Xen PV, `iommu=on` (force-on) caused dom0 to attempt IOMMU
# mappings that conflicted with Xen's `no-igfx` setting and the
# framebuffer never came up. `iommu=pt` keeps /sys/kernel/iommu_groups
# populated for dGPU passthrough detection without forcing translation
# on the iGPU.
GRUB_CMDLINE_LINUX_XEN_REPLACE_DEFAULT="console=tty0 quiet plymouth.use-mode=text iommu=pt intel_iommu=on amd_iommu=on"
"#)
}

/// Connect to the daemon socket, run the handshake, call host.info and
/// engine.status, and return a one-line summary on success or a clear
/// error on failure. Used at the end of install to fail loudly rather
/// than letting the user discover problems at cockpit-launch.
fn smoke_test_daemon() -> std::result::Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let path = "/run/rotten-apple.sock";
    // The daemon takes a moment after restart to bind the socket.
    // Poll up to 5 seconds before giving up.
    let mut stream = None;
    for _ in 0..50 {
        match UnixStream::connect(path) {
            Ok(s) => { stream = Some(s); break; }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    let stream = stream.ok_or_else(|| format!(
        "could not connect to {path} after 5s — is the daemon running?"))?;
    let read_stream = stream.try_clone()
        .map_err(|e| format!("clone stream: {e}"))?;
    let mut reader = BufReader::new(read_stream);
    let mut writer = stream;

    // hello
    writeln!(writer, r#"{{"jsonrpc":"2.0","method":"hello","params":{{"protocol_version":"0.1"}},"id":1}}"#)
        .map_err(|e| format!("write hello: {e}"))?;
    writer.flush().map_err(|e| format!("flush hello: {e}"))?;
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("read hello: {e}"))?;
    if !line.contains("\"protocol_version\":\"0.1\"") {
        return Err(format!("handshake mismatch: {}", line.trim()));
    }

    // host.info — also exercises the actor without requiring libxl
    // success (host.info responds even when the backend is unavailable)
    line.clear();
    writeln!(writer, r#"{{"jsonrpc":"2.0","method":"host.info","params":{{}},"id":2}}"#)
        .map_err(|e| format!("write host.info: {e}"))?;
    writer.flush().map_err(|e| format!("flush host.info: {e}"))?;
    reader.read_line(&mut line).map_err(|e| format!("read host.info: {e}"))?;
    let backend = if line.contains(r#""backend":"xen""#) { "xen" }
                  else if line.contains(r#""backend":"unavailable""#) { "unavailable (libxl not reachable)" }
                  else { "unknown" };

    Ok(format!("daemon up, backend={backend}"))
}

fn install_cli_binary(src: &Path, dry_run: bool) -> Result<()> {
    let dst = Path::new("/usr/local/bin/rotten-apple");
    if dry_run {
        eprintln!("    [install cli] would copy {} -> {} (chmod 755)",
                  src.display(), dst.display());
        return Ok(());
    }
    // Running `sudo rotten-apple update` from /usr/local/bin lands here
    // with src == dst. Linux refuses to fs::copy a running ELF onto
    // itself (ETXTBSY); even if it succeeded it'd be a no-op. Detect by
    // inode and skip — install still progresses to refresh adjuncts,
    // systemd, GRUB, smoke test.
    if same_inode(src, dst) {
        eprintln!("    [install cli] {} (already current — skipping copy)",
                  dst.display());
        return Ok(());
    }
    atomic_swap(src, dst, "install cli binary")?;
    eprintln!("    [install cli] → {}", dst.display());
    install_short_alias(dst);
    Ok(())
}

/// Atomically replace `dst` with the contents of `src`. Handles the
/// ETXTBSY case (a running process is executing `dst` — common when the
/// cockpit is open in another tty during update). Strategy:
///
///   1. Try plain `fs::copy(src, dst)`. Works in the common case.
///   2. On ETXTBSY: rename `dst` → `dst.old`, copy `src` → `dst`. Linux
///      lets you rename a file even while it's being executed; the
///      running process keeps its mmap'd inode, the new copy gets a
///      fresh inode at the canonical path, and the old inode is freed
///      when the running process exits.
///   3. Best-effort `unlink(dst.old)` so we don't leave debris.
///
/// chmod 0755 on the destination at the end. Used by all three binary
/// installers (cli, orchestratord, mcp).
pub(crate) fn atomic_swap(src: &Path, dst: &Path, step: &'static str) -> Result<()> {
    let mk_err = |action: &str, e: std::io::Error| LiftError::Command {
        step,
        detail: format!("{action} {} -> {}: {e}", src.display(), dst.display()),
    };
    match std::fs::copy(src, dst) {
        Ok(_) => {}
        Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
            let backup = dst.with_extension("old");
            // best-effort: if there's already a stale .old, drop it
            let _ = std::fs::remove_file(&backup);
            std::fs::rename(dst, &backup).map_err(|e| mk_err("rename", e))?;
            std::fs::copy(src, dst).map_err(|e| mk_err("copy after rename", e))?;
            // Don't fail if unlink of .old fails — running processes still
            // hold its inode; kernel frees it on their exit.
            let _ = std::fs::remove_file(&backup);
            eprintln!("    [{step}] (running instance was holding {} — rotated)",
                      dst.display());
        }
        Err(e) => return Err(mk_err("copy", e)),
    }
    let cstr = std::ffi::CString::new(dst.to_string_lossy().as_bytes()).unwrap();
    // SAFETY: standard libc chmod with stable args.
    unsafe { libc::chmod(cstr.as_ptr(), 0o755); }
    Ok(())
}

/// Drop a `ra` symlink alongside the canonical `rotten-apple` binary.
/// Saves typing 11 characters every command — meaningful when there's
/// no clipboard. Idempotent: stale symlink replaced; missing symlink
/// created. Real-binary-at-target case is left alone (unlikely; we don't
/// install our own binary as `ra`).
fn install_short_alias(target: &Path) {
    let alias = Path::new("/usr/local/bin/ra");
    // best-effort remove; if it's a symlink that's already correct we'll
    // see the readlink check below succeed and re-link cleanly anyway.
    let _ = std::fs::remove_file(alias);
    if let Err(e) = std::os::unix::fs::symlink(target, alias) {
        eprintln!("    [install alias] could not create {} → {}: {e}",
                  alias.display(), target.display());
        return;
    }
    eprintln!("    [install alias] → {} (-> {})", alias.display(), target.display());
}

/// True when two paths refer to the same file (same dev + inode). Returns
/// false if either path doesn't exist or stat fails — caller treats that
/// as "not the same" and proceeds with the copy.
pub(crate) fn same_inode(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

fn install_desktop_launcher(dry_run: bool) -> Result<()> {
    let desktop = r#"[Desktop Entry]
Version=1.0
Type=Application
Name=rotten-apple cockpit
GenericName=Hypervisor cockpit
Comment=Manage Xen domains, lift readiness, and lifecycle from a TUI
Exec=x-terminal-emulator -e "sudo rotten-apple cockpit"
Terminal=false
Icon=utilities-terminal
Categories=System;Settings;
StartupNotify=false
Keywords=xen;dom0;hypervisor;orchestrator;
"#;
    let path = Path::new("/usr/share/applications/rotten-apple.desktop");
    if dry_run {
        eprintln!("    [install desktop] would write {} ({} bytes)",
                  path.display(), desktop.len());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e|
            LiftError::Command { step: "install desktop launcher",
                detail: format!("mkdir {}: {}", parent.display(), e) })?;
    }
    std::fs::write(path, desktop).map_err(|e|
        LiftError::Command { step: "install desktop launcher",
            detail: format!("write {}: {}", path.display(), e) })?;
    eprintln!("    [install desktop] → {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grub_snippet_is_self_describing_round_trip() {
        // The drift detector parses dom0_mem, max:, and dom0_max_vcpus out
        // of a file produced by grub_xen_cmdline_snippet. If they disagree,
        // install would always think the snippet is drifted and re-run
        // update-grub on every install — mostly harmless but noisy. Pin them.
        let s = grub_xen_cmdline_snippet(2048, 8192, 4);
        let (mem, mem_max, vcpus) = parse_dom0_sizing_from_grub_snippet(&s).unwrap();
        assert_eq!(mem, 2048);
        assert_eq!(mem_max, 8192);
        assert_eq!(vcpus, 4);
    }

    #[test]
    fn grub_snippet_supports_live_expansion_max_above_start() {
        // The whole point of having a separate `max:` is that dom0 can
        // start small and balloon up at runtime. Ensure max != start
        // round-trips correctly through the formatter.
        let s = grub_xen_cmdline_snippet(4096, 32768, 20);
        assert!(s.contains("dom0_mem=4096M,max:32768M"),
            "snippet must allow max > start for live expansion: {s}");
    }

    #[test]
    fn grub_drift_parser_accepts_g_suffix() {
        // Operators (and our docs) use `4G` more often than `4096M`.
        // Both must round-trip through the parser.
        let s = "GRUB_CMDLINE_XEN_DEFAULT=\"dom0_mem=4G,max:32G dom0_max_vcpus=20 dom0_vcpus_pin\"\n";
        let (mem, mem_max, vcpus) = parse_dom0_sizing_from_grub_snippet(s).unwrap();
        assert_eq!(mem, 4096);
        assert_eq!(mem_max, 32768);
        assert_eq!(vcpus, 20);
    }

    #[test]
    fn grub_snippet_drops_console_hvc0() {
        // Black-screen regression: under this Xen 4.20 build, listing hvc0
        // on the dom0 cmdline has caused boot deadlocks. Stay tty0-only.
        // Check just the quoted cmdline value — comments legitimately
        // mention `hvc0` to document why we avoid it.
        let s = grub_xen_cmdline_snippet(1856, 1856, 4);
        let cmdline_value = s
            .lines()
            .find(|l| l.starts_with("GRUB_CMDLINE_LINUX_XEN_REPLACE_DEFAULT="))
            .expect("snippet must define cmdline line")
            .split('"').nth(1).expect("quoted value");
        assert!(!cmdline_value.contains("console=hvc0"),
            "dom0 cmdline must not include console=hvc0: {cmdline_value:?}");
        assert!(cmdline_value.contains("console=tty0"),
            "dom0 cmdline must keep console=tty0: {cmdline_value:?}");
    }

    #[test]
    fn grub_snippet_drops_splash_keeps_text_plymouth() {
        // Pre-TPM2 history: under Xen, plymouth's graphical splash claimed
        // the framebuffer and hid the LUKS passphrase prompt. Splash stays
        // out as belt-and-braces; TPM2 auto-unlock is the actual fix for
        // LUKS visibility (see /etc/crypttab `tpm2-device=auto`).
        let s = grub_xen_cmdline_snippet(1856, 1856, 4);
        let cmdline_value = s
            .lines()
            .find(|l| l.starts_with("GRUB_CMDLINE_LINUX_XEN_REPLACE_DEFAULT="))
            .expect("snippet must define GRUB_CMDLINE_LINUX_XEN_REPLACE_DEFAULT")
            .split('"')
            .nth(1)
            .expect("cmdline value should be quoted");
        assert!(!cmdline_value.contains("splash"),
            "Xen cmdline must NOT contain `splash`: {cmdline_value:?}");
        assert!(cmdline_value.contains("plymouth.use-mode=text"),
            "Xen cmdline must keep plymouth.use-mode=text as belt-and-braces: {cmdline_value:?}");
        assert!(cmdline_value.contains("quiet"),
            "Xen cmdline must keep `quiet` to suppress kernel chatter: {cmdline_value:?}");
        assert!(cmdline_value.contains("console=tty0"),
            "Xen cmdline must keep console=tty0 for the framebuffer: {cmdline_value:?}");
    }

    #[test]
    fn grub_snippet_uses_iommu_pt_not_force_on() {
        // Bare-metal Ubuntu boots cleanly with `iommu=pt`. Under Xen PV,
        // `iommu=on` (force-on) caused dom0 to attempt IOMMU mappings that
        // conflict with Xen's `no-igfx` setting and the framebuffer never
        // came up (2026-05-08). `iommu=pt` keeps /sys/kernel/iommu_groups
        // populated for dGPU passthrough detection without forcing
        // translation on the iGPU.
        let s = grub_xen_cmdline_snippet(1856, 1856, 4);
        let cmdline_value = s
            .lines()
            .find(|l| l.starts_with("GRUB_CMDLINE_LINUX_XEN_REPLACE_DEFAULT="))
            .expect("snippet must define cmdline line")
            .split('"').nth(1).expect("quoted value");
        // Token-exact check: `iommu=on` as its own whitespace-delimited
        // token. Substring would false-positive on `intel_iommu=on` and
        // `amd_iommu=on` which we keep as belt-and-braces.
        let tokens: Vec<&str> = cmdline_value.split_whitespace().collect();
        assert!(tokens.contains(&"iommu=pt"),
            "dom0 Linux cmdline must use iommu=pt: {cmdline_value:?}");
        assert!(!tokens.contains(&"iommu=on"),
            "dom0 Linux cmdline must not use iommu=on: {cmdline_value:?}");
        // Vendor-specific flags are belt-and-braces; harmless on the
        // wrong CPU because the kernel ignores the one that doesn't apply.
        assert!(cmdline_value.contains("intel_iommu=on"),
            "Xen cmdline must include intel_iommu=on: {cmdline_value:?}");
        assert!(cmdline_value.contains("amd_iommu=on"),
            "Xen cmdline must include amd_iommu=on: {cmdline_value:?}");
    }

    #[test]
    fn grub_snippet_includes_iommu_no_igfx() {
        // Intel iGPU mode-switch under Xen IOMMU is fragile. Pin the
        // workaround in the template.
        let s = grub_xen_cmdline_snippet(1856, 1856, 4);
        assert!(s.contains("iommu=verbose,no-igfx"),
            "GRUB snippet must keep iommu=verbose,no-igfx");
    }

    #[test]
    fn grub_drift_parser_returns_none_on_unrecognised_shape() {
        // Hand-edited or unrelated grub snippet: parser returns None, so
        // refresh_xen_cmdline_if_present leaves the file alone instead
        // of clobbering whatever the operator put there.
        let s = "GRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash\"\n";
        assert!(parse_dom0_sizing_from_grub_snippet(s).is_none());
    }

    #[test]
    fn grub_drift_parser_returns_none_on_missing_max_field() {
        // Old-shape (pre-2026-05-08) snippets without `,max:` no longer
        // parse — install leaves them alone rather than guessing the max.
        let s = "GRUB_CMDLINE_XEN_DEFAULT=\"dom0_mem=2048M dom0_max_vcpus=4 dom0_vcpus_pin\"\n";
        assert!(parse_dom0_sizing_from_grub_snippet(s).is_none());
    }

    #[test]
    fn same_inode_self_is_true() {
        // Pin: a real file is same_inode with itself. Used to short-
        // circuit copy-onto-self when `sudo rotten-apple update` is run
        // from /usr/local/bin (the installed location).
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(same_inode(f.path(), f.path()));
    }

    #[test]
    fn same_inode_distinct_files_is_false() {
        let a = tempfile::NamedTempFile::new().unwrap();
        let b = tempfile::NamedTempFile::new().unwrap();
        assert!(!same_inode(a.path(), b.path()));
    }

    #[test]
    fn same_inode_missing_path_is_false() {
        // If either side fails to stat we treat as not-same and proceed.
        let a = tempfile::NamedTempFile::new().unwrap();
        let nope = std::path::Path::new("/definitely/not/a/real/path/zzz");
        assert!(!same_inode(a.path(), nope));
    }
}
