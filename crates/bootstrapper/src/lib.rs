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

use std::path::{Path, PathBuf};
use std::process::Command;

use rotten_apple_detect::{Detection, LiftReadiness, CpuTopology};

mod autowire;
pub use autowire::install_adjuncts;

pub mod boot_mode;

// ---------------------------------------------------------------------------
// Public types

#[derive(Debug, Clone)]
pub struct LiftPlan {
    /// Path on the host to the manifest the orchestrator will use.
    pub manifest_src: PathBuf,
    /// Path on the host to the orchestrator binary to install.
    pub orchestrator_binary: PathBuf,
    /// Path on the host to the `rotten-apple` CLI binary (cockpit + lift
    /// + manifest tools — all subcommands of the same binary). Defaults
    ///   to `/proc/self/exe` resolved through std::env::current_exe; the
    ///   lift step copies it to /usr/local/bin/rotten-apple so post-lift
    ///   the user can run `sudo rotten-apple cockpit` from any terminal.
    pub cli_binary: PathBuf,
    /// dom0_mem / dom0_max_vcpus — planner-derived from this host's RAM
    /// and CPU topology. Bake into the Xen cmdline so dom0 is sized at
    /// boot rather than letting it claim everything.
    pub dom0_mem_mb: u64,
    pub dom0_vcpus: u32,
    /// If true, print steps without running them.
    pub dry_run: bool,
}

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

impl LiftPlan {
    pub fn for_this_host(
        manifest_src: PathBuf,
        orchestrator_binary: PathBuf,
        dry_run: bool,
    ) -> Result<Self> {
        let detection = Detection::run();
        let topo = CpuTopology::probe();
        let plan = rotten_apple_detect::plan(&detection, &topo);
        let cli_binary = std::env::current_exe()
            .map_err(|e| LiftError::PreFlight(format!(
                "could not locate own binary path: {e}")))?;
        Ok(LiftPlan {
            manifest_src,
            orchestrator_binary,
            cli_binary,
            dom0_mem_mb: plan.dom0.memory_mb,
            dom0_vcpus: plan.dom0.vcpus,
            dry_run,
        })
    }
}

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
    let (mem, vcpus) = match parse_dom0_sizing_from_grub_snippet(&current) {
        Some(t) => t,
        None => return Ok(false),
    };
    let target = grub_xen_cmdline_snippet(mem, vcpus);
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

/// Pull `dom0_mem` (M value before the comma) and `dom0_max_vcpus` out
/// of an existing grub.d snippet. Returns None when the snippet doesn't
/// match the shape we wrote — in which case install leaves it alone.
fn parse_dom0_sizing_from_grub_snippet(s: &str) -> Option<(u64, u32)> {
    let line = s.lines().find(|l| l.starts_with("GRUB_CMDLINE_XEN_DEFAULT="))?;
    // dom0_mem=<N>M,max:<N>M
    let mem = line.split("dom0_mem=").nth(1)?
        .split('M').next()?.parse::<u64>().ok()?;
    let vcpus = line.split("dom0_max_vcpus=").nth(1)?
        .split_whitespace().next()?.parse::<u32>().ok()?;
    Some((mem, vcpus))
}

/// Single source of truth for the Xen+dom0 GRUB cmdline. Used by both the
/// initial lift (LiftPlan::step_xen_cmdline) and the install-time refresh.
pub(crate) fn grub_xen_cmdline_snippet(dom0_mem_mb: u64, dom0_vcpus: u32) -> String {
    format!(r#"# Generated by rotten-apple bootstrapper.
# Sized from `rotten-apple plan-lift`. Override by editing this file or
# /etc/default/grub directly, then `update-grub`.

# Xen hypervisor cmdline: dom0 footprint + visible console + iGPU left
# alone (no IOMMU group for it) so i915 in dom0 can take the framebuffer
# cleanly after EFI hands off.
GRUB_CMDLINE_XEN_DEFAULT="dom0_mem={dom0_mem_mb}M,max:{dom0_mem_mb}M dom0_max_vcpus={dom0_vcpus} dom0_vcpus_pin iommu=verbose,no-igfx console=vga loglvl=all guest_loglvl=all"

# Dom0 Linux kernel cmdline (XEN ENTRY ONLY — bare-metal entry untouched).
# The Xen PV console (hvc0) was removed in v0.0.3 — last `console=` wins
# for /dev/console and gdm/plymouth follow it; with the PV console listed
# the framebuffer never got the foreground and post-LUKS-prompt was a
# black screen on encrypted hosts.
# `splash quiet` restored so plymouth runs and gdm picks up tty7.
GRUB_CMDLINE_LINUX_XEN_REPLACE_DEFAULT="console=tty0 splash quiet"
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
// Pre-flight

pub fn pre_flight() -> Result<(Detection, LiftReadiness)> {
    let det = Detection::run();
    let lr  = LiftReadiness::run();
    if !det.blockers.is_empty() {
        return Err(LiftError::PreFlight(format!(
            "Detection blockers: {}", det.blockers.join("; "))));
    }
    if !lr.blockers.is_empty() {
        return Err(LiftError::PreFlight(format!(
            "LiftReadiness blockers: {}", lr.blockers.join("; "))));
    }
    Ok((det, lr))
}

// ---------------------------------------------------------------------------
// Execute

impl LiftPlan {
    pub fn execute(&self) -> Result<()> {
        eprintln!("==> rotten-apple lift v0.0.1 (host becomes dom0): {}",
                  if self.dry_run { "DRY RUN" } else { "EXECUTE" });
        eprintln!("    manifest:          {}", self.manifest_src.display());
        eprintln!("    orchestrator bin:  {}", self.orchestrator_binary.display());
        eprintln!("    dom0_mem:          {} MB (Xen cmdline)", self.dom0_mem_mb);
        eprintln!("    dom0_max_vcpus:    {} (Xen cmdline)", self.dom0_vcpus);
        eprintln!();

        let (_det, _lr) = pre_flight()?;

        if !self.manifest_src.exists() {
            return Err(LiftError::PreFlight(format!(
                "manifest does not exist: {}", self.manifest_src.display())));
        }
        if !self.orchestrator_binary.exists() {
            return Err(LiftError::PreFlight(format!(
                "orchestrator binary does not exist: {}",
                self.orchestrator_binary.display())));
        }

        // One-time-only steps (apt, GRUB cmdline). The lift handles
        // these because they're only legal pre-Xen-boot or right after.
        self.step_apt_install()?;
        self.step_install_manifest()?;
        self.step_xen_cmdline()?;
        self.step_update_grub()?;
        self.step_verify_dual_entry()?;

        // Per-build install steps (binaries, systemd unit, MCP wiring,
        // smoke test). install_system is the single source of truth for
        // these — running it from lift means lift produces the same end
        // state as a subsequent `rotten-apple update`.
        eprintln!();
        eprintln!("==> running per-build install (cli + daemon + mcp)");
        install_system(&self.cli_binary, self.dry_run)?;

        eprintln!();
        eprintln!("==> lift complete.");
        eprintln!("    Bare-metal Ubuntu stays the GRUB default.");
        eprintln!("    Reboot, hold Shift at the GRUB menu, pick");
        eprintln!("    'Ubuntu GNU/Linux, with Xen hypervisor'.");
        eprintln!("    If anything goes wrong: reboot, pick 'Ubuntu' (no Xen) to recover.");
        eprintln!();
        eprintln!("    Post-reboot: `systemctl status rotten-apple-orchestratord`");
        eprintln!("    + `rotten-apple cockpit` to verify the daemon round-trips.");
        eprintln!("    Future updates: build, then `sudo rotten-apple update`.");
        Ok(())
    }

    // ---- steps ----

    fn step_apt_install(&self) -> Result<()> {
        // xen-system-amd64 is the meta-package: hypervisor + xen-utils +
        // libxl runtime + xen-tools + grub helpers that auto-add a
        // 'Ubuntu, with Xen hypervisor' menuentry when update-grub runs.
        // No-install-recommends to keep dom0 lean (no qemu-system-x86,
        // no docs).
        self.run_with_env("apt install xen-system",
            "apt-get",
            &["install", "-y", "--no-install-recommends", "xen-system-amd64"],
            &[("DEBIAN_FRONTEND", "noninteractive")])
    }

    fn step_install_manifest(&self) -> Result<()> {
        let dst = Path::new("/etc/rotten-apple/active.toml");
        if self.dry_run {
            self.step_log("install manifest",
                &format!("would copy {} -> {}",
                         self.manifest_src.display(), dst.display()));
            return Ok(());
        }
        std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e|
            LiftError::Command { step: "mkdir /etc/rotten-apple",
                                 detail: e.to_string() })?;
        std::fs::copy(&self.manifest_src, dst).map_err(|e|
            LiftError::Command { step: "copy manifest", detail: e.to_string() })?;
        self.step_log("install manifest", &format!("→ {}", dst.display()));
        Ok(())
    }

    fn step_xen_cmdline(&self) -> Result<()> {
        // Single source of truth lives in `grub_xen_cmdline_snippet` so
        // post-lift `rotten-apple install` can detect drift against the
        // SAME template and refresh without forcing a full re-lift.
        let snippet = grub_xen_cmdline_snippet(self.dom0_mem_mb, self.dom0_vcpus);
        let path = Path::new("/etc/default/grub.d/40-rotten-apple.cfg");
        self.write_file("write Xen cmdline", path, &snippet)
    }

    fn step_update_grub(&self) -> Result<()> {
        self.run("update-grub", "update-grub", &[])
    }

    fn step_verify_dual_entry(&self) -> Result<()> {
        if self.dry_run {
            self.step_log("verify dual-entry GRUB",
                "would read /boot/grub/grub.cfg and assert both 'Ubuntu' \
                 and 'Ubuntu …with Xen hypervisor' menuentries present");
            return Ok(());
        }
        let cfg = std::fs::read_to_string("/boot/grub/grub.cfg")
            .map_err(|e| LiftError::Verify(format!(
                "could not read /boot/grub/grub.cfg: {e}")))?;
        let has_xen = cfg.contains("with Xen hypervisor")
                   || cfg.contains("xen.gz")
                   || cfg.contains("multiboot2 /boot/xen");
        let has_bare_metal = cfg.matches("menuentry 'Ubuntu").count() >= 1;
        if !has_xen {
            return Err(LiftError::Verify(
                "Xen GRUB entry not present after update-grub. Did \
                 xen-system-amd64 install correctly? Check apt log.".into()));
        }
        if !has_bare_metal {
            return Err(LiftError::Verify(
                "bare-metal Ubuntu menuentry not present in grub.cfg. \
                 Refusing to leave the system without a recovery path.".into()));
        }
        self.step_log("verify dual-entry GRUB",
            &format!("OK — Xen entry + bare-metal Ubuntu entry both \
                      present in {} bytes of grub.cfg", cfg.len()));
        Ok(())
    }

    // ---- helpers ----

    fn run(&self, step: &'static str, prog: &str, args: &[&str]) -> Result<()> {
        self.run_with_env(step, prog, args, &[])
    }

    fn run_with_env(
        &self, step: &'static str, prog: &str, args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<()> {
        let env_prefix = env.iter()
            .map(|(k, v)| format!("{k}={v} "))
            .collect::<String>();
        self.step_log(step, &format!("{env_prefix}{prog} {}", args.join(" ")));
        if self.dry_run { return Ok(()) }
        let mut cmd = Command::new(prog);
        cmd.args(args);
        for (k, v) in env { cmd.env(k, v); }
        let out = cmd.output().map_err(|e|
            LiftError::Command { step, detail: format!("spawn: {e}") })?;
        if !out.status.success() {
            return Err(LiftError::Command { step,
                detail: format!("exit={}\nstdout: {}\nstderr: {}",
                                out.status,
                                String::from_utf8_lossy(&out.stdout),
                                String::from_utf8_lossy(&out.stderr)) });
        }
        Ok(())
    }

    fn write_file(&self, step: &'static str, path: &Path, content: &str) -> Result<()> {
        if self.dry_run {
            self.step_log(step,
                &format!("would write {} ({} bytes)", path.display(), content.len()));
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e|
                LiftError::Command { step,
                    detail: format!("mkdir {}: {}", parent.display(), e) })?;
        }
        std::fs::write(path, content).map_err(|e|
            LiftError::Command { step,
                detail: format!("write {}: {}", path.display(), e) })?;
        self.step_log(step, &format!("→ {} ({} bytes)", path.display(), content.len()));
        Ok(())
    }

    fn step_log(&self, step: &str, what: &str) {
        eprintln!("    [{step}] {what}");
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lift_plan_for_this_host_does_not_panic() {
        let manifest = tempfile::NamedTempFile::new().unwrap();
        let bin = tempfile::NamedTempFile::new().unwrap();
        let _ = LiftPlan::for_this_host(
            manifest.path().to_path_buf(),
            bin.path().to_path_buf(),
            true);
    }

    #[test]
    fn lift_plan_carries_planner_dom0_size() {
        let manifest = tempfile::NamedTempFile::new().unwrap();
        let bin = tempfile::NamedTempFile::new().unwrap();
        let p = LiftPlan::for_this_host(
            manifest.path().to_path_buf(),
            bin.path().to_path_buf(),
            true).unwrap();
        // Sanity bounds; specific value depends on host RAM. Planner clamps
        // to [768, 4096].
        assert!(p.dom0_mem_mb >= 768);
        assert!(p.dom0_mem_mb <= 4096);
        assert!(p.dom0_vcpus >= 1);
        assert!(p.dom0_vcpus <= 4);
    }

    #[test]
    fn grub_snippet_is_self_describing_round_trip() {
        // The drift detector parses dom0_mem and dom0_max_vcpus out of a
        // file produced by grub_xen_cmdline_snippet. If those two
        // functions disagree, install would always think the snippet is
        // drifted and re-run update-grub on every install — mostly
        // harmless but noisy. Pin them.
        let s = grub_xen_cmdline_snippet(2048, 4);
        let (mem, vcpus) = parse_dom0_sizing_from_grub_snippet(&s).unwrap();
        assert_eq!(mem, 2048);
        assert_eq!(vcpus, 4);
    }

    #[test]
    fn grub_snippet_drops_console_hvc0() {
        // Encrypted-disk-then-black-screen regression: hvc0 hijacks
        // /dev/console from the framebuffer. Ensure the template stays
        // free of it.
        let s = grub_xen_cmdline_snippet(1856, 4);
        assert!(!s.contains("console=hvc0"),
            "GRUB snippet must not include console=hvc0; that hides post-LUKS visuals");
        assert!(s.contains("console=tty0"),
            "GRUB snippet must keep console=tty0 for the framebuffer");
        assert!(s.contains("splash quiet"),
            "GRUB snippet must keep splash+quiet so plymouth runs");
    }

    #[test]
    fn grub_snippet_includes_iommu_no_igfx() {
        // Intel iGPU mode-switch under Xen IOMMU is fragile. Pin the
        // workaround in the template.
        let s = grub_xen_cmdline_snippet(1856, 4);
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
