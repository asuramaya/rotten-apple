//! rotten-apple CLI.
//!
//! One binary, multiple subcommands. Subcommands grow as crates land
//! (`detect` and `manifest validate` ship with this skeleton; `lift`,
//! `unlift`, `verify`, `wizard`, `harness`, `orchestrator` follow per
//! the roadmap).

use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rotten_apple_backend::HypervisorBackend;
use rotten_apple_backend_xen::XenBackend;
use rotten_apple_bootstrapper::boot_mode;
use rotten_apple_detect::{CpuTopology, Detection, LiftReadiness, plan};
use rotten_apple_manifest::{BackendCapabilities, Profile};

#[derive(Parser)]
#[command(
    name = "rotten-apple",
    version,
    about = "Hypervisor-agnostic resource orchestrator for Linux + Windows guests.",
    long_about = "rotten-apple lifts a stock Ubuntu install onto a thin hypervisor \
                  (Xen today, Hyper-V later) and presents Linux + Windows + appliance \
                  guests as a unified desktop. See design/architecture.md for the \
                  contract; ROADMAP.md for what's done and next."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Inspect host state: distro, UEFI, IOMMU, GPU, disk space, etc.
    Detect,

    /// Print this node's mesh identity: NodeId + Ed25519 pubkey
    /// fingerprint. Bootstraps the keypair on first run (writes to
    /// /var/lib/rotten-apple/node.key, mode 0600). Use this to wire
    /// up `[[peers]]` entries on other nodes — paste the hex pubkey
    /// into their mesh.toml.
    NodeInfo {
        /// Override key path (default: /var/lib/rotten-apple/node.key).
        /// Useful in tests / dev — production should use the default.
        #[arg(long)]
        key_path: Option<PathBuf>,
        /// Override node-id file path (default: /etc/rotten-apple/node-id).
        #[arg(long)]
        node_id_path: Option<PathBuf>,
        /// Print the full pubkey hex in addition to the short fingerprint.
        /// Required when registering this node in another node's mesh.toml
        /// under `trust.mode = "config"`.
        #[arg(long)]
        full_pubkey: bool,
    },

    /// Lift-specific probes: partition layout, LUKS, IOMMU groups, GRUB.
    LiftReadiness,

    /// Print what `lift` WOULD do on this host, without touching anything.
    /// Reads /proc/self/mountinfo, /dev/disk/by-uuid, and the host's
    /// running kernel to fill in the install plan, then renders it.
    /// Includes the recommended dom0/domU resource split.
    PlanLift,

    /// Execute the lift: turn this Ubuntu install into a ThinDom0 host.
    /// Builds a tiny dom0 cpio.gz (mmdebstrap + busybox + xen-tools +
    /// rotten-apple), drops a GRUB entry that boots Xen + that cpio.gz,
    /// stages the user-desktop guest manifest. Defaults to dry-run.
    /// Bare-metal Ubuntu menuentry is preserved as the recovery path.
    Lift {
        /// Actually run the steps. Without this, prints the steps and exits.
        #[arg(long)]
        execute: bool,
        /// Recovery mode: skip apt-install + rootfs build + kernel copy.
        /// Just rewrite /etc/grub.d/41_rotten_apple_thindom0, run
        /// update-grub, and verify dual entries. Use after a previous
        /// `--execute` got far enough to land the cpio.gz but the GRUB
        /// step bombed (e.g. a renderer bug). Implies `--execute`.
        #[arg(long)]
        grub_only: bool,
    },

    /// Operate on profile manifests.
    Manifest {
        #[command(subcommand)]
        action: ManifestAction,
    },

    /// Xen backend smoke tests — talk to libxl directly.
    Xen {
        #[command(subcommand)]
        action: XenAction,
    },

    /// Manage the local image catalog: pull cloud images, list, remove.
    /// The catalog lives at /var/lib/rotten-apple/images/index.toml and
    /// is the input to `instance new` (separate command).
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },

    /// Manage instances: a base image plus a copy-on-write overlay equals
    /// a domain that can be created, started, destroyed, forked without
    /// touching the base. Sub-second create.
    Instance {
        #[command(subcommand)]
        action: InstanceAction,
    },

    /// Install or update rotten-apple system-wide. Idempotent and
    /// self-verifying: detects what's already done, only does what's
    /// needed, ends with a daemon round-trip smoke test. Use this for
    /// every build update — `rotten-apple update` is an alias.
    ///
    /// Steps (each conditional):
    ///   · cli + orchestratord + mcp binaries → /usr/local/bin
    ///   · desktop launcher
    ///   · legacy --check systemd unit removed
    ///   · daemon stopped, binary swapped, restarted
    ///   · ~/.claude.json gets mcpServers.rotten-apple (created if absent)
    ///   · GRUB cmdline refreshed if drifted (update-grub if so)
    ///   · daemon round-trip verified
    Install {
        /// Print steps without acting.
        #[arg(long)]
        dry_run: bool,
    },

    /// Alias for `install` — same code path, intent-explicit name for
    /// "I'm refreshing a working install with a new build."
    Update {
        /// Print steps without acting.
        #[arg(long)]
        dry_run: bool,
    },

    /// Interactive TUI cockpit. Auto-detects host state: shows the
    /// pre-lift inspection + lift action on a virgin Ubuntu, an
    /// awaiting-reboot screen if Xen is installed but not booted,
    /// or the live domain UI in dom0.
    Cockpit {
        /// Manifest used by the [c]reate keybinding (Active mode) and
        /// installed into dom0 by the lift (PreLift mode).
        #[arg(long, default_value = "/etc/rotten-apple/active.toml")]
        manifest: PathBuf,
        /// Path to the orchestrator binary the lift will install into
        /// dom0. If omitted, the cockpit looks next to itself.
        #[arg(long)]
        orchestrator: Option<PathBuf>,
    },

    /// Toggle the next-boot UI between the standard Ubuntu desktop and
    /// rotten-apple's cockpit on tty1. Reversible. Reboot to apply.
    ///
    ///   cockpit  — mask the display manager + drop a getty@tty1 override
    ///              that execs `rotten-apple cockpit` directly.
    ///   desktop  — undo the above; restore normal boot.
    ///   status   — print the current setting (no side-effects).
    BootMode {
        /// Which boot mode to set, or `status` to query.
        target: BootModeArg,
        /// After a successful cockpit/desktop change, run `systemctl reboot`.
        /// One-command "flip and go" — typing-friendly when no clipboard.
        #[arg(long)]
        reboot: bool,
        /// Console font loaded by `setfont` before cockpit starts on tty1.
        /// Default `Uni3-TerminusBold32x16` is large + Unicode-capable;
        /// good for 4K. Try `Uni3-TerminusBold24x12` on 1080p, or
        /// `Lat15-TerminusBold20x10` if the default is too big. Only
        /// meaningful with the `cockpit` target.
        #[arg(long, default_value = "Uni3-TerminusBold32x16")]
        font: String,
    },

    /// One-command "I just changed code, push the new build to my host".
    /// Runs `cargo build --workspace --release` from the source dir, then
    /// re-execs as `sudo rotten-apple update` to install the fresh binary.
    /// Single password prompt; fewer keystrokes than the manual sequence.
    Rebuild {
        /// Path to the workspace root. Default: walk up from CWD looking
        /// for the workspace's Cargo.toml. Fails clearly if not found.
        #[arg(long)]
        source: Option<PathBuf>,
    },

    /// One-command panic-button: undo cockpit-boot mode, unmask gdm,
    /// start the graphical session. Use this to escape a stranded
    /// cockpit-on-tty1 state without remembering the three-step dance.
    Recover,

    /// Print a non-TUI diagnostic of what cockpit-boot would do right
    /// now: detection summary, daemon socket reachability, libxl status,
    /// last few cockpit boot-log lines. Safe to run from any shell —
    /// useful from a recovery getty when cockpit's TUI itself fails.
    CockpitDiag,

    /// dom0 memory governor loop (internal — spawned from /init on a
    /// ThinDom0). dom0 boots high enough to unpack the initramfs, then this
    /// balloons Domain-0 down to a steady floor and back up on demand, using
    /// `xl mem-set 0`. Runs until killed. Defaults mirror the ThinDom0
    /// doctrine (1 GiB steady) and the `dom0_mem=...,max:` cmdline ceiling;
    /// keep `--max-mb` in sync with that ceiling.
    Dom0Memd {
        #[arg(long, default_value_t = 1024)]
        steady_mb: u64,
        #[arg(long, default_value_t = 4096)]
        max_mb: u64,
        #[arg(long, default_value_t = 256)]
        per_guest_mb: u64,
        #[arg(long, default_value_t = 12)]
        low_watermark_pct: u64,
        #[arg(long, default_value_t = 30)]
        high_watermark_pct: u64,
        #[arg(long, default_value_t = 256)]
        step_mb: u64,
        #[arg(long, default_value_t = 128)]
        dead_band_mb: u64,
        #[arg(long, default_value_t = 2)]
        tick_secs: u64,
    },
}

/// CLI-side enum for `rotten-apple boot-mode <…>`. Mirrors
/// `boot_mode::BootMode` but adds a `Status` query variant. Kept in
/// main.rs because clap's ValueEnum derive lives at the binary edge.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum BootModeArg {
    /// Mask the DM + override getty@tty1 to spawn the cockpit.
    Cockpit,
    /// Restore normal Ubuntu desktop boot.
    Desktop,
    /// Print the current boot mode without changing it.
    Status,
}

#[derive(Subcommand)]
enum XenAction {
    /// List domains visible to dom0 via libxl.
    List,

    /// Attempt to create a guest from a manifest. Will fail with a typed
    /// error on non-Xen hosts; useful for proving the path is wired
    /// before the lift.
    TryCreate {
        /// Path to a manifest TOML file.
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ManifestAction {
    /// Load a manifest TOML and validate against reference backend caps.
    Validate {
        /// Path to a manifest TOML file.
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ImageAction {
    /// Print the catalog's current entries.
    List,

    /// Download a known cloud image and register it in the catalog.
    /// Names are shorthand like `ubuntu:24.04`; see `image list-known`.
    Pull {
        /// Source shorthand (e.g. `ubuntu:24.04`, `debian:12`).
        name: String,
        /// Print the curl invocation without downloading.
        #[arg(long)]
        dry_run: bool,
    },

    /// Remove an entry from the catalog and unlink its backing file.
    Rm {
        /// Catalog entry name (the `name` field in index.toml).
        name: String,
    },

    /// Print the hard-coded list of supported sources.
    ListKnown,
}

#[derive(Subcommand)]
enum InstanceAction {
    /// Create a new instance: write a CoW overlay on top of <base>, drop
    /// a generated Profile manifest into /etc/rotten-apple/manifests.d/,
    /// then ask orchestratord to start it. Use `--no-start` to skip the
    /// daemon hop and start the instance later.
    New {
        /// Instance id — also the manifest filename and registry key.
        id: String,
        /// Base image catalog entry (must exist; pull with `image pull`).
        #[arg(long)]
        base: String,
        /// Memory in MB. Defaults to 4096.
        #[arg(long)]
        memory: Option<u64>,
        /// Virtual CPU count. Defaults to 2.
        #[arg(long)]
        vcpus: Option<u32>,
        /// Mark for auto-cleanup on shutdown.
        #[arg(long)]
        ephemeral: bool,
        /// Print steps without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the post-create domain.create call to the daemon.
        #[arg(long)]
        no_start: bool,
    },

    /// Fork an existing instance: create a child overlay backed by the
    /// parent's overlay. The parent SHOULD NOT be running at the moment
    /// of fork — qcow2 backing-file integrity requires the parent
    /// quiesced.
    Fork {
        parent_id: String,
        new_id: String,
        /// Print steps without writing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Destroy an instance: unlink overlay + manifest, drop from registry.
    Rm {
        id: String,
        /// Print steps without acting.
        #[arg(long)]
        dry_run: bool,
    },

    /// List registered instances.
    List,

    /// Start an existing instance via the daemon (`domain.create` against
    /// the generated manifest). Convenience for the `--no-start` path.
    Start {
        id: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Detect => cmd_detect(),
        Commands::NodeInfo { key_path, node_id_path, full_pubkey } =>
            cmd_node_info(key_path, node_id_path, full_pubkey),
        Commands::LiftReadiness => cmd_lift_readiness(),
        Commands::PlanLift => cmd_plan_lift(),
        Commands::Lift { execute, grub_only } =>
            cmd_lift(!execute && !grub_only, grub_only),
        Commands::Manifest { action } => match action {
            ManifestAction::Validate { path } => cmd_manifest_validate(&path),
        },
        Commands::Xen { action } => match action {
            XenAction::List              => cmd_xen_list(),
            XenAction::TryCreate { path } => cmd_xen_try_create(&path),
        },
        Commands::Image { action } => match action {
            ImageAction::List              => cmd_image_list(),
            ImageAction::Pull { name, dry_run } => cmd_image_pull(&name, dry_run),
            ImageAction::Rm { name }       => cmd_image_rm(&name),
            ImageAction::ListKnown         => cmd_image_list_known(),
        },
        Commands::Instance { action } => match action {
            InstanceAction::New {
                id, base, memory, vcpus, ephemeral, dry_run, no_start,
            } => cmd_instance_new(
                id, base, memory, vcpus, ephemeral, dry_run, no_start),
            InstanceAction::Fork { parent_id, new_id, dry_run } =>
                cmd_instance_fork(&parent_id, &new_id, dry_run),
            InstanceAction::Rm { id, dry_run } =>
                cmd_instance_rm(&id, dry_run),
            InstanceAction::List => cmd_instance_list(),
            InstanceAction::Start { id } => cmd_instance_start(&id),
        },
        Commands::Install { dry_run } => cmd_install(dry_run),
        Commands::Update  { dry_run } => cmd_install(dry_run),
        Commands::Cockpit { manifest, orchestrator } =>
            cmd_cockpit(manifest, orchestrator),
        Commands::BootMode { target, reboot, font } =>
            cmd_boot_mode(target, reboot, font),
        Commands::Rebuild { source } => cmd_rebuild(source),
        Commands::Recover => cmd_recover(),
        Commands::CockpitDiag => cmd_cockpit_diag(),
        Commands::Dom0Memd {
            steady_mb, max_mb, per_guest_mb, low_watermark_pct,
            high_watermark_pct, step_mb, dead_band_mb, tick_secs,
        } => cmd_dom0_memd(
            steady_mb, max_mb, per_guest_mb, low_watermark_pct,
            high_watermark_pct, step_mb, dead_band_mb, tick_secs),
    }
}

/// `rotten-apple dom0-memd` — run the dom0 memory governor. Returns only on a
/// fatal error (loop is otherwise infinite); a dead governor leaves dom0 at
/// its last size, which is safe, so /init does not respawn it.
#[allow(clippy::too_many_arguments)]
fn cmd_dom0_memd(
    steady_mb: u64, max_mb: u64, per_guest_mb: u64, low_watermark_pct: u64,
    high_watermark_pct: u64, step_mb: u64, dead_band_mb: u64, tick_secs: u64,
) -> ExitCode {
    let cfg = rotten_apple_dom0_memd::GovernorConfig {
        steady_mb, max_mb, per_guest_mb, low_watermark_pct,
        high_watermark_pct, step_mb, dead_band_mb,
        tick: std::time::Duration::from_secs(tick_secs),
    };
    match rotten_apple_dom0_memd::run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dom0-memd: fatal: {e}");
            ExitCode::from(1)
        }
    }
}

/// Non-TUI diagnostic: tells you what cockpit-boot WOULD see right now,
/// without launching the TUI. Designed to be runnable from a recovery
/// getty when the cockpit itself can't start.
fn cmd_cockpit_diag() -> ExitCode {
    use std::io::Read;
    println!("==> rotten-apple cockpit-diag");
    let det = rotten_apple_detect::Detection::run();
    println!();
    println!("host detection");
    println!("  kernel              {}", det.kernel);
    println!("  arch                {}", det.arch);
    println!("  distro              {} {}", det.distro_id, det.distro_version);
    println!("  running_under_xen   {}", det.running_under_xen);
    println!("  xen_already_installed {}", det.xen_already_installed);
    println!("  is_uefi             {}", det.is_uefi);
    println!("  iommu_in_cmdline    {}", det.iommu_in_cmdline);

    println!();
    println!("daemon transports");
    let sock = std::path::Path::new("/run/rotten-apple.sock");
    if sock.exists() {
        match std::os::unix::net::UnixStream::connect(sock) {
            Ok(_) => println!("  /run/rotten-apple.sock     reachable"),
            Err(e) => println!("  /run/rotten-apple.sock     UNREACHABLE: {e}"),
        }
    } else {
        println!("  /run/rotten-apple.sock     missing (orchestratord not started?)");
    }
    match rotten_apple_cockpit::daemon_client::DaemonClient::connect_default() {
        Ok(mut c) => match c.handshake() {
            Ok(()) => println!("  daemon default path        reachable (unix or vsock host fallback)"),
            Err(e) => println!("  daemon default path        HANDSHAKE FAILED: {e}"),
        }
        Err(e) => println!("  daemon default path        UNREACHABLE: {e}"),
    }
    let unit_state = std::process::Command::new("systemctl")
        .args(["is-active", "rotten-apple-orchestratord.service"])
        .output();
    if let Ok(o) = unit_state {
        let s = String::from_utf8_lossy(&o.stdout);
        println!("  orchestratord.service     {}", s.trim());
    }

    println!();
    println!("libxl direct (would the cockpit's fallback path work?)");
    match XenBackend::new() {
        Ok(_) => println!("  XenBackend::new()         ok"),
        Err(e) => println!("  XenBackend::new()         {e}"),
    }

    println!();
    println!("boot mode");
    println!("  current_boot_mode         {}", boot_mode::current_boot_mode().label());

    println!();
    println!("last cockpit-boot log lines (newest 5):");
    let log = "/var/log/rotten-apple-cockpit-boot.log";
    match std::fs::File::open(log) {
        Ok(mut f) => {
            let mut s = String::new();
            let _ = f.read_to_string(&mut s);
            let mut lines: Vec<&str> = s.lines().collect();
            let n = lines.len();
            for line in lines.split_off(n.saturating_sub(5)) {
                println!("  {line}");
            }
        }
        Err(e) => println!("  ({log}: {e})"),
    }

    println!();
    println!("==> diag complete. To launch the cockpit interactively:");
    println!("    sudo rotten-apple cockpit");
    ExitCode::SUCCESS
}

fn cmd_boot_mode(target: BootModeArg, reboot: bool, font: String) -> ExitCode {
    let outcome = match target {
        BootModeArg::Status => {
            let m = boot_mode::current_boot_mode();
            println!("current boot mode: {}", m.label());
            return ExitCode::SUCCESS;
        }
        BootModeArg::Cockpit => boot_mode::enable_cockpit_boot_with_font(false, &font),
        BootModeArg::Desktop => boot_mode::disable_cockpit_boot(false),
    };
    if let Err(e) = outcome {
        eprintln!("boot-mode: {e}");
        return ExitCode::from(2);
    }
    if reboot {
        eprintln!("==> rebooting (--reboot was set)...");
        let s = std::process::Command::new("systemctl").arg("reboot").status();
        if let Err(e) = s {
            eprintln!("systemctl reboot: {e}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

/// `rotten-apple rebuild` — single command for "I changed code, push it".
/// Runs as the user (cargo build), then re-execs as sudo to install.
fn cmd_rebuild(source: Option<PathBuf>) -> ExitCode {
    // Don't run cargo as root: it'd create root-owned target/ files in
    // the user's source tree. Refuse gracefully and tell them how.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        eprintln!("rebuild: don't run me as root — cargo build would create");
        eprintln!("         root-owned target/ files in your source tree.");
        eprintln!("         Run as your normal user; I'll sudo for the install step.");
        return ExitCode::from(2);
    }
    let src = match source.or_else(find_workspace_root) {
        Some(p) => p,
        None => {
            eprintln!("rebuild: could not find workspace Cargo.toml — pass --source <path>");
            eprintln!("         or run from inside the rotten-apple source tree.");
            return ExitCode::from(2);
        }
    };
    eprintln!("==> cargo build --workspace --release  (from {})", src.display());
    let status = std::process::Command::new("cargo")
        .args(["build", "--workspace", "--release"])
        .current_dir(&src)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => { eprintln!("cargo build exited {s}"); return ExitCode::from(2) }
        Err(e) => { eprintln!("cargo: {e}"); return ExitCode::from(2) }
    }
    let fresh = src.join("target/release/rotten-apple");
    if !fresh.exists() {
        eprintln!("rebuild: build succeeded but {} is missing — workspace layout drift?",
                  fresh.display());
        return ExitCode::from(2);
    }
    // Re-exec as sudo to drive the install. One password prompt for the
    // user; the freshly-built binary takes over.
    eprintln!("==> sudo {} update  (single password prompt incoming)", fresh.display());
    let s = std::process::Command::new("sudo")
        .arg(&fresh).arg("update")
        .status();
    match s {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => { eprintln!("install exited {s}"); ExitCode::from(2) }
        Err(e) => { eprintln!("sudo: {e}"); ExitCode::from(2) }
    }
}

/// Walk up from CWD looking for the workspace root (a Cargo.toml that
/// contains `[workspace]`). Returns the directory containing it.
fn find_workspace_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.exists()
            && let Ok(s) = std::fs::read_to_string(&cargo)
            && s.contains("[workspace]")
        {
            return Some(dir);
        }
        if !dir.pop() { return None; }
    }
}

/// `sudo rotten-apple recover` — one-command panic button. Undoes the
/// cockpit-boot override, unmasks gdm, starts gdm. The escape-hatch
/// equivalent of a Ctrl-Alt-F2 + boot-mode desktop + reboot dance.
fn cmd_recover() -> ExitCode {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        eprintln!("recover: needs root. Run: sudo rotten-apple recover");
        return ExitCode::from(2);
    }
    eprintln!("==> recover: undo cockpit-boot, unmask gdm, start the graphical session");
    let mut had_error = false;
    eprintln!("    [1/3] disable cockpit-boot (idempotent)");
    if let Err(e) = boot_mode::disable_cockpit_boot(false) {
        eprintln!("          {e} — continuing anyway");
        had_error = true;
    }
    eprintln!("    [2/3] unmask display managers (best-effort)");
    for dm in ["gdm3.service", "gdm.service", "lightdm.service",
               "sddm.service", "lxdm.service", "display-manager.service"] {
        let _ = std::process::Command::new("systemctl")
            .args(["unmask", dm]).output();
    }
    eprintln!("    [3/3] start a graphical session");
    let started = ["gdm3.service", "gdm.service", "lightdm.service",
                   "sddm.service", "lxdm.service"].iter().find(|dm| {
        std::process::Command::new("systemctl")
            .args(["start", dm]).status()
            .map(|s| s.success()).unwrap_or(false)
    });
    match started {
        Some(dm) => eprintln!("          → {dm} started"),
        None => {
            eprintln!("          (no DM started cleanly — log in via tty1 or reboot)");
            had_error = true;
        }
    }
    eprintln!();
    if had_error {
        eprintln!("==> recover finished with warnings; reboot if the desktop doesn't appear.");
    } else {
        eprintln!("==> recover complete. Graphical session should be appearing now.");
    }
    ExitCode::SUCCESS
}

fn cmd_cockpit(manifest: PathBuf, orchestrator: Option<PathBuf>) -> ExitCode {
    let config = rotten_apple_cockpit::CockpitConfig {
        manifest_path: manifest,
        orchestrator_path: orchestrator,
    };
    match rotten_apple_cockpit::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("cockpit: {e}"); ExitCode::from(2) }
    }
}

// ---------------------------------------------------------------------------

fn cmd_detect() -> ExitCode {
    let d = Detection::run();
    println!("rotten-apple v{} — host detection report\n", env!("CARGO_PKG_VERSION"));
    println!("  distro:           {} {}", d.distro_id, d.distro_version);
    println!("  kernel:           {} ({})", d.kernel, d.arch);
    println!("  firmware:         {}", if d.is_uefi { "UEFI" } else { "BIOS/legacy" });
    println!("  secure boot:      {}", d.secure_boot);
    println!("  cpus:             {}", d.cpu_count);
    println!("  ram:              {} MB", d.mem_total_kb / 1024);
    let iommu = match (d.has_intel_iommu, d.has_amd_iommu) {
        (true,  _)     => "intel",
        (_,     true)  => "amd",
        _              => "?",
    };
    println!("  iommu:            {} ({})", iommu,
             if d.iommu_in_cmdline { "enabled in cmdline" } else { "not in cmdline" });
    println!("  nvidia driver:    {}",
             if d.nvidia_proprietary { "proprietary" } else { "open / not loaded" });
    println!("  xen hypervisor:   {}",
             if d.xen_already_installed { "installed" } else { "absent" });
    println!("  running under xen: {}",
             if d.running_under_xen { "yes" } else { "no" });
    println!("  GRUB_DEFAULT:     {}", d.grub_default_raw.as_deref().unwrap_or("(unset)"));
    println!("  /boot free:       {} MB", d.boot_free_mb);
    println!("  / free:           {} MB", d.root_free_mb);
    match (&d.initramfs_path, d.initramfs_size_mb) {
        (Some(p), s) => println!("  initramfs:        {p} ({s} MB)"),
        (None, _)    => println!("  initramfs:        <none for this kernel>"),
    }
    println!();
    if !d.blockers.is_empty() {
        println!("BLOCKERS:");
        for b in &d.blockers { println!("  ! {b}"); }
        println!();
    }
    if !d.warnings.is_empty() {
        println!("warnings:");
        for w in &d.warnings { println!("  - {w}"); }
        println!();
    }
    if d.blockers.is_empty() {
        println!("ready. (lift not yet implemented in this binary; see ROADMAP.md.)");
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn cmd_node_info(
    key_path: Option<PathBuf>,
    node_id_path: Option<PathBuf>,
    full_pubkey: bool,
) -> ExitCode {
    use rotten_apple_fabric::{KeyPair, NodeId};

    let kp_path = key_path.unwrap_or_else(||
        PathBuf::from("/var/lib/rotten-apple/node.key"));
    let nid_path = node_id_path.unwrap_or_else(||
        PathBuf::from("/etc/rotten-apple/node-id"));
    let mid_path = PathBuf::from("/etc/machine-id");

    // Best-effort role hint from /etc/hostname so first-run derivation
    // gives nodes recognizable display names ("laptop-...", "desk-box-...").
    let role_hint = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let node_id = match NodeId::derive_from_paths(
        &nid_path, &mid_path, role_hint.as_deref(),
    ) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("node-info: derive node-id: {e}");
            eprintln!("(running unprivileged? the default paths need root; try --node-id-path)");
            return ExitCode::from(2);
        }
    };

    let kp = match KeyPair::load_or_generate_at(&kp_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("node-info: load/generate keypair at {}: {e}", kp_path.display());
            eprintln!("(running unprivileged? the default path is 0600 in /var/lib/; try --key-path)");
            return ExitCode::from(2);
        }
    };
    let pk = kp.public();

    println!("rotten-apple v{} — node identity", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  node id:           {}", node_id);
    println!("  hex suffix:        {}", node_id.hex_suffix());
    println!("  pubkey (short):    {}", pk.fingerprint_short());
    if full_pubkey {
        println!("  pubkey (full):     {}", pk.hex());
    }
    println!("  key path:          {}", kp_path.display());
    println!("  node-id path:      {}", nid_path.display());
    println!();
    println!("To register THIS node on another node, add to its /etc/rotten-apple/mesh.toml:");
    println!();
    println!("  [[peers]]");
    println!("  node_id = \"{}\"", node_id);
    println!("  addr    = [\"<host-or-ip>:7042\"]");
    if full_pubkey {
        println!("  pubkey  = \"{}\"  # required for trust.mode=\"config\"", pk.hex());
    } else {
        println!("  # pubkey  = \"<re-run with --full-pubkey to print>\"");
    }
    ExitCode::SUCCESS
}

fn cmd_lift_readiness() -> ExitCode {
    let r = LiftReadiness::run();
    println!("rotten-apple v{} — lift-readiness report\n", env!("CARGO_PKG_VERSION"));

    println!("partitions:");
    println!("  /     source:   {}", r.root_source.as_deref().unwrap_or("(unknown)"));
    println!("  /     fs:       {}", r.root_fs.as_deref().unwrap_or("(unknown)"));
    println!("  /     on LUKS:  {}", yesno(r.root_on_luks));
    if r.boot_separate {
        println!("  /boot source:   {}", r.boot_source.as_deref().unwrap_or("(unknown)"));
        println!("  /boot fs:       {}", r.boot_fs.as_deref().unwrap_or("(unknown)"));
        println!("  /boot on LUKS:  {}", yesno(r.boot_on_luks));
    } else {
        println!("  /boot:          (not a separate partition; lives on /)");
    }
    println!();

    println!("hibernation:");
    println!("  configured:     {}", yesno(r.hibernation_configured));
    if let Some(d) = &r.resume_device {
        println!("  resume device:  {d}");
    }
    println!();

    println!("grub flavor:      {}",
             if r.grub_flavor.is_empty() { "(none detected)".into() }
             else { r.grub_flavor.join(", ") });
    println!();

    println!("iommu groups:     {} total{}", r.iommu_groups.len(),
             match r.gpu_group {
                 Some(g) => format!(", gpu in group {g}"),
                 None    => " (no display controller mapped)".into(),
             });
    if let Some(gpu_grp) = r.gpu_group
        && let Some(g) = r.iommu_groups.iter().find(|g| g.id == gpu_grp)
    {
        println!("  group {} (gpu):", g.id);
        for d in &g.devices {
            let v = d.vendor_device.as_deref().unwrap_or("?");
            println!("    {}  [{}]  {}  ({})", d.addr, v, d.class_label, d.class_hex);
        }
    }
    println!();

    if !r.blockers.is_empty() {
        println!("BLOCKERS:");
        for b in &r.blockers { println!("  ! {b}"); }
        println!();
    }
    if !r.warnings.is_empty() {
        println!("warnings:");
        for w in &r.warnings { println!("  - {w}"); }
        println!();
    }
    if r.blockers.is_empty() {
        println!("ready to lift (modulo warnings).");
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn yesno(b: bool) -> &'static str { if b { "yes" } else { "no" } }

fn cmd_plan_lift() -> ExitCode {
    // The plan probes the host: /proc/self/mountinfo for the user's
    // root mount, /dev/disk/by-uuid for the UUID, uname -r + /boot
    // for the kernel and Xen images. dry_run is irrelevant here —
    // this command never executes — but we set it so the printed
    // summary is honest about what mode it's describing.
    use rotten_apple_bootstrapper::thin_dom0::ThinDom0Plan;
    use rotten_apple_bootstrapper::thin_dom0_manifest::{
        UserDesktopInputs, UserDesktopDisplayMode, render_user_desktop_manifest,
    };
    let lift_plan = match ThinDom0Plan::for_this_host(true) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("plan-lift: {e}");
            return ExitCode::from(2);
        }
    };

    // Host sizing recommendation block — what the legacy planner
    // produced. Useful as context next to the install recipe so the
    // user can sanity-check that the dom0/domU split fits the host.
    let d = Detection::run();
    let t = CpuTopology::probe();
    let p = plan(&d, &t);
    println!("host sizing recommendation:");
    println!("  total RAM:      {} MB", d.mem_total_kb / 1024);
    println!("  logical CPUs:   {}", t.logical_cpus);
    if t.is_hybrid {
        println!("  CPU layout:     hybrid (P {:?}, E {:?})", t.p_cores, t.e_cores);
    } else {
        println!("  CPU layout:     uniform ({} cores)", t.logical_cpus);
    }
    println!("  dom0 suggested: {} vcpus / {} MB",
             p.dom0.vcpus, p.dom0.memory_mb);
    println!("  domU suggested: {} vcpus / {} MB active",
             p.ubuntu_domu.vcpus, p.ubuntu_domu.memory_active_mb);
    println!();

    print!("{lift_plan}");
    println!();
    println!("Preview — /etc/rotten-apple/user-desktop.toml:");
    println!("─────────────────────────────────────────────");
    // Use the framebuffer GPU detected at plan time so the preview
    // matches what install will actually emit. Production default is
    // ParavirtOnly: dom0 keeps the iGPU so cockpit stays visible on the
    // laptop panel and the guest renders to a PV display. (Passthrough —
    // guest takes the iGPU, dom0 goes headless — is a later explicit step.)
    let manifest = render_user_desktop_manifest(&lift_plan, &UserDesktopInputs {
        gpu_bdf: lift_plan.framebuffer_gpu_bdf.clone(),
        display_mode: UserDesktopDisplayMode::ParavirtOnly,
        tpm_mode: rotten_apple_manifest::TpmMode::None,
        autostart_enabled: true,
    });
    for line in manifest.lines() {
        println!("  {line}");
    }
    ExitCode::SUCCESS
}

fn cmd_lift(dry_run: bool, grub_only: bool) -> ExitCode {
    use rotten_apple_bootstrapper::thin_dom0::ThinDom0Plan;
    let mut plan = match ThinDom0Plan::for_this_host(dry_run) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lift: pre-flight failed: {e}");
            return ExitCode::from(2);
        }
    };
    plan.dry_run = dry_run;
    let result = if grub_only {
        plan.execute_grub_only()
    } else {
        plan.execute()
    };
    if let Err(e) = result {
        eprintln!("lift: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn cmd_manifest_validate(path: &PathBuf) -> ExitCode {
    let p = match Profile::load(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("load error: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("loaded: {}  ({:?})", p.name(), p.kind());
    println!("  description:  {}", p.description());
    println!(
        "  memory:       active={} idle={} min={}",
        fmt_bytes(p.resources.memory_active_bytes),
        fmt_bytes(p.resources.memory_idle_bytes),
        fmt_bytes(p.resources.memory_minimum_bytes),
    );
    println!(
        "  vcpus:        active={} idle={} min={}",
        p.resources.vcpus_active, p.resources.vcpus_idle, p.resources.vcpus_minimum,
    );
    let storage_target = p.storage.root.source.as_deref()
        .or(p.storage.root.path.as_deref())
        .unwrap_or("?");
    println!("  storage.root: {} -> {}", p.storage.root.kind, storage_target);
    println!("  gpu:          {}{}", p.gpu.mode,
             p.gpu.device.as_deref().map(|d| format!(" ({d})")).unwrap_or_default());
    println!("  tpm:          {:?}", p.tpm.mode);
    println!("  attestation:  required={}", p.attestation_required());

    // Mesh-aware sections — empty/absent = no constraint, surface that
    // explicitly so users see them and can opt in. Only print when the
    // section says something interesting; an entirely-default manifest
    // shouldn't grow noise.
    let anchor_pinned = p.anchor.node.is_some();
    if anchor_pinned || p.anchor.migratable {
        let pin = p.anchor.node.as_deref().unwrap_or("(none)");
        let mig = if p.anchor.migratable { "yes" } else { "no" };
        println!("  anchor:       node={pin} migratable={mig}");
    }
    let caps = &p.capabilities_required;
    let any_cap = caps.need_iommu
        || caps.need_gpu_class.is_some()
        || caps.need_pcpu_min.is_some()
        || caps.need_memory_mb_min.is_some();
    if any_cap {
        let mut bits = Vec::new();
        if caps.need_iommu { bits.push("iommu".into()); }
        if let Some(g) = &caps.need_gpu_class { bits.push(format!("gpu={g}")); }
        if let Some(c) = caps.need_pcpu_min  { bits.push(format!("pcpu>={c}")); }
        if let Some(m) = caps.need_memory_mb_min { bits.push(format!("memMB>={m}")); }
        println!("  needs:        {}", bits.join(" "));
    }
    let lp = &p.lease_policy;
    if !lp.controllers_allowed.is_empty() || !lp.operations_allowed.is_empty() {
        let ctrl = if lp.controllers_allowed.is_empty()
            { "(any)".into() }
            else { lp.controllers_allowed.join(",") };
        let ops = if lp.operations_allowed.is_empty()
            { "(default-safe)".into() }
            else { lp.operations_allowed.join(",") };
        println!("  lease:        controllers={ctrl} ops={ops}");
    }
    println!();

    let mut had_issues = false;
    for caps in [BackendCapabilities::xen_reference(),
                 BackendCapabilities::hyperv_reference()] {
        let issues = p.validate_against(&caps);
        if issues.is_empty() {
            println!("  {:8} → OK", caps.backend_name);
        } else {
            had_issues = true;
            println!("  {:8} → {} issue(s)", caps.backend_name, issues.len());
            for line in issues { println!("      - {line}"); }
        }
    }

    if had_issues { ExitCode::from(2) } else { ExitCode::SUCCESS }
}

fn cmd_xen_list() -> ExitCode {
    let backend = match XenBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("XenBackend::new failed: {e}");
            eprintln!("(this is expected on non-Xen hosts; lift the machine first)");
            return ExitCode::from(2);
        }
    };
    println!("backend: {} ({})", backend.name(),
             rotten_apple_backend_xen::compat::LIBXL_BUILD_VERSION);
    let summaries = backend.list();
    if summaries.is_empty() {
        println!("(no domains)");
    } else {
        for s in summaries {
            println!("  {}  {:?}  {}", s.handle, s.state, s.name);
        }
    }
    ExitCode::SUCCESS
}

fn cmd_xen_try_create(path: &PathBuf) -> ExitCode {
    let profile = match Profile::load(path) {
        Ok(p) => p,
        Err(e) => { eprintln!("load error: {e}"); return ExitCode::FAILURE; }
    };
    println!("loaded manifest: {}", profile.name());

    let backend = match XenBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("XenBackend::new failed: {e}");
            eprintln!("(expected on non-Xen hosts — proves error mapping works)");
            return ExitCode::from(2);
        }
    };
    println!("opened libxl ctx (backend = {})", backend.name());

    match backend.create_guest(&profile) {
        Ok(h) => {
            println!("created: domid={h}");
            println!("(guest is in PAUSED state; call `xen start <handle>` to run it)");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("create_guest failed: {e}");
            ExitCode::from(2)
        }
    }
}

fn cmd_install(dry_run: bool) -> ExitCode {
    let me = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => { eprintln!("install: locate self: {e}"); return ExitCode::FAILURE; }
    };
    let source = preferred_install_source(&me);
    eprintln!("==> rotten-apple install: {}",
              if dry_run { "DRY RUN" } else { "EXECUTE" });
    eprintln!("    source: {}", source.display());
    if source != me {
        eprintln!("    note: current binary is installed at {}, but a repo build was found and preferred", me.display());
    }
    eprintln!();
    match rotten_apple_bootstrapper::install_system(&source, dry_run) {
        Ok(()) => {
            eprintln!();
            eprintln!("==> installed.");
            eprintln!("    Run: sudo rotten-apple cockpit");
            eprintln!("    Or open 'rotten-apple cockpit' from your app menu.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("install: {e}");
            eprintln!();
            eprintln!("(must run as root — the binary lands in /usr/local/bin)");
            ExitCode::from(2)
        }
    }
}

/// When the user runs `sudo rotten-apple update` from inside a source
/// checkout, `current_exe()` points at the already-installed
/// `/usr/local/bin/rotten-apple`. That would just re-install the stale
/// installed binaries and never pick up the freshly built repo artifacts.
///
/// Prefer `target/release/rotten-apple` from the current workspace when:
///   1. we're running from the installed path, and
///   2. the workspace root is discoverable from CWD, and
///   3. the release binary exists there.
///
/// This keeps the common "sudo rotten-apple update" workflow usable from
/// inside the repo after a local `cargo build --release`.
fn preferred_install_source(current_exe: &Path) -> PathBuf {
    if current_exe == Path::new("/usr/local/bin/rotten-apple")
        && let Some(root) = find_workspace_root()
    {
        let candidate = root.join("target/release/rotten-apple");
        if candidate.exists() {
            return candidate;
        }
    }
    current_exe.to_path_buf()
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 { format!("{:.1}G", b as f64 / (1u64 << 30) as f64) }
    else if b >= 1 << 20 { format!("{}M", b / (1u64 << 20)) }
    else { format!("{b}B") }
}

// --- image catalog ---------------------------------------------------------

fn image_catalog_path() -> PathBuf {
    PathBuf::from(rotten_apple_images::DEFAULT_CATALOG_PATH)
}

fn image_cache_dir() -> PathBuf {
    // Same directory the catalog lives in — colocate index + backing
    // files so a single `chown -R` covers everything.
    image_catalog_path().parent().map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/rotten-apple/images"))
}

fn cmd_image_list() -> ExitCode {
    let path = image_catalog_path();
    let cat = rotten_apple_images::Catalog::load_or_empty(&path);
    println!("catalog: {}", path.display());
    if cat.images.is_empty() {
        println!("  (no images registered)");
        return ExitCode::SUCCESS;
    }
    for e in &cat.images {
        println!("  {:<20} {:<6} {:<8} {} ({})",
                 e.name, e.format, e.arch, fmt_bytes(e.size_bytes), e.pulled_at);
        println!("    backing: {}", e.backing.display());
    }
    ExitCode::SUCCESS
}

fn cmd_image_pull(name: &str, dry_run: bool) -> ExitCode {
    let dest = image_cache_dir();
    let cat_path = image_catalog_path();
    eprintln!("==> pull {name}  (cache: {})", dest.display());
    let entry = match rotten_apple_images::pull(name, &dest, dry_run) {
        Ok(e) => e,
        Err(e) => { eprintln!("pull: {e}"); return ExitCode::from(2); }
    };
    if dry_run {
        eprintln!("(dry-run; catalog not updated)");
        return ExitCode::SUCCESS;
    }
    let mut cat = rotten_apple_images::Catalog::load_or_empty(&cat_path);
    cat.upsert(entry.clone());
    if let Err(e) = cat.save(&cat_path) {
        eprintln!("pull: write catalog {}: {e}", cat_path.display());
        return ExitCode::from(2);
    }
    println!("registered: {} → {}", entry.name, entry.backing.display());
    ExitCode::SUCCESS
}

fn cmd_image_rm(name: &str) -> ExitCode {
    let cat_path = image_catalog_path();
    let mut cat = rotten_apple_images::Catalog::load_or_empty(&cat_path);
    let backing = cat.find(name).map(|e| e.backing.clone());
    if !cat.remove(name) {
        eprintln!("rm: no such image: {name}");
        return ExitCode::from(2);
    }
    if let Some(p) = backing
        && p.exists()
        && let Err(e) = std::fs::remove_file(&p)
    {
        eprintln!("rm: unlink {} failed: {e} (entry already removed from catalog)", p.display());
    }
    if let Err(e) = cat.save(&cat_path) {
        eprintln!("rm: write catalog {}: {e}", cat_path.display());
        return ExitCode::from(2);
    }
    println!("removed: {name}");
    ExitCode::SUCCESS
}

fn cmd_image_list_known() -> ExitCode {
    println!("known sources (compile-time list — pin sha256s for production):");
    for s in rotten_apple_images::known_sources() {
        let pin = if s.sha256.is_empty() { "unpinned" } else { "pinned" };
        println!("  {:<16} {:<8} {:<6} [{pin}]", s.name, s.distro, s.arch);
        println!("    {}", s.url);
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// instance subcommand

fn cmd_instance_new(
    id: String,
    base: String,
    memory: Option<u64>,
    vcpus: Option<u32>,
    ephemeral: bool,
    dry_run: bool,
    no_start: bool,
) -> ExitCode {
    use rotten_apple_instances::{
        DEFAULT_REGISTRY_PATH, NewInstanceParams,
        create_instance, dispatch_domain_create,
    };

    let p = NewInstanceParams {
        id: id.clone(),
        base_image: base,
        memory_mb: memory.unwrap_or(rotten_apple_instances::DEFAULT_MEMORY_MB),
        vcpus:     vcpus .unwrap_or(rotten_apple_instances::DEFAULT_VCPUS),
        ephemeral,
    };

    let entry = match create_instance(p, dry_run) {
        Ok(e) => e,
        Err(e) => { eprintln!("instance new: {e}"); return ExitCode::from(2) }
    };

    if dry_run {
        println!("(dry-run) would create:");
        println!("  id        {}", entry.id);
        println!("  base      {}", entry.base_image);
        println!("  overlay   {}", entry.overlay);
        println!("  memory    {} MB", entry.memory_mb);
        println!("  vcpus     {}", entry.vcpus);
        println!("  ephemeral {}", entry.ephemeral);
        println!("  registry  {DEFAULT_REGISTRY_PATH}");
        return ExitCode::SUCCESS;
    }

    println!("created instance {} (overlay {})", entry.id, entry.overlay);

    if no_start {
        println!("(--no-start: skipping domain.create)");
        return ExitCode::SUCCESS;
    }

    let manifest_path = std::path::Path::new(
        rotten_apple_instances::DEFAULT_MANIFESTS_DIR
    ).join(format!("{id}.toml"));
    let socket = std::path::Path::new(
        rotten_apple_orchestratord::DEFAULT_SOCKET_PATH);
    match dispatch_domain_create(socket, &manifest_path) {
        Ok(domid) => {
            println!("started: domid {domid}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("instance new: started overlay but daemon failed: {e}");
            eprintln!("  (instance is registered; run `rotten-apple instance start {id}`)");
            ExitCode::from(2)
        }
    }
}

fn cmd_instance_fork(parent: &str, child: &str, dry_run: bool) -> ExitCode {
    match rotten_apple_instances::fork_instance(parent, child, dry_run) {
        Ok(e) => {
            if dry_run {
                println!("(dry-run) would fork {} → {}", parent, e.id);
                println!("  backing  {}", e.base_image);
                println!("  overlay  {}", e.overlay);
            } else {
                println!("forked {} → {} (overlay {})", parent, e.id, e.overlay);
            }
            ExitCode::SUCCESS
        }
        Err(e) => { eprintln!("instance fork: {e}"); ExitCode::from(2) }
    }
}

fn cmd_instance_rm(id: &str, dry_run: bool) -> ExitCode {
    match rotten_apple_instances::destroy_instance(id, dry_run) {
        Ok(()) => {
            if dry_run {
                println!("(dry-run) would destroy {id}");
            } else {
                println!("destroyed {id}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => { eprintln!("instance rm: {e}"); ExitCode::from(2) }
    }
}

fn cmd_instance_list() -> ExitCode {
    use rotten_apple_instances::{DEFAULT_REGISTRY_PATH, InstanceRegistry};
    let reg = InstanceRegistry::load_or_empty(
        std::path::Path::new(DEFAULT_REGISTRY_PATH));
    if reg.instances.is_empty() {
        println!("(no instances registered)");
        return ExitCode::SUCCESS;
    }
    println!("{:<20} {:<20} {:<8} {:<6} {:<10} parent", "id", "base", "mem(M)", "vcpus", "eph");
    for e in &reg.instances {
        println!("{:<20} {:<20} {:<8} {:<6} {:<10} {}",
            e.id, e.base_image, e.memory_mb, e.vcpus,
            if e.ephemeral { "yes" } else { "no" },
            e.parent.as_deref().unwrap_or("-"));
    }
    ExitCode::SUCCESS
}

fn cmd_instance_start(id: &str) -> ExitCode {
    let manifest_path = std::path::Path::new(
        rotten_apple_instances::DEFAULT_MANIFESTS_DIR
    ).join(format!("{id}.toml"));
    if !manifest_path.exists() {
        eprintln!("instance start: manifest missing for {id} ({})",
            manifest_path.display());
        eprintln!("  (was the instance ever created? run `instance list` to check)");
        return ExitCode::from(2);
    }
    let socket = std::path::Path::new(
        rotten_apple_orchestratord::DEFAULT_SOCKET_PATH);
    // libxl_domain_create_new lands the domain in *paused* state. The
    // trait splits create from start so callers can fork or snapshot
    // before the first instruction runs. The user-facing `instance start`
    // wants the domain actually running, so issue both calls in sequence.
    let domid = match rotten_apple_instances::dispatch_domain_create(socket, &manifest_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("instance start: domain.create: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = rotten_apple_instances::dispatch_domain_start(socket, domid) {
        eprintln!("instance start: domain.start (domid {domid}): {e}");
        eprintln!("  (domain was created but is paused — `xl unpause {domid}` to recover, or `xl destroy {domid}` to clean up)");
        return ExitCode::from(2);
    }
    println!("started {id}: domid {domid}");
    ExitCode::SUCCESS
}
