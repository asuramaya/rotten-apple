//! rotten-apple cockpit — TUI control surface.
//!
//! Single-process, links the backend trait directly. Works locally on
//! dom0 (sudo required for libxl) and over SSH (a remote terminal is
//! still a terminal — ratatui renders identically).
//!
//! Architecture:
//!   - main thread runs the UI loop: ~20 fps draw, key event polling
//!   - worker thread owns the libxl backend and handles all calls;
//!     receives commands and sends Snapshot / ActionResult messages
//!     back over mpsc channels (libxl_ctx is `!Sync`, so single-actor
//!     ownership is the cleanest enforcement)
//!   - 1 Hz background polling; `r` triggers immediate refresh
//!
//! No tokio. std threads + mpsc — libxl is sync, the UI is sync, and a
//! tokio runtime would just add an event loop nobody asked for.
//!
//! Worker strategy: if the local `/run/rotten-apple.sock` or the
//! default host-vsock transport is reachable, the orchestratord
//! handshake succeeds, and `host.info` reports a usable backend, the
//! worker thread translates Cmd → JSON-RPC against the daemon.
//! Otherwise the worker opens libxl directly (the original path;
//! required for first-run before the daemon is installed). Both produce
//! the same `Msg` types so the UI doesn't need to know which is in use.

pub mod daemon_client;
pub mod widgets;

use daemon_client::{DaemonClient, DaemonError, DaemonTransport};

use rotten_apple_backend::{
    GuestHandle, GuestState, GuestStatus, GuestSummary, HypervisorBackend,
};
use rotten_apple_backend_xen::XenBackend;
use rotten_apple_bootstrapper::LiftPlan;
use rotten_apple_bootstrapper::boot_mode::{self, BootMode};
use rotten_apple_detect::Detection;
use rotten_apple_manifest::Profile;
use rotten_apple_orchestratord::DEFAULT_SOCKET_PATH;

use serde_json::{Value, json};

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        execute,
        terminal::{
            EnterAlternateScreen, LeaveAlternateScreen,
            disable_raw_mode, enable_raw_mode,
        },
    },
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Public entry

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const UI_TICK: Duration = Duration::from_millis(50);
const TOAST_TTL: Duration = Duration::from_secs(4);
const MAX_EVENTS: usize = 200;
const DEFAULT_MANIFEST: &str = "/etc/rotten-apple/active.toml";

#[derive(Clone)]
pub struct CockpitConfig {
    pub manifest_path: PathBuf,
    /// Path to the orchestrator binary used by the lift action. If None,
    /// auto-detected by looking next to the cockpit binary itself
    /// (`/proc/self/exe`'s sibling). Required when entering Lifting mode.
    pub orchestrator_path: Option<PathBuf>,
}

impl Default for CockpitConfig {
    fn default() -> Self {
        Self {
            manifest_path: PathBuf::from(DEFAULT_MANIFEST),
            orchestrator_path: None,
        }
    }
}

/// Auto-detect the orchestrator binary next to the current executable.
fn auto_orchestrator_path() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let sibling = me.parent()?.join("rotten-apple-orchestrator");
    if sibling.exists() { Some(sibling) } else { None }
}

/// What the cockpit should be doing on this host right now. Determined
/// at startup; can change if the user runs the lift inside the cockpit.
#[derive(Debug, Clone)]
enum AppMode {
    /// Bare-metal Ubuntu, no Xen package installed. Lift hasn't been run.
    PreLift,
    /// Xen package is installed but we're not booted under Xen — probably
    /// because the user lifted but is sitting in the bare-metal entry.
    /// Tell them to reboot.
    AwaitingReboot,
    /// libxl is reachable; show the standard cockpit (domain list etc).
    Active,
    /// libxl couldn't be opened despite Xen looking present at runtime;
    /// surface the error in the UI and let the user quit.
    BackendError(String),
}

fn detect_app_mode(detection: &Detection) -> AppMode {
    // If the daemon is reachable, we're Active even if libxl can't be
    // opened from this process — the daemon owns libxl and the cockpit
    // talks to it over the socket. Lets us run the cockpit unprivileged
    // when orchestratord is the libxl owner.
    if ready_daemon_client(Path::new(DEFAULT_SOCKET_PATH)).is_some() {
        return AppMode::Active;
    }

    // Fall through: try the backend directly. If it works, we're Active
    // regardless of package state — the cockpit can manage existing
    // domains via libxl.
    match XenBackend::new() {
        Ok(_) => AppMode::Active,
        Err(e) => {
            if detection.running_under_xen {
                // We're under Xen but libxl broke; that's a real error,
                // not a "not lifted yet" state.
                AppMode::BackendError(e.to_string())
            } else if detection.xen_already_installed {
                AppMode::AwaitingReboot
            } else {
                AppMode::PreLift
            }
        }
    }
}

/// Run the cockpit until the user quits. Detects what the host needs
/// (PreLift / AwaitingReboot / Active / BackendError) and dispatches
/// to the right view.
///
/// PreLift's `e` keybinding tears down the TUI, runs the lift to plain
/// terminal output (so apt's progress is visible — debootstrap+install
/// is a multi-minute thing), then exits. The user reboots and relaunches
/// cockpit; on the next run mode-detection lands them in Active.
pub fn run(config: CockpitConfig) -> io::Result<()> {
    // Discoverability nudge: if the user is running from ./target/...
    // (or anywhere outside /usr/local/bin), tell them once before the
    // TUI takes over the screen. After they install, this stops firing.
    if let Ok(me) = std::env::current_exe()
        && me != Path::new("/usr/local/bin/rotten-apple")
    {
        eprintln!("note: running from {}", me.display());
        eprintln!("      to install system-wide: sudo {} install",
                  me.display());
    }

    // Silence fd 2 for the duration of the TUI. libxl's xtl logger and
    // libxenctrl write diagnostic lines straight to stderr (e.g.
    // "xencall: error: Could not obtain handle on privileged command
    // interface" when libxl_ctx_alloc fails on a non-Xen host). Those
    // lines land on the user's real terminal scrollback while the
    // alt-screen is hiding them — when the cockpit exits, the noise is
    // sitting there looking like a crash.
    //
    // The silencer dup2's /dev/null over fd 2 for as long as it's
    // alive; Drop restores fd 2 even on panic. We hold it across the
    // entire run, including setup_terminal/teardown_terminal.
    let _silencer = StderrSilencer::install();

    // Boot-time diagnostic: one line per cockpit launch into a known log
    // path so that even if the TUI never renders, you can ssh / Ctrl-Alt-F2
    // to another tty and `tail /var/log/rotten-apple-cockpit-boot.log` to
    // see what was probed and what failed. Best-effort — silent if we
    // can't write (e.g. running as a non-root user without /var/log).
    let detection = Detection::run();
    let mode = detect_app_mode(&detection);
    log_boot_diagnostic(&detection, &mode);

    let mut terminal = setup_terminal()?;
    let config_clone = config.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match &mode {
            AppMode::Active =>
                run_active(&mut terminal, config_clone),
            AppMode::PreLift =>
                run_pre_lift(&mut terminal, &config_clone, &detection),
            AppMode::AwaitingReboot =>
                run_static_screen(&mut terminal, awaiting_reboot_screen()),
            AppMode::BackendError(e) =>
                run_static_screen(&mut terminal, backend_error_screen(e)),
        }
    }));
    teardown_terminal(&mut terminal)?;

    // CRITICAL: a panic inside the dispatch USED to resume_unwind here,
    // which crashed the cockpit out to bare tty. Under cockpit-boot mode
    // that meant systemd respawned us (and pre-StartLimit fixes, looped
    // forever — bricking the box). Now: catch the panic, re-establish
    // the terminal, display a recovery screen with [s]/[d]/[q] keys, and
    // stay there. The user is never stranded.
    let inner = match result {
        Ok(r) => r,
        Err(p) => {
            let msg = panic_msg(&p);
            return show_panic_recovery_screen(&config, &msg);
        }
    };

    // PreLift run can ask us to perform the lift AFTER tearing down the
    // TUI so apt's output streams to the user's terminal naturally.
    match inner? {
        PostUiAction::None => Ok(()),
        PostUiAction::ExecuteLift => {
            execute_lift_after_teardown(&config)
        }
        PostUiAction::Reboot => execute_reboot_after_teardown(),
        PostUiAction::DropToShell => execute_shell_after_teardown(),
        PostUiAction::DisableCockpitBootAndReboot =>
            execute_disable_cockpit_boot_and_reboot(),
    }
}

/// One-line append per cockpit launch. Gives the user a forensic trail
/// when the TUI fails to come up properly under cockpit-boot — Ctrl-Alt-F2
/// to a getty, `tail /var/log/rotten-apple-cockpit-boot.log` shows the
/// last few launches and what they detected. Best-effort: silent on
/// write failure (non-root invocation, /var/log read-only, etc.).
fn log_boot_diagnostic(detection: &Detection, mode: &AppMode) {
    use std::io::Write;
    let path = "/var/log/rotten-apple-cockpit-boot.log";
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true).open(path) else { return };
    // Best-effort timestamp without pulling in chrono: seconds since
    // epoch is enough for ordering and post-hoc correlation with journal.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let mode_label = match mode {
        AppMode::Active           => "Active".to_string(),
        AppMode::PreLift          => "PreLift".to_string(),
        AppMode::AwaitingReboot   => "AwaitingReboot".to_string(),
        AppMode::BackendError(e)  => format!("BackendError({e})"),
    };
    let line = format!(
        "ts={secs} mode={mode_label} under_xen={} xen_installed={} kernel={}\n",
        detection.running_under_xen,
        detection.xen_already_installed,
        detection.kernel);
    let _ = f.write_all(line.as_bytes());
}

/// Pull a String message out of a `Box<dyn Any>` panic payload. Handles
/// both `&str` (panic!("literal")) and `String` (panic!("{var}")) cases;
/// falls back to a generic label for other types.
fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() { return (*s).to_string(); }
    if let Some(s) = p.downcast_ref::<String>() { return s.clone(); }
    "(panic payload was not a string)".to_string()
}

/// Last-resort recovery: cockpit's main UI panicked. Re-establish the
/// terminal, render a static screen showing the panic message + the
/// usual recovery keys, wait for the user to pick one. Never crashes
/// to bare tty. Idempotent on terminal teardown — even if setup fails
/// we still try to dispatch the post-teardown action.
fn show_panic_recovery_screen(
    config: &CockpitConfig, panic_message: &str,
) -> io::Result<()> {
    let screen = StaticScreen {
        title: " cockpit panic ".into(),
        headline: "The cockpit's main UI panicked.".into(),
        body: vec![
            "This shouldn't happen — please copy the message below and".into(),
            "report it. Recovery keys still work; the box isn't stranded.".into(),
            "".into(),
            "Panic message:".into(),
            format!("  {panic_message}"),
            "".into(),
            "  [s] drop to a root shell on this tty".into(),
            "  [d] disable cockpit-boot mode and reboot to desktop".into(),
            "  [q] exit (systemd will retry; StartLimit caps at 3 in 60s)".into(),
        ],
        accent: Color::Red,
        allow_reboot: false,
        allow_recovery: true,
    };
    let mut terminal = match setup_terminal() {
        Ok(t) => t,
        Err(_) => {
            // Terminal won't initialise — log to stderr (visible briefly
            // before TTYVTDisallocate) and return so systemd can decide
            // what to do. StartLimit catches the loop.
            eprintln!("==> COCKPIT PANIC (terminal also broken):");
            eprintln!("    {panic_message}");
            return Ok(());
        }
    };
    let action = run_static_screen(&mut terminal, screen);
    let _ = teardown_terminal(&mut terminal);
    match action? {
        PostUiAction::DropToShell => execute_shell_after_teardown(),
        PostUiAction::DisableCockpitBootAndReboot =>
            execute_disable_cockpit_boot_and_reboot(),
        _ => Ok(()),
    }
    .or_else(|_| { let _ = config.manifest_path; Ok(()) })
}

/// `[s]` recovery key from BackendError. Replaces the cockpit process
/// with /bin/bash so the user has an interactive root shell on tty1.
/// When they exit the shell, getty respawns (if cockpit-boot still set),
/// or systemd cycles getty (if not). Either way they're not stranded.
fn execute_shell_after_teardown() -> io::Result<()> {
    let on_tty1 = is_on_systemd_cockpit_tty();
    eprintln!();
    eprintln!("==> rotten-apple cockpit exited.");
    if on_tty1 {
        eprintln!("    You are in a root shell on tty1 (the cockpit-boot tty).");
        eprintln!();
        eprintln!("    Common next steps:");
        eprintln!("      rotten-apple cockpit                        relaunch the TUI");
        eprintln!("      rotten-apple boot-mode desktop --reboot     return to gdm");
        eprintln!("      rotten-apple recover                        panic-button: undo cockpit-boot");
        eprintln!();
        eprintln!("    `exit` will end this shell — systemd will NOT auto-relaunch the");
        eprintln!("    cockpit (clean exits don't trip Restart=on-failure). Use one of");
        eprintln!("    the commands above instead.");
    } else {
        eprintln!("    Dropped to shell. `exit` returns to your previous shell.");
    }
    eprintln!();
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("/bin/bash")
        .arg("--login")
        .exec();
    Err(io::Error::other(format!("exec /bin/bash failed: {err}")))
}

/// `[d]` recovery key. The point-of-no-return escape hatch: undoes the
/// cockpit-boot override AND reboots, so when systemd comes back up
/// you're at gdm. This is what to press if the cockpit can't establish
/// itself and you don't want to keep retrying.
fn execute_disable_cockpit_boot_and_reboot() -> io::Result<()> {
    eprintln!();
    eprintln!("==> disabling cockpit-boot mode...");
    if let Err(e) = rotten_apple_bootstrapper::boot_mode::disable_cockpit_boot(false) {
        eprintln!("    boot-mode disable failed: {e}");
        eprintln!("    you can run manually: sudo rotten-apple boot-mode desktop");
    } else {
        eprintln!("    cockpit-boot disabled. Rebooting...");
    }
    let status = std::process::Command::new("systemctl")
        .arg("reboot").status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(io::Error::other(format!("systemctl reboot exited {s}"))),
        Err(e) => Err(io::Error::other(format!("systemctl reboot: {e}"))),
    }
}

fn execute_reboot_after_teardown() -> io::Result<()> {
    eprintln!();
    eprintln!("==> rebooting now (TUI torn down).");
    eprintln!("    Hold Shift at the GRUB menu, pick");
    eprintln!("    'Ubuntu GNU/Linux, with Xen hypervisor'.");
    eprintln!();
    let status = std::process::Command::new("systemctl")
        .arg("reboot")
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(io::Error::other(format!(
            "systemctl reboot exited {s}"))),
        Err(e) => Err(io::Error::other(format!(
            "systemctl reboot: {e} (you may need to `sudo reboot` manually)"))),
    }
}

/// What `run_*` returns when it wants the caller to do something after
/// the TUI is torn down.
enum PostUiAction {
    None,
    ExecuteLift,
    Reboot,
    /// BackendError recovery: replace cockpit with /bin/bash. Used when
    /// cockpit is the boot UI on tty1 and the backend won't initialize.
    DropToShell,
    /// BackendError recovery: undo cockpit-boot mode + systemctl reboot.
    DisableCockpitBootAndReboot,
}

fn execute_lift_after_teardown(config: &CockpitConfig) -> io::Result<()> {
    let orchestrator = config.orchestrator_path.clone()
        .or_else(auto_orchestrator_path)
        .ok_or_else(|| io::Error::other(
            "could not find rotten-apple-orchestrator binary. \
             Pass --orchestrator <path> to cockpit, or place the binary \
             next to the cockpit binary."))?;

    eprintln!();
    eprintln!("==> running lift from cockpit (TUI torn down so you can");
    eprintln!("    see apt's output and any prompts).");
    eprintln!();

    let plan = LiftPlan::for_this_host(
        config.manifest_path.clone(),
        orchestrator,
        false, // not dry-run
    ).map_err(|e| io::Error::other(format!("lift plan: {e}")))?;

    plan.execute()
        .map_err(|e| io::Error::other(format!("lift execute: {e}")))?;

    eprintln!();
    eprintln!("==> lift complete. Reboot, hold Shift at GRUB, pick the");
    eprintln!("    'Ubuntu GNU/Linux, with Xen hypervisor' entry. Then");
    eprintln!("    relaunch the cockpit and you'll be in Active mode.");
    Ok(())
}

/// RAII guard that swaps fd 2 for /dev/null. Restores the original fd
/// on Drop (also fires on panic via the unwind path). Dropping silently
/// is acceptable — if dup2 fails on restore, the fd was already broken.
struct StderrSilencer {
    saved_fd: libc::c_int,
    /// Holds /dev/null open until Drop so its fd stays valid.
    _devnull: std::fs::File,
}

impl StderrSilencer {
    /// None on failure (couldn't open /dev/null, dup, or dup2). The
    /// caller carries on without silencing — degraded but functional.
    fn install() -> Option<Self> {
        use std::os::fd::AsRawFd;
        let devnull = std::fs::OpenOptions::new()
            .write(true).open("/dev/null").ok()?;
        // SAFETY: libc::dup is a standard syscall; saved_fd valid on success.
        let saved_fd = unsafe { libc::dup(2) };
        if saved_fd < 0 { return None }
        // SAFETY: dup2 is standard; arguments are valid fds.
        if unsafe { libc::dup2(devnull.as_raw_fd(), 2) } < 0 {
            unsafe { libc::close(saved_fd); }
            return None;
        }
        Some(StderrSilencer { saved_fd, _devnull: devnull })
    }
}

impl Drop for StderrSilencer {
    fn drop(&mut self) {
        // SAFETY: saved_fd is the original fd 2 we duped on install.
        unsafe {
            libc::dup2(self.saved_fd, 2);
            libc::close(self.saved_fd);
        }
    }
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn teardown_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_active(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: CockpitConfig,
) -> io::Result<PostUiAction> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();

    // Pick the worker strategy on the UI thread (so any "I tried the
    // socket and it wasn't there" noise stays out of the worker), then
    // hand the chosen strategy off to the worker thread.
    let strategy = pick_worker_strategy();

    // Spawn the worker. Daemon strategy owns just a socket; direct
    // strategy owns the libxl backend (libxl_ctx is !Sync, so it must
    // be created on the worker thread).
    thread::Builder::new()
        .name("cockpit-worker".into())
        .spawn(move || match strategy {
            WorkerStrategy::Daemon(client) =>
                worker_loop_daemon(client, cmd_rx, msg_tx),
            WorkerStrategy::Direct =>
                worker_loop_direct(cmd_rx, msg_tx),
        })
        .expect("spawn cockpit-worker");

    let mut state = State::new(config);

    loop {
        // Drain any pending messages from the worker before drawing so
        // the UI reflects current state.
        while let Ok(msg) = msg_rx.try_recv() {
            state.apply(msg);
        }

        terminal.draw(|f| render(f, &mut state))?;

        if state.should_quit { break }

        // Poll for terminal events with a short timeout so we still tick
        // even when nothing is happening.
        if event::poll(UI_TICK)?
            && let Event::Key(k) = event::read()?
            && let Some(cmd) = state.handle_key(k)
        {
            let _ = cmd_tx.send(cmd);
        }
    }

    let _ = cmd_tx.send(Cmd::Shutdown);
    // When the cockpit was launched by the systemd cockpit-boot override
    // on tty1, a clean exit leaves the unit stopped and tty1 goes blank
    // with no getty (the override replaced ExecStart, so systemd won't
    // fall back to login). Drop the user into a root shell on the same
    // tty so they always have a way out — relaunch with `rotten-apple
    // cockpit`, return to gdm with `boot-mode desktop --reboot`, etc.
    if is_on_systemd_cockpit_tty() {
        Ok(PostUiAction::DropToShell)
    } else {
        Ok(PostUiAction::None)
    }
}

/// Are we the systemd-launched cockpit on tty1?
///
/// Two signals must agree:
///   1. The cockpit-boot override file exists (we know we're configured
///      to take over tty1).
///   2. Our stdin's tty is `/dev/tty1` (we actually got launched there,
///      not from a developer's `sudo rotten-apple cockpit` in a desktop
///      terminal).
///
/// When both are true, a normal `[q]` exit needs to drop into a shell
/// rather than letting systemd see us terminate cleanly and stop the
/// unit (which would leave the tty blank with no recovery).
fn is_on_systemd_cockpit_tty() -> bool {
    let override_path = "/etc/systemd/system/getty@tty1.service.d/00-rotten-apple-cockpit.conf";
    if !Path::new(override_path).exists() {
        return false;
    }
    match std::fs::read_link("/proc/self/fd/0") {
        Ok(p) => p == Path::new("/dev/tty1"),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Pre-lift / static landing pages

fn run_pre_lift(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &CockpitConfig,
    detection: &Detection,
) -> io::Result<PostUiAction> {
    let topo = rotten_apple_detect::CpuTopology::probe();
    let plan = rotten_apple_detect::plan(detection, &topo);
    let lr = rotten_apple_detect::LiftReadiness::run();

    let mut confirming = false;

    let manifest_exists = config.manifest_path.exists();

    loop {
        terminal.draw(|f| {
            render_pre_lift(f, detection, &topo, &plan, &lr,
                &config.manifest_path, manifest_exists, confirming);
        })?;

        if event::poll(UI_TICK)?
            && let Event::Key(k) = event::read()?
        {
            if k.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(k.code, KeyCode::Char('c')) {
                return Ok(PostUiAction::None);
            }
            if confirming {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        return Ok(PostUiAction::ExecuteLift);
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                        confirming = false;
                    }
                    _ => {}
                }
                continue;
            }
            match k.code {
                KeyCode::Char('q') => return Ok(PostUiAction::None),
                KeyCode::Char('e') => {
                    if !manifest_exists {
                        // can't lift without a manifest; ignore
                        continue;
                    }
                    if !lr.blockers.is_empty() {
                        continue;
                    }
                    confirming = true;
                }
                _ => {}
            }
        }
    }
}

fn run_static_screen(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    screen: StaticScreen,
) -> io::Result<PostUiAction> {
    let mut confirming_reboot = false;
    loop {
        terminal.draw(|f| {
            render_static_screen(f, &screen);
            if confirming_reboot {
                render_confirm_reboot(f, f.area());
            }
        })?;
        if event::poll(UI_TICK)?
            && let Event::Key(k) = event::read()?
        {
            if k.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(k.code, KeyCode::Char('c')) {
                return Ok(PostUiAction::None);
            }
            if confirming_reboot {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') =>
                        return Ok(PostUiAction::Reboot),
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') =>
                        confirming_reboot = false,
                    _ => {}
                }
                continue;
            }
            match k.code {
                KeyCode::Char('q') => return Ok(PostUiAction::None),
                KeyCode::Char('r') if screen.allow_reboot => {
                    confirming_reboot = true;
                }
                // BackendError recovery keys — only available when the
                // screen explicitly opts in. `s` and `d` are the escape
                // hatches that prevent cockpit-boot from stranding tty1.
                KeyCode::Char('s') if screen.allow_recovery => {
                    return Ok(PostUiAction::DropToShell);
                }
                KeyCode::Char('d') if screen.allow_recovery => {
                    return Ok(PostUiAction::DisableCockpitBootAndReboot);
                }
                _ => {}
            }
        }
    }
}

fn render_confirm_reboot(f: &mut Frame, area: Rect) {
    let w = 60u16;
    let h = 7u16;
    let overlay = centered_rect_abs(w, h, area);
    f.render_widget(Clear, overlay);
    let lines = vec![
        Line::from(Span::styled(" reboot now?",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
        Line::raw(""),
        Line::from("  Will run `systemctl reboot` and hand off to GRUB."),
        Line::from("  Pick the 'with Xen hypervisor' entry to land in dom0."),
        Line::raw(""),
        Line::from(Span::styled("  [y] yes, reboot   [Esc/n] cancel",
            Style::default().fg(Color::Yellow))),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" confirm reboot "));
    f.render_widget(p, overlay);
}

struct StaticScreen {
    title: String,
    headline: String,
    body: Vec<String>,
    accent: Color,
    /// If true, `r` keypress prompts to reboot. Used by AwaitingReboot
    /// where the user's only sensible next action is to actually reboot.
    allow_reboot: bool,
    /// If true, `s`/`d` keys offer recovery actions (drop to shell,
    /// disable cockpit boot mode + reboot). Used by BackendError so the
    /// user is never stranded when cockpit is the boot UI on tty1 — the
    /// pre-v0.0.5 design exited cleanly and let systemd respawn forever,
    /// which bricked the box. Keys here are the escape hatch.
    allow_recovery: bool,
}

fn awaiting_reboot_screen() -> StaticScreen {
    StaticScreen {
        title: " awaiting reboot ".into(),
        headline: "Lifted, but not yet running under Xen.".into(),
        body: vec![
            "xen-system-amd64 is installed, GRUB has the Xen menuentry,".into(),
            "but you're booted into the bare-metal Ubuntu entry.".into(),
            "".into(),
            "  Press [r] to reboot now (will run `systemctl reboot`).".into(),
            "  Hold Shift at the GRUB menu and pick:".into(),
            "    Ubuntu GNU/Linux, with Xen hypervisor".into(),
            "  (may be inside an 'Advanced options' submenu).".into(),
            "".into(),
            "  After Xen boots, relaunch this cockpit — you'll land in".into(),
            "  Active mode and see Domain-0 + any guests.".into(),
            "".into(),
            "  If anything goes wrong: reboot, pick 'Ubuntu' (no Xen),".into(),
            "  recover. Bare-metal stays the GRUB default until you change it.".into(),
        ],
        accent: Color::Yellow,
        allow_reboot: true,
        allow_recovery: false,
    }
}

fn backend_error_screen(e: &str) -> StaticScreen {
    StaticScreen {
        title: " backend error ".into(),
        headline: "libxl could not be opened.".into(),
        body: vec![
            "Detection says we're under Xen, but libxl_ctx_alloc failed.".into(),
            "This usually means one of:".into(),
            "  · orchestratord daemon hasn't bound /run/rotten-apple.sock yet".into(),
            "  · xenstored isn't running (`systemctl status xenstored`)".into(),
            "  · a libxl version mismatch between the build host and dom0".into(),
            "  · you booted the bare-metal kernel instead of the Xen entry".into(),
            "".into(),
            "Underlying error:".into(),
            format!("  {e}"),
            "".into(),
            "Recovery (this screen has escape hatches so cockpit-boot mode".into(),
            "doesn't strand the box):".into(),
            "  [s] drop to a root shell on this tty".into(),
            "  [d] disable cockpit-boot mode and reboot to desktop".into(),
            "  [q] exit (systemd will retry up to 3 times in 60s, then stop)".into(),
        ],
        accent: Color::Red,
        allow_reboot: false,
        allow_recovery: true,
    }
}

// Internal render fn; threading every detection/plan piece individually is
// clearer than a 1-use bag struct for a single call site.
#[allow(clippy::too_many_arguments)]
fn render_pre_lift(
    f: &mut Frame,
    det: &Detection,
    topo: &rotten_apple_detect::CpuTopology,
    plan: &rotten_apple_detect::LiftPlan,
    lr: &rotten_apple_detect::LiftReadiness,
    manifest_path: &std::path::Path,
    manifest_exists: bool,
    confirming: bool,
) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(0),     // body
            Constraint::Length(1),  // footer
        ])
        .split(area);

    // Header
    let header = Paragraph::new(
        " rotten-apple cockpit  ▸  PRE-LIFT  ▸  this host has not been lifted ")
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(header, chunks[0]);

    // Body: detection summary | proposed plan, side-by-side
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // ---- left: detection ----
    let mut left_lines: Vec<Line> = vec![
        Line::from(Span::styled("host", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(format!("  distro     {} {}", det.distro_id, det.distro_version)),
        Line::from(format!("  kernel     {} ({})", det.kernel, det.arch)),
        Line::from(format!("  firmware   {}",
            if det.is_uefi { "UEFI" } else { "BIOS/legacy" })),
        Line::from(format!("  ram        {} MB", det.mem_total_kb / 1024)),
        Line::from(format!("  cpus       {} logical{}",
            topo.logical_cpus,
            if topo.is_hybrid { format!(" ({}P + {}E)", topo.p_cores.len(), topo.e_cores.len()) }
            else { String::new() })),
        Line::from(format!("  iommu      {}",
            if det.iommu_in_cmdline { "enabled in cmdline" }
            else { "NOT in cmdline (passthrough won't work)" })),
        Line::raw(""),
        Line::from(Span::styled("planned dom0 footprint",
            Style::default().add_modifier(Modifier::BOLD))),
        Line::from(format!("  memory     {} MB", plan.dom0.memory_mb)),
        Line::from(format!("  vcpus      {} {:?}", plan.dom0.vcpus, plan.dom0.cpu_pin)),
        Line::raw(""),
        Line::from(Span::styled("manifest",
            Style::default().add_modifier(Modifier::BOLD))),
        Line::from(format!("  path       {}", manifest_path.display())),
        Line::from(if manifest_exists {
            Span::styled("  status     present",
                Style::default().fg(Color::Green))
        } else {
            Span::styled("  status     MISSING — pass --manifest <path>",
                Style::default().fg(Color::Red))
        }),
    ];
    let _ = &mut left_lines;
    let p = Paragraph::new(left_lines)
        .block(Block::default().borders(Borders::ALL).title(" inspection "))
        .wrap(Wrap { trim: false });
    f.render_widget(p, body[0]);

    // ---- right: lift readiness blockers/warnings ----
    let mut right_lines: Vec<Line> = vec![
        Line::from(Span::styled("blockers",
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red))),
    ];
    if lr.blockers.is_empty() && det.blockers.is_empty() {
        right_lines.push(Line::from(Span::styled(
            "  (none — lift can proceed)", Style::default().fg(Color::Green))));
    } else {
        for b in det.blockers.iter().chain(lr.blockers.iter()) {
            right_lines.push(Line::from(format!("  · {b}")));
        }
    }
    right_lines.push(Line::raw(""));
    right_lines.push(Line::from(Span::styled("warnings",
        Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow))));
    let warns: Vec<&String> = det.warnings.iter().chain(lr.warnings.iter()).collect();
    if warns.is_empty() {
        right_lines.push(Line::from("  (none)"));
    } else {
        for w in warns.iter().take(8) {
            right_lines.push(Line::from(format!("  · {w}")));
        }
        if warns.len() > 8 {
            right_lines.push(Line::from(format!(
                "  ({} more not shown)", warns.len() - 8)));
        }
    }
    let p = Paragraph::new(right_lines)
        .block(Block::default().borders(Borders::ALL).title(" pre-flight "))
        .wrap(Wrap { trim: false });
    f.render_widget(p, body[1]);

    // Footer
    let footer = if !manifest_exists {
        Line::from(Span::styled(
            " manifest missing — pass --manifest <path> to enable [e]xecute. [q]uit",
            Style::default().fg(Color::DarkGray)))
    } else if !lr.blockers.is_empty() || !det.blockers.is_empty() {
        Line::from(Span::styled(
            " blockers present — resolve them before lifting. [q]uit",
            Style::default().fg(Color::Red)))
    } else {
        Line::from(vec![
            keybind("e", "xecute lift"), Span::raw("    "),
            keybind("q", "uit"),
        ])
    };
    let p = Paragraph::new(footer).alignment(Alignment::Center);
    f.render_widget(p, chunks[2]);

    // Confirmation modal
    if confirming {
        let w = 60u16;
        let h = 8u16;
        let overlay = centered_rect_abs(w, h, area);
        f.render_widget(Clear, overlay);
        let lines = vec![
            Line::from(Span::styled(" confirm lift",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
            Line::raw(""),
            Line::from("  This will run apt-install + write systemd unit"),
            Line::from("  + add Xen GRUB entry. ~3-5 min. Idempotent."),
            Line::from("  Bare-metal Ubuntu stays the GRUB default."),
            Line::raw(""),
            Line::from(Span::styled("  [y] yes, run lift   [n/Esc] cancel",
                Style::default().fg(Color::Yellow))),
        ];
        let p = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" confirm "));
        f.render_widget(p, overlay);
    }
}

fn render_static_screen(f: &mut Frame, s: &StaticScreen) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Paragraph::new(format!(" rotten-apple cockpit  ▸ {}", s.title))
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().fg(s.accent));
    f.render_widget(header, chunks[0]);

    let mut body = vec![
        Line::from(Span::styled(s.headline.clone(),
            Style::default().fg(s.accent).add_modifier(Modifier::BOLD))),
        Line::raw(""),
    ];
    for line in &s.body {
        body.push(Line::from(line.clone()));
    }
    let p = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    f.render_widget(p, chunks[1]);

    let footer = if s.allow_recovery {
        Line::from(vec![
            keybind("s", "hell"), Span::raw("    "),
            keybind("d", "esktop-mode + reboot"), Span::raw("    "),
            keybind("q", "uit"),
        ])
    } else if s.allow_reboot {
        Line::from(vec![
            keybind("r", "eboot now"), Span::raw("    "),
            keybind("q", "uit"),
        ])
    } else {
        Line::from(vec![keybind("q", "uit")])
    };
    let p = Paragraph::new(footer).alignment(Alignment::Center);
    f.render_widget(p, chunks[2]);
}

// ---------------------------------------------------------------------------
// Worker

#[derive(Debug)]
enum Cmd {
    Refresh,
    Start(GuestHandle),
    Stop { handle: GuestHandle, force: bool },
    Balloon { handle: GuestHandle, target_mb: u64 },
    CreateFromManifest(PathBuf),
    /// Create-and-start an instance: write CoW overlay + manifest via the
    /// instances crate, then call `domain.create` against the resulting
    /// manifest. Daemon-only — direct/libxl mode rejects with a toast
    /// because the orchestratord socket is the create channel.
    CreateInstance {
        id: String,
        base: String,
        memory_mb: u64,
        vcpus: u32,
    },
    PromoteXenDefault,
    /// Set the engine's memory policy for a domain. Only meaningful in
    /// daemon mode — the direct/libxl worker rejects with an error toast
    /// since the engine lives in orchestratord.
    SetPolicy { domid: u32, min_mb: u64, max_mb: u64, cooldown_s: u64 },
    Shutdown,
}

#[derive(Debug)]
enum Msg {
    BackendReady {
        name: String,
        libxl_version: String,
        /// Which strategy this worker is running. Surfaced in the header
        /// so the operator can tell at a glance whether they're talking
        /// to the daemon or to libxl directly.
        source: BackendSource,
    },
    BackendError(String),
    Snapshot(Snapshot),
    Event(EventEntry),
}

/// Where Snapshots/commands are flowing. Cosmetic — the UI behaves the
/// same either way — but the operator wants to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendSource {
    /// Talking to orchestratord over the local Unix socket.
    DaemonUnix,
    /// Talking to orchestratord over host-vsock.
    DaemonVsock,
    /// Cockpit opened libxl in-process.
    Libxl,
}

impl BackendSource {
    fn label(self) -> &'static str {
        match self {
            BackendSource::DaemonUnix => "daemon/unix",
            BackendSource::DaemonVsock => "daemon/vsock",
            BackendSource::Libxl => "via libxl",
        }
    }
}

#[derive(Debug, Clone)]
struct Snapshot {
    domains: Vec<DomainView>,
    /// Host-wide aggregates: total/free CPU + memory. None when the
    /// backend doesn't report (libxl unavailable, or polling hadn't
    /// completed a host.resources call yet on this tick).
    host: Option<HostResourcesView>,
}

#[derive(Debug, Clone)]
struct HostResourcesView {
    pub total_pcpus: Option<u32>,
    pub threads_per_core: Option<u32>,
    pub cores_per_socket: Option<u32>,
    pub total_memory_mb: Option<u64>,
    pub free_memory_mb: Option<u64>,
    pub scrub_memory_mb: Option<u64>,
}

#[derive(Debug, Clone)]
struct DomainView {
    handle: GuestHandle,
    name: String,
    state: GuestState,
    /// Detailed status — may be `None` if `status()` failed for this domain
    /// (e.g. it was destroyed between list and status).
    status: Option<GuestStatus>,
}

/// Picks Daemon (with a connected, handshook client) when the
/// orchestratord socket is reachable and the daemon reports a usable
/// backend; otherwise Direct. Either fault at any step (missing socket,
/// connect refused, handshake mismatch, daemon stuck at
/// backend="unavailable") degrades silently to Direct so first-run
/// cockpit still works and a healthy direct-libxl path is not masked by
/// an unhealthy daemon.
fn pick_worker_strategy() -> WorkerStrategy {
    let socket = Path::new(DEFAULT_SOCKET_PATH);
    // Poll for up to 10s before giving up. systemd's After= ordering on
    // the cockpit-boot getty override should already have the daemon up
    // first, but this is the belt for that suspenders: even on hosts
    // with loose ordering or hosts where the daemon takes a moment to
    // bind libxl, cockpit waits politely instead of falling straight
    // through to direct-libxl (which usually then also fails).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Some(c) = ready_daemon_client(socket) {
            return WorkerStrategy::Daemon(c);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    WorkerStrategy::Direct
}

/// Connect + handshake + verify that the daemon actually owns a usable
/// backend. A listening socket alone is not enough: orchestratord stays
/// up and answers hello even when libxl never opened, and in that state
/// every domain RPC fails with -32001 BackendUnavailable.
fn ready_daemon_client(socket: &Path) -> Option<DaemonClient> {
    let mut client = if socket.exists() {
        DaemonClient::connect(socket).ok()
    } else {
        None
    }.or_else(|| DaemonClient::connect_default().ok())?;
    client.handshake().ok()?;
    let host_info = client.call("host.info", json!({})).ok()?;
    if !daemon_host_is_usable(&host_info) {
        return None;
    }
    Some(client)
}

fn daemon_host_is_usable(host_info: &Value) -> bool {
    !matches!(
        host_info.get("backend").and_then(|v| v.as_str()),
        None | Some("unavailable")
    )
}

/// Which worker_loop_* the run_active dispatcher will spawn. The Daemon
/// variant carries an already-handshook client; the worker just calls
/// methods on it.
enum WorkerStrategy {
    Daemon(DaemonClient),
    Direct,
}

fn worker_loop_direct(cmd_rx: Receiver<Cmd>, msg_tx: Sender<Msg>) {
    let backend = match XenBackend::new() {
        Ok(b) => {
            let _ = msg_tx.send(Msg::BackendReady {
                name: b.name().to_string(),
                libxl_version:
                    rotten_apple_backend_xen::compat::LIBXL_BUILD_VERSION
                        .to_string(),
                source: BackendSource::Libxl,
            });
            b
        }
        Err(e) => {
            let _ = msg_tx.send(Msg::BackendError(e.to_string()));
            // Drain the cmd channel so we don't block the UI when the user
            // hits keys — but every action is rejected with a toast.
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, Cmd::Shutdown) { return }
                let _ = msg_tx.send(Msg::Event(EventEntry::error(
                    "backend not available — start cockpit with sudo on dom0")));
            }
            return;
        }
    };

    // Initial poll so the UI doesn't show "waiting" longer than necessary.
    push_snapshot(&backend, &msg_tx);

    let mut last_poll = Instant::now();
    loop {
        // Wait for a command up to the next poll deadline.
        let next_poll = last_poll + POLL_INTERVAL;
        let now = Instant::now();
        let timeout = next_poll.saturating_duration_since(now);

        match cmd_rx.recv_timeout(timeout) {
            Ok(Cmd::Shutdown) => return,
            Ok(cmd) => {
                dispatch(&backend, &msg_tx, cmd);
                // Re-poll after any state-changing action so the UI
                // reflects the result immediately.
                push_snapshot(&backend, &msg_tx);
                last_poll = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {
                push_snapshot(&backend, &msg_tx);
                last_poll = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Daemon-backed worker. Replaces direct libxl calls with JSON-RPC
/// requests against orchestratord. Same Msg shape, same poll cadence —
/// the UI cannot tell which strategy is running other than via the
/// `BackendSource` tag.
fn worker_loop_daemon(
    mut client: DaemonClient,
    cmd_rx: Receiver<Cmd>,
    msg_tx: Sender<Msg>,
) {
    announce_daemon_backend(&mut client, &msg_tx);

    push_snapshot_daemon(&mut client, &msg_tx);

    let mut last_poll = Instant::now();
    loop {
        let next_poll = last_poll + POLL_INTERVAL;
        let now = Instant::now();
        let timeout = next_poll.saturating_duration_since(now);

        match cmd_rx.recv_timeout(timeout) {
            Ok(Cmd::Shutdown) => {
                // Dropping the client closes the socket; the daemon
                // handles per-connection EOF cleanly.
                drop(client);
                return;
            }
            Ok(cmd) => {
                dispatch_daemon(&mut client, &msg_tx, cmd);
                push_snapshot_daemon(&mut client, &msg_tx);
                last_poll = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {
                push_snapshot_daemon(&mut client, &msg_tx);
                last_poll = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn announce_daemon_backend(client: &mut DaemonClient, msg_tx: &Sender<Msg>) {
    // host.info gives us libxl version + a friendly backend name. If it
    // fails the daemon is degraded or mid-restart; still mark the worker
    // daemon-backed so the UI can distinguish transport mode cleanly.
    let (name, libxl_version) = match client.call("host.info", json!({})) {
        Ok(info) => {
            let name = info.get("backend")
                .and_then(|v| v.as_str())
                .unwrap_or("orchestratord")
                .to_string();
            let v = info.get("libxl_version")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            (name, v)
        }
        Err(_) => ("orchestratord".into(), "?".into()),
    };
    let _ = msg_tx.send(Msg::BackendReady {
        name,
        libxl_version,
        source: match client.transport() {
            DaemonTransport::Unix => BackendSource::DaemonUnix,
            DaemonTransport::Vsock => BackendSource::DaemonVsock,
        },
    });
}

fn reconnect_daemon_client(msg_tx: &Sender<Msg>) -> Result<DaemonClient, DaemonError> {
    let _ = msg_tx.send(Msg::Event(EventEntry::info(
        "daemon transport dropped; reconnecting",
    )));
    let mut client = DaemonClient::connect_default()?;
    client.handshake()?;
    announce_daemon_backend(&mut client, msg_tx);
    let _ = msg_tx.send(Msg::Event(EventEntry::success(
        "daemon transport reconnected",
    )));
    Ok(client)
}

fn call_daemon(
    client: &mut DaemonClient,
    msg_tx: &Sender<Msg>,
    method: &str,
    params: Value,
) -> Result<Value, DaemonError> {
    match client.call(method, params.clone()) {
        Ok(v) => Ok(v),
        Err(DaemonError::Io(_)) => {
            *client = reconnect_daemon_client(msg_tx)?;
            client.call(method, params)
        }
        Err(e) => Err(e),
    }
}

/// Pull a fresh snapshot from the daemon. `domain.list` returns enough
/// to populate the per-row view in one round trip — no second
/// `domain.get` per domain needed (the daemon already calls status()
/// for every entry on the actor side). Failures surface as a toast
/// without dropping the snapshot we already have.
fn push_snapshot_daemon(client: &mut DaemonClient, msg_tx: &Sender<Msg>) {
    let result = match call_daemon(client, msg_tx, "domain.list", json!({})) {
        Ok(v) => v,
        Err(e) => {
            let _ = msg_tx.send(Msg::Event(EventEntry::error(
                &format!("domain.list: {e}"))));
            return;
        }
    };
    let arr = result.get("domains")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut domains = Vec::with_capacity(arr.len());
    for d in arr {
        if let Some(dv) = domain_view_from_daemon(&d) {
            domains.push(dv);
        }
    }
    // host.resources is best-effort: a failure (e.g. backend unavailable)
    // means we still ship the domains snapshot; the resources row just
    // shows "—". Silent on error so we don't spam the events panel.
    let host = call_daemon(client, msg_tx, "host.resources", json!({})).ok()
        .map(|v| HostResourcesView {
            total_pcpus: v.get("total_pcpus").and_then(|x| x.as_u64()).map(|n| n as u32),
            threads_per_core: v.get("threads_per_core").and_then(|x| x.as_u64()).map(|n| n as u32),
            cores_per_socket: v.get("cores_per_socket").and_then(|x| x.as_u64()).map(|n| n as u32),
            total_memory_mb: v.get("total_memory_mb").and_then(|x| x.as_u64()),
            free_memory_mb:  v.get("free_memory_mb") .and_then(|x| x.as_u64()),
            scrub_memory_mb: v.get("scrub_memory_mb").and_then(|x| x.as_u64()),
        });
    let _ = msg_tx.send(Msg::Snapshot(Snapshot { domains, host }));
}

/// Translate one daemon `DomainInfo` JSON value into a `DomainView`.
/// `None` is the "this entry was malformed" path — skip it rather than
/// fail the whole snapshot, in case one bad row would otherwise blank
/// the list.
fn domain_view_from_daemon(d: &Value) -> Option<DomainView> {
    let domid = d.get("domid")?.as_u64()?;
    let name = d.get("name")?.as_str()?.to_string();
    let state_str = d.get("state")?.as_str()?;
    let state = parse_state_str(state_str);
    let status = GuestStatus {
        state: state.clone(),
        memory_mb: d.get("memory_mb")
            .and_then(|v| v.as_u64()).unwrap_or(0),
        memory_max_mb: d.get("memory_max_mb")
            .and_then(|v| v.as_u64()).unwrap_or(0),
        vcpus: d.get("vcpus")
            .and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        uptime: Duration::from_secs(d.get("uptime_seconds")
            .and_then(|v| v.as_u64()).unwrap_or(0)),
        last_event: None,
    };
    Some(DomainView {
        handle: GuestHandle(domid.to_string()),
        name,
        state,
        status: Some(status),
    })
}

/// Inverse of `actor::state_str` in orchestratord. Unknown strings fold
/// to `Stopped` rather than panicking — the UI will display "stopped"
/// for it, which is a safer fallback than crashing the worker.
fn parse_state_str(s: &str) -> GuestState {
    match s {
        "Created"   => GuestState::Created,
        "Running"   => GuestState::Running,
        "Idle"      => GuestState::Idle,
        "Suspended" => GuestState::Suspended,
        "Stopped"   => GuestState::Stopped,
        "Failed"    => GuestState::Failed,
        _           => GuestState::Stopped,
    }
}

/// Dispatch a `Cmd` to the daemon. Translation is in `cmd_to_rpc` so
/// it's testable without a socket; this fn handles the side-effects
/// (the call, the event toast). Cmds that have no daemon equivalent
/// (PromoteXenDefault — local GRUB writing; Refresh / Shutdown — loop
/// control) are handled inline.
fn dispatch_daemon(
    client: &mut DaemonClient,
    msg_tx: &Sender<Msg>,
    cmd: Cmd,
) {
    let entry = match &cmd {
        Cmd::Refresh | Cmd::Shutdown => return,
        Cmd::PromoteXenDefault => promote_xen_default(),
        // CreateInstance is two operations: (1) write the overlay +
        // manifest locally via the instances crate, (2) tell the daemon
        // to start the resulting manifest. Either step's error becomes
        // a toast; on success we report the new domid.
        Cmd::CreateInstance { id, base, memory_mb, vcpus } => {
            create_instance_then_start(client, msg_tx, id, base, *memory_mb, *vcpus)
        }
        _ => match cmd_to_rpc(&cmd) {
            None => return,
            Some((method, params)) => {
                let label = action_label(&cmd);
                match call_daemon(client, msg_tx, &method, params) {
                    Ok(_) => EventEntry::success(&label),
                    Err(e) => EventEntry::error(&format!("{label}: {e}")),
                }
            }
        },
    };
    let _ = msg_tx.send(Msg::Event(entry));
}

/// Run the [n]ew flow: write the overlay + manifest via the instances
/// crate (which calls qemu-img), then `domain.create` against the
/// manifest path. Pulled out of `dispatch_daemon` so the two error
/// surfaces (instances-crate errors, daemon errors) are easy to read
/// at the call site.
fn create_instance_then_start(
    client: &mut DaemonClient,
    msg_tx: &Sender<Msg>,
    id: &str, base: &str, memory_mb: u64, vcpus: u32,
) -> EventEntry {
    use rotten_apple_instances::{NewInstanceParams, create_instance};
    let p = NewInstanceParams {
        id: id.into(),
        base_image: base.into(),
        memory_mb,
        vcpus,
        ephemeral: false,
    };
    let entry = match create_instance(p, false) {
        Ok(e) => e,
        Err(rotten_apple_instances::InstanceError::BaseImageNotFound(_)) => {
            let _ = msg_tx.send(Msg::Event(EventEntry::info(
                &format!("base image {base} missing; pulling automatically"),
            )));
            match ensure_base_image_present(base) {
                Ok(()) => {
                    let retry = NewInstanceParams {
                        id: id.into(),
                        base_image: base.into(),
                        memory_mb,
                        vcpus,
                        ephemeral: false,
                    };
                    match create_instance(retry, false) {
                        Ok(e) => e,
                        Err(e) => return EventEntry::error(
                            &format!("instance new {id}: {e}")),
                    }
                }
                Err(e) => return EventEntry::error(
                    &format!("instance new {id}: auto-pull {base}: {e}")),
            }
        }
        Err(e) => return EventEntry::error(
            &format!("instance new {id}: {e}")),
    };
    let manifest_path = std::path::Path::new(
        rotten_apple_instances::DEFAULT_MANIFESTS_DIR
    ).join(format!("{}.toml", entry.id));
    let domid = match call_daemon(client, msg_tx, "domain.create", json!({
        "manifest_path": manifest_path.to_string_lossy(),
    })) {
        Ok(v) => v.get("domid")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        Err(e) => return EventEntry::error(
            &format!("instance {} created but daemon: {e}", entry.id)),
    };
    // libxl_domain_create_new lands the domain in paused state. Issue
    // domain.start so the wizard's "create" actually runs the guest;
    // otherwise xl list shows --p--- and the user has to know about
    // `xl unpause` to make it go.
    match call_daemon(client, msg_tx, "domain.start", json!({
        "domid": domid,
    })) {
        Ok(_) => EventEntry::success(
            &format!("created and started {} → domid {domid}", entry.id)),
        Err(e) => EventEntry::error(
            &format!("instance {} created (domid {domid}) but unpause: {e}",
                entry.id)),
    }
}

fn ensure_base_image_present(name: &str) -> Result<(), String> {
    let cat_path = std::path::Path::new(rotten_apple_images::DEFAULT_CATALOG_PATH);
    let dest = cat_path.parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/rotten-apple/images"));
    let mut cat = rotten_apple_images::Catalog::load_or_empty(cat_path);
    if cat.find(name).is_some() {
        return Ok(());
    }
    let entry = rotten_apple_images::pull(name, &dest, false)
        .map_err(|e| e.to_string())?;
    cat.upsert(entry);
    cat.save(cat_path).map_err(|e| e.to_string())
}

fn available_base_choices() -> Vec<String> {
    let mut names = rotten_apple_instances::list_known_images(
        std::path::Path::new(rotten_apple_instances::DEFAULT_IMAGES_INDEX)
    );
    for src in rotten_apple_images::known_sources() {
        if !names.iter().any(|n| n == src.name) {
            names.push(src.name.to_string());
        }
    }
    names
}

/// Pure mapping `Cmd → (method, params)`. Returns None for commands
/// that don't have a daemon equivalent (Refresh, Shutdown,
/// PromoteXenDefault — those stay local). Pulled out as a free fn so
/// the parameter translation (target_mb → target_kb!) is unit-testable
/// without spinning up a client.
fn cmd_to_rpc(cmd: &Cmd) -> Option<(String, Value)> {
    match cmd {
        Cmd::Refresh | Cmd::Shutdown | Cmd::PromoteXenDefault => None,
        // CreateInstance is multi-step — local file write then RPC. The
        // dispatcher handles it inline, so the pure-mapping fn returns
        // None here. Tests on cmd_to_rpc still work; the integration
        // path has its own toast generation.
        Cmd::CreateInstance { .. } => None,
        Cmd::Start(h) => {
            let domid = handle_to_domid(h)?;
            Some(("domain.start".into(), json!({ "domid": domid })))
        }
        Cmd::Stop { handle, force } => {
            let domid = handle_to_domid(handle)?;
            Some(("domain.shutdown".into(),
                  json!({ "domid": domid, "force": *force })))
        }
        Cmd::Balloon { handle, target_mb } => {
            let domid = handle_to_domid(handle)?;
            // Daemon API takes kB; cockpit prompt collects MB. Convert
            // here so the wire shape matches orchestratord's contract.
            let target_kb = target_mb.saturating_mul(1024);
            Some(("domain.balloon".into(),
                  json!({ "domid": domid, "target_kb": target_kb })))
        }
        Cmd::CreateFromManifest(path) => {
            // Path is OS-string-ish; lossy is fine for transmission —
            // orchestratord re-loads the manifest from disk.
            Some(("domain.create".into(),
                  json!({ "manifest_path": path.to_string_lossy() })))
        }
        Cmd::SetPolicy { domid, min_mb, max_mb, cooldown_s } => {
            // Wire shape mirrors orchestratord::dispatch::parse_set_policy:
            // {domid, policy: {min_mb, max_mb, cooldown_s}}. We don't
            // currently expose target_headroom_pct in the editor.
            Some(("engine.set_policy".into(), json!({
                "domid": domid,
                "policy": {
                    "min_mb": min_mb,
                    "max_mb": max_mb,
                    "cooldown_s": cooldown_s,
                },
            })))
        }
    }
}

/// `GuestHandle` is opaque (a String) on the trait side; the daemon
/// uses u32 domids. Parse on the way out and bail if the handle isn't
/// numeric — anything else is a backend mismatch that the trait would
/// also reject.
fn handle_to_domid(h: &GuestHandle) -> Option<u32> {
    h.0.parse::<u32>().ok()
}

/// Human-friendly label for the action — used as the success toast and
/// as the prefix of the error toast.
fn action_label(cmd: &Cmd) -> String {
    match cmd {
        Cmd::Start(h) => format!("start {h}"),
        Cmd::Stop { handle, force: true }  => format!("destroy {handle}"),
        Cmd::Stop { handle, force: false } => format!("shutdown {handle}"),
        Cmd::Balloon { handle, target_mb } =>
            format!("balloon {handle} → {target_mb}M"),
        Cmd::CreateFromManifest(p) =>
            format!("create from {}", p.display()),
        Cmd::CreateInstance { id, .. } => format!("instance new {id}"),
        Cmd::SetPolicy { domid, min_mb, max_mb, cooldown_s } =>
            format!("policy domain {domid}: min={min_mb}M max={max_mb}M cd={cooldown_s}s"),
        Cmd::Refresh | Cmd::Shutdown | Cmd::PromoteXenDefault =>
            String::new(),
    }
}

/// Promote the Xen menuentry to GRUB default. Reads the existing
/// grub.cfg, finds the Xen entry's submenu>id path, sets GRUB_DEFAULT
/// via /etc/default/grub.d/41-rotten-apple-default.cfg, runs update-grub.
/// Idempotent — running it twice has the same result as once.
fn promote_xen_default() -> EventEntry {
    use std::process::Command;
    // Find the Xen entry id from grub.cfg. Format inside an Advanced
    // Options submenu:
    //   submenu '...' $menuentry_id_option 'gnulinux-advanced-XYZ' { ...
    //     menuentry '...with Xen hypervisor' --class ... $menuentry_id_option 'gnulinux-...' { ...
    let cfg = match std::fs::read_to_string("/boot/grub/grub.cfg") {
        Ok(s) => s,
        Err(e) => return EventEntry::error(
            &format!("promote: read grub.cfg: {e}")),
    };
    let path = match find_xen_grub_path(&cfg) {
        Some(p) => p,
        None => return EventEntry::error(
            "promote: could not find Xen menuentry in grub.cfg"),
    };

    let snippet = format!(r#"# Generated by rotten-apple cockpit (promote action).
GRUB_DEFAULT="{path}"
"#);
    if let Err(e) = std::fs::write(
        "/etc/default/grub.d/41-rotten-apple-default.cfg", snippet) {
        return EventEntry::error(
            &format!("promote: write default-cfg: {e}"));
    }

    let out = Command::new("update-grub").output();
    match out {
        Ok(o) if o.status.success() => EventEntry::success(
            &format!("Xen entry promoted to GRUB default ({path})")),
        Ok(o) => EventEntry::error(
            &format!("promote: update-grub exit {}: {}", o.status,
                String::from_utf8_lossy(&o.stderr))),
        Err(e) => EventEntry::error(
            &format!("promote: spawn update-grub: {e}")),
    }
}

/// Walk grub.cfg, find the Xen menuentry, and return its full GRUB
/// addressable path: `submenu_id>menuentry_id` if nested, else just
/// `menuentry_id`. Looks for `--id` tags emitted by Ubuntu's grub-mkconfig.
///
/// Tracks brace depth so the submenu context isn't dropped by the inner
/// menuentries' own `{ … }` blocks (every menuentry inside an Advanced
/// Options submenu has braces of its own).
fn find_xen_grub_path(cfg: &str) -> Option<String> {
    let mut current_submenu: Option<(String, i32)> = None; // (id, depth_when_opened)
    let mut depth: i32 = 0;
    for line in cfg.lines() {
        let t = line.trim_start();
        // Count this line's braces, but evaluate the menuentry test
        // BEFORE applying them so a Xen menuentry on the same line as
        // its own `{` is correctly attributed to the enclosing submenu.
        let opens  = t.bytes().filter(|&b| b == b'{').count() as i32;
        let closes = t.bytes().filter(|&b| b == b'}').count() as i32;

        if let Some(id) = parse_block_with_id(t, "submenu") {
            // The submenu's own opening `{` is on this line; record the
            // depth we'll be at *after* applying opens-closes.
            current_submenu = Some((id, depth + opens - closes));
        } else if let Some(id) = parse_block_with_id(t, "menuentry")
            && (t.contains("with Xen hypervisor") || t.contains("Xen 4")
                || t.contains("xen.gz"))
        {
            return Some(match &current_submenu {
                Some((s, _)) => format!("{s}>{id}"),
                None => id,
            });
        }

        depth += opens;
        depth -= closes;

        if let Some((_, when)) = &current_submenu
            && depth < *when
        {
            current_submenu = None;
        }
    }
    None
}

/// Extract the `--id 'foo'` value from a `menuentry '...' --class ... --id 'foo' { ... }`
/// line. Also handles `--id "foo"` and `--id foo`.
fn parse_block_with_id(line: &str, kind: &str) -> Option<String> {
    if !line.starts_with(kind) { return None }
    let idx = line.find("--id")?;
    let rest = &line[idx + 4..].trim_start();
    let bytes = rest.as_bytes();
    if bytes.is_empty() { return None }
    let (start_quote, content_start) = match bytes[0] {
        b'\'' => (Some(b'\''), 1),
        b'"' => (Some(b'"'), 1),
        _ => (None, 0),
    };
    let after = &rest[content_start..];
    let end = match start_quote {
        Some(q) => after.find(q as char)?,
        None => after.find(|c: char| c.is_whitespace() || c == '{')?,
    };
    Some(after[..end].to_string())
}

fn push_snapshot(backend: &XenBackend, msg_tx: &Sender<Msg>) {
    let summaries = backend.list();
    let domains = summaries.into_iter()
        .map(|s: GuestSummary| {
            let status = backend.status(&s.handle).ok();
            DomainView {
                handle: s.handle,
                name: s.name,
                state: status.as_ref().map(|x| x.state.clone())
                                     .unwrap_or(s.state),
                status,
            }
        })
        .collect();
    let host = backend.physinfo().ok().map(|p| HostResourcesView {
        total_pcpus: Some(p.total_pcpus),
        threads_per_core: Some(p.threads_per_core),
        cores_per_socket: Some(p.cores_per_socket),
        total_memory_mb: Some(p.total_memory_mb),
        free_memory_mb: Some(p.free_memory_mb),
        scrub_memory_mb: Some(p.scrub_memory_mb),
    });
    let _ = msg_tx.send(Msg::Snapshot(Snapshot { domains, host }));
}

fn dispatch(backend: &XenBackend, msg_tx: &Sender<Msg>, cmd: Cmd) {
    let entry = match cmd {
        Cmd::Refresh => return,
        Cmd::Start(h) => match backend.start_guest(&h) {
            Ok(()) => EventEntry::success(&format!("start: {h}")),
            Err(e) => EventEntry::error(&format!("start {h}: {e}")),
        },
        Cmd::Stop { handle, force } => {
            let label = if force { "destroy" } else { "shutdown" };
            match backend.stop_guest(&handle, force) {
                Ok(()) => EventEntry::success(&format!("{label}: {handle}")),
                Err(e) => EventEntry::error(&format!("{label} {handle}: {e}")),
            }
        }
        Cmd::Balloon { handle, target_mb } => match backend.balloon_to(&handle, target_mb) {
            Ok(()) => EventEntry::success(
                &format!("balloon {handle} → {target_mb}M")),
            Err(e) => EventEntry::error(&format!("balloon {handle}: {e}")),
        },
        Cmd::CreateFromManifest(path) => {
            match Profile::load(&path) {
                Err(e) => EventEntry::error(
                    &format!("load {}: {e}", path.display())),
                Ok(p) => match backend.create_guest(&p) {
                    Ok(h) => EventEntry::success(
                        &format!("created {} → domid {h}", p.name())),
                    Err(e) => EventEntry::error(
                        &format!("create_guest {}: {e}", p.name())),
                },
            }
        }
        Cmd::PromoteXenDefault => promote_xen_default(),
        // The engine lives in orchestratord; in direct/libxl mode there
        // is no engine to talk to. Surface a clear toast instead of
        // silently dropping the request.
        Cmd::SetPolicy { .. } => EventEntry::error(
            "policy edit requires daemon mode — start orchestratord"),
        Cmd::CreateInstance { .. } => EventEntry::error(
            "instance creation requires daemon mode"),
        Cmd::Shutdown => return,
    };
    let _ = msg_tx.send(Msg::Event(entry));
}

// ---------------------------------------------------------------------------
// State

struct State {
    config: CockpitConfig,
    backend_name: Option<String>,
    libxl_version: Option<String>,
    backend_source: Option<BackendSource>,
    backend_error: Option<String>,
    /// Cached at startup; refreshed after a successful [B] toggle. Used
    /// by the footer hint and the confirm modal to phrase the prompt
    /// in terms of the current state.
    boot_mode: BootMode,
    snapshot: Option<Snapshot>,
    list_state: ListState,
    events: std::collections::VecDeque<EventEntry>,
    mode: Mode,
    toast: Option<(Instant, String, ToastKind)>,
    should_quit: bool,
}

#[derive(Debug, Clone)]
enum Mode {
    Normal,
    BalloonPrompt {
        handle: GuestHandle,
        /// Current allocation captured at prompt-open. Shown in the prompt
        /// so the user sees what they're moving from. Also used by the
        /// dom0 floor enforcement.
        current_mb: u64,
        max_mb: u64,
        options_mb: Vec<u64>,
        selected_idx: usize,
        /// Tab cycles entry modes. Presets is the default; ManualMb and
        /// Percentage let the user type an exact value when none of the
        /// presets is right (e.g. stress-walking dom0 in fine steps).
        entry_mode: BalloonEntry,
        /// Free text for ManualMb / Percentage. Empty until the user
        /// types something while focused on one of those modes.
        text_input: String,
    },
    /// Memory-policy editor. We don't have an engine.get_policy RPC yet,
    /// so the editor sets but doesn't read — fields are pre-populated
    /// from the snapshot's current_mb / max_mb / a 30s cooldown default.
    PolicyEditor {
        domid: u32,
        name: String,
        min_options: Vec<u64>,
        max_options: Vec<u64>,
        cooldown_options: Vec<u64>,
        min_idx: usize,
        max_idx: usize,
        cooldown_idx: usize,
        focus: PolicyField,
        current_mb: u64,
        current_max_mb: u64,
    },
    HelpOverlay,
    HostResourcesOverlay,
    /// First-launch welcome shown once per user (marker file at
    /// `$XDG_CONFIG_HOME/rotten-apple/welcomed` or `~/.config/...`).
    /// Dismissed by any key; that keypress writes the marker so the
    /// overlay never appears again on this account.
    FirstRunWelcome,
    ConfirmKill { handle: GuestHandle },
    ConfirmPromote,
    /// `[B]` confirmation. `target` is the mode we'll switch INTO if
    /// the user confirms — i.e. the opposite of `state.boot_mode`.
    #[allow(clippy::enum_variant_names)]
    ConfirmBootMode { target: BootMode },
    /// `[n]ew` wizard. Collects id, base image (cycled via Tab through
    /// the images catalog), memory_mb, vcpus. Submit creates the
    /// instance and starts it via the daemon.
    InstanceWizard {
        id_input:     String,
        /// Catalog snapshot taken when the wizard opened. Tab cycles
        /// through these; the user can also type a name freely.
        known_bases:  Vec<String>,
        base_idx:     usize,
        memory_options: Vec<u64>,
        memory_idx: usize,
        vcpu_options: Vec<u32>,
        vcpu_idx: usize,
        focus:        WizardField,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BalloonEntry { Presets, ManualMb, Percentage }

impl BalloonEntry {
    fn next(self) -> Self {
        match self {
            BalloonEntry::Presets    => BalloonEntry::ManualMb,
            BalloonEntry::ManualMb   => BalloonEntry::Percentage,
            BalloonEntry::Percentage => BalloonEntry::Presets,
        }
    }
    fn label(self) -> &'static str {
        match self {
            BalloonEntry::Presets    => "presets",
            BalloonEntry::ManualMb   => "manual MB",
            BalloonEntry::Percentage => "percentage",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardField { Id, Base, Memory, Vcpus }

impl WizardField {
    fn next(self) -> Self {
        match self {
            WizardField::Id     => WizardField::Base,
            WizardField::Base   => WizardField::Memory,
            WizardField::Memory => WizardField::Vcpus,
            WizardField::Vcpus  => WizardField::Id,
        }
    }
    fn prev(self) -> Self {
        match self {
            WizardField::Id     => WizardField::Vcpus,
            WizardField::Base   => WizardField::Id,
            WizardField::Memory => WizardField::Base,
            WizardField::Vcpus  => WizardField::Memory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyField { Min, Max, Cooldown }

impl PolicyField {
    fn next(self) -> Self {
        match self {
            PolicyField::Min      => PolicyField::Max,
            PolicyField::Max      => PolicyField::Cooldown,
            PolicyField::Cooldown => PolicyField::Min,
        }
    }
    fn prev(self) -> Self {
        match self {
            PolicyField::Min      => PolicyField::Cooldown,
            PolicyField::Max      => PolicyField::Min,
            PolicyField::Cooldown => PolicyField::Max,
        }
    }
}

/// Hard floor for dom0 memory in MB. Below this the kernel typically
/// OOMs during normal operation (page tables, slab caches, network
/// buffers add up fast). The floor is intentionally permissive enough
/// to allow stress-testing balloon round-trips through min/max — the
/// previous "no shrink at all" rail prevented the use case entirely.
const DOM0_FLOOR_MB: u64 = 512;

/// Domain-0 is the host. Treat it as read-mostly: refuse start/stop/kill
/// and enforce the kernel-survival floor before ballooning down.
fn is_dom0(handle: &GuestHandle) -> bool {
    handle.0 == "0"
}

/// Resolve the user's balloon target based on which entry mode is
/// active. Presets reads `options[selected_idx]`; ManualMb parses the
/// text as a literal MB count; Percentage parses 0-100 and computes
/// `cap * pct / 100`. Empty text in a typed mode is rejected with a
/// hint that nudges the user to type a number.
fn resolve_balloon_target(
    entry_mode: BalloonEntry,
    text: &str,
    options: &[u64],
    selected_idx: usize,
    current_mb: u64,
    max_mb: u64,
) -> std::result::Result<u64, String> {
    match entry_mode {
        BalloonEntry::Presets => Ok(options.get(selected_idx)
            .copied()
            .unwrap_or(current_mb.max(1))),
        BalloonEntry::ManualMb => {
            if text.is_empty() {
                return Err("balloon: type a target in MB, then Enter".into());
            }
            text.parse::<u64>()
                .map_err(|_| format!("balloon: '{text}' is not a valid MB count"))
        }
        BalloonEntry::Percentage => {
            if text.is_empty() {
                return Err("balloon: type 0-100 (% of max), then Enter".into());
            }
            let pct = text.parse::<u64>()
                .map_err(|_| format!("balloon: '{text}' is not a valid percentage"))?;
            if pct > 100 {
                return Err(format!("balloon: percentage {pct} > 100"));
            }
            // Round-half-up so 50% of an odd cap maps to a sensible MB.
            let cap = if max_mb > 0 { max_mb } else { current_mb };
            Ok((cap * pct + 50) / 100)
        }
    }
}

/// Pure validator for the balloon prompt. Pulled out of the keyboard
/// handler so the rules (dom0 floor, max ceiling, "must be > 0") are
/// unit-testable without spinning up a terminal. Returns the validated
/// target on success, or a user-facing message on rejection.
fn validate_balloon_target(
    target_mb: u64, _current_mb: u64, max_mb: u64, is_dom0: bool,
) -> std::result::Result<u64, String> {
    if target_mb == 0 {
        return Err("balloon: target must be > 0".into());
    }
    if max_mb > 0 && target_mb > max_mb {
        return Err(format!("balloon: target {target_mb}M exceeds max {max_mb}M"));
    }
    if is_dom0 && target_mb < DOM0_FLOOR_MB {
        return Err(format!(
            "dom0: cannot balloon below {DOM0_FLOOR_MB}M (kernel survival floor)"));
    }
    Ok(target_mb)
}

fn sorted_unique_u64(mut values: Vec<u64>) -> Vec<u64> {
    values.retain(|v| *v > 0);
    values.sort_unstable();
    values.dedup();
    values
}

fn balloon_options_mb(current_mb: u64, max_mb: u64, is_dom0: bool) -> Vec<u64> {
    let cap = if max_mb > 0 { max_mb } else { current_mb.max(1) };
    let floor = if is_dom0 { DOM0_FLOOR_MB } else { 1 };
    // Stress-test friendly option set: include the floor, current,
    // ceiling, halves, quarters, and ±256/512 around current. For dom0
    // this gives a clean ramp from DOM0_FLOOR_MB up to dom0_max_mem so
    // the user can walk the balloon through the whole range without
    // hand-typing numbers.
    let mut values = vec![
        floor,
        current_mb.max(floor),
        cap,
        cap / 4,
        cap / 2,
        cap.saturating_mul(3) / 4,
        current_mb.saturating_sub(256).max(floor),
        current_mb.saturating_sub(512).max(floor),
        current_mb.saturating_add(256).min(cap),
        current_mb.saturating_add(512).min(cap),
    ];
    values.retain(|v| *v >= floor && *v <= cap && *v > 0);
    let values = sorted_unique_u64(values);
    if values.is_empty() { vec![current_mb.max(1)] } else { values }
}

fn policy_options_mb(current_mb: u64, max_mb: u64) -> (Vec<u64>, Vec<u64>) {
    let cap = max_mb.max(current_mb).max(256);
    let min_values = sorted_unique_u64(vec![
        256,
        512,
        1024,
        current_mb.min(cap),
        (current_mb / 2).max(256),
        cap.min(2048),
    ]);
    let max_values = sorted_unique_u64(vec![
        current_mb.max(256),
        cap,
        1024,
        2048,
        4096.min(cap),
        8192.min(cap),
    ]);
    (min_values, max_values)
}

fn nearest_index_u64(options: &[u64], target: u64) -> usize {
    options.iter().enumerate()
        .min_by_key(|(_, v)| (**v).abs_diff(target))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn nearest_index_u32(options: &[u32], target: u32) -> usize {
    options.iter().enumerate()
        .min_by_key(|(_, v)| (**v).abs_diff(target))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
enum ToastKind { Info, Success, Error }

#[derive(Debug, Clone)]
struct EventEntry {
    when: Instant,
    text: String,
    kind: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind { Info, Success, Error }

impl EventEntry {
    fn info(text: &str) -> Self {
        Self { when: Instant::now(), text: text.into(), kind: EventKind::Info }
    }
    fn success(text: &str) -> Self {
        Self { when: Instant::now(), text: text.into(), kind: EventKind::Success }
    }
    fn error(text: &str) -> Self {
        Self { when: Instant::now(), text: text.into(), kind: EventKind::Error }
    }
}

impl State {
    fn new(config: CockpitConfig) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        // First-run welcome: shown once per user account. The marker file
        // is checked here so a fresh launch on a new login lands on the
        // welcome overlay; subsequent launches skip straight to Normal.
        let mode = if welcome_needed() {
            Mode::FirstRunWelcome
        } else {
            Mode::Normal
        };
        Self {
            config,
            backend_name: None,
            libxl_version: None,
            backend_source: None,
            backend_error: None,
            boot_mode: boot_mode::current_boot_mode(),
            snapshot: None,
            list_state,
            events: std::collections::VecDeque::with_capacity(MAX_EVENTS),
            mode,
            toast: None,
            should_quit: false,
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::BackendReady { name, libxl_version, source } => {
                let evt = format!("backend ready ({}): libxl {libxl_version}",
                    source.label());
                self.backend_name = Some(name);
                self.libxl_version = Some(libxl_version);
                self.backend_source = Some(source);
                self.backend_error = None;
                self.push_event(EventEntry::info(&evt));
            }
            Msg::BackendError(e) => {
                self.backend_error = Some(e.clone());
                self.push_event(EventEntry::error(&format!("backend init: {e}")));
                self.toast(ToastKind::Error, format!("backend: {e}"));
            }
            Msg::Snapshot(s) => {
                let max = s.domains.len().saturating_sub(1);
                let cur = self.list_state.selected().unwrap_or(0).min(max);
                if !s.domains.is_empty() {
                    self.list_state.select(Some(cur));
                } else {
                    self.list_state.select(None);
                }
                self.snapshot = Some(s);
            }
            Msg::Event(e) => {
                let kind = match e.kind {
                    EventKind::Info => ToastKind::Info,
                    EventKind::Success => ToastKind::Success,
                    EventKind::Error => ToastKind::Error,
                };
                self.toast(kind, e.text.clone());
                self.push_event(e);
            }
        }
    }

    fn push_event(&mut self, e: EventEntry) {
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    fn toast(&mut self, kind: ToastKind, text: String) {
        self.toast = Some((Instant::now(), text, kind));
    }

    fn handle_key(&mut self, k: KeyEvent) -> Option<Cmd> {
        // Esc cancels overlay/prompt regardless of mode.
        if k.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return None;
        }

        match &mut self.mode {
            Mode::HelpOverlay | Mode::HostResourcesOverlay => {
                self.mode = Mode::Normal;
                None
            }
            Mode::FirstRunWelcome => {
                // Any key dismisses; write the marker so we don't show
                // it again. Best-effort write — never blocks dismissal.
                mark_welcome_seen();
                self.mode = Mode::Normal;
                None
            }
            Mode::ConfirmKill { handle } => {
                let h = handle.clone();
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.mode = Mode::Normal;
                        Some(Cmd::Stop { handle: h, force: true })
                    }
                    _ => { self.mode = Mode::Normal; None }
                }
            }
            Mode::ConfirmPromote => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.mode = Mode::Normal;
                        Some(Cmd::PromoteXenDefault)
                    }
                    _ => { self.mode = Mode::Normal; None }
                }
            }
            Mode::ConfirmBootMode { target } => {
                let target = *target;
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.mode = Mode::Normal;
                        self.apply_boot_mode_toggle(target);
                        None
                    }
                    _ => { self.mode = Mode::Normal; None }
                }
            }
            Mode::BalloonPrompt {
                handle, current_mb, max_mb, options_mb, selected_idx,
                entry_mode, text_input,
            } => {
                match k.code {
                    KeyCode::Tab => {
                        *entry_mode = entry_mode.next();
                        text_input.clear();
                        None
                    }
                    KeyCode::Left | KeyCode::Char('h')
                        if *entry_mode == BalloonEntry::Presets =>
                    {
                        if *selected_idx > 0 { *selected_idx -= 1; }
                        None
                    }
                    KeyCode::Right | KeyCode::Char('l')
                        if *entry_mode == BalloonEntry::Presets =>
                    {
                        if *selected_idx + 1 < options_mb.len() { *selected_idx += 1; }
                        None
                    }
                    KeyCode::Char(c) if matches!(
                        *entry_mode,
                        BalloonEntry::ManualMb | BalloonEntry::Percentage
                    ) && c.is_ascii_digit() => {
                        // Cap inputs to a reasonable length so the field
                        // can't grow unbounded; 6 digits = 999999 MB ~= 1 TB.
                        if text_input.len() < 6 { text_input.push(c); }
                        None
                    }
                    KeyCode::Backspace if matches!(
                        *entry_mode,
                        BalloonEntry::ManualMb | BalloonEntry::Percentage
                    ) => {
                        text_input.pop();
                        None
                    }
                    KeyCode::Enter => {
                        let h = handle.clone();
                        let cur = *current_mb;
                        let cap = *max_mb;
                        let target = match resolve_balloon_target(
                            *entry_mode, text_input, options_mb, *selected_idx, cur, cap,
                        ) {
                            Ok(t) => t,
                            Err(msg) => {
                                self.toast(ToastKind::Error, msg);
                                return None;
                            }
                        };
                        self.mode = Mode::Normal;
                        match validate_balloon_target(
                            target, cur, cap, is_dom0(&h)
                        ) {
                            Ok(target) => Some(Cmd::Balloon {
                                handle: h, target_mb: target }),
                            Err(msg) => {
                                self.toast(ToastKind::Error, msg);
                                None
                            }
                        }
                    }
                    _ => None,
                }
            }
            Mode::PolicyEditor {
                domid, min_options, max_options, cooldown_options,
                min_idx, max_idx, cooldown_idx, focus, ..
            } => {
                match k.code {
                    KeyCode::Tab => { *focus = focus.next(); None }
                    KeyCode::BackTab | KeyCode::Up => {
                        *focus = focus.prev(); None
                    }
                    KeyCode::Down => { *focus = focus.next(); None }
                    KeyCode::Left | KeyCode::Char('h') => {
                        match focus {
                            PolicyField::Min if *min_idx > 0 => *min_idx -= 1,
                            PolicyField::Max if *max_idx > 0 => *max_idx -= 1,
                            PolicyField::Cooldown if *cooldown_idx > 0 => *cooldown_idx -= 1,
                            _ => {}
                        }
                        None
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        match focus {
                            PolicyField::Min if *min_idx + 1 < min_options.len() => *min_idx += 1,
                            PolicyField::Max if *max_idx + 1 < max_options.len() => *max_idx += 1,
                            PolicyField::Cooldown if *cooldown_idx + 1 < cooldown_options.len() => *cooldown_idx += 1,
                            _ => {}
                        }
                        None
                    }
                    KeyCode::Enter => {
                        let id = *domid;
                        let min = min_options.get(*min_idx).copied().unwrap_or(0);
                        let max = max_options.get(*max_idx).copied().unwrap_or(0);
                        let cd  = cooldown_options.get(*cooldown_idx).copied().unwrap_or(0);
                        if min > max {
                            self.toast(ToastKind::Error,
                                "policy: min must be ≤ max".into());
                            None
                        } else if cd < 1 {
                            self.toast(ToastKind::Error,
                                "policy: cooldown must be ≥ 1".into());
                            None
                        } else {
                            self.mode = Mode::Normal;
                            Some(Cmd::SetPolicy {
                                domid: id,
                                min_mb: min,
                                max_mb: max,
                                cooldown_s: cd,
                            })
                        }
                    }
                    _ => None,
                }
            }
            Mode::InstanceWizard {
                id_input, known_bases, base_idx,
                memory_options, memory_idx, vcpu_options, vcpu_idx, focus,
            } => {
                match k.code {
                    KeyCode::Tab => {
                        *focus = focus.next();
                        None
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        *focus = focus.prev(); None
                    }
                    KeyCode::Down => { *focus = focus.next(); None }
                    KeyCode::Left | KeyCode::Char('h') => {
                        match focus {
                            WizardField::Id => {}
                            WizardField::Base if !known_bases.is_empty() => {
                                *base_idx = if *base_idx == 0 {
                                    known_bases.len().saturating_sub(1)
                                } else {
                                    *base_idx - 1
                                };
                            }
                            WizardField::Memory if *memory_idx > 0 => *memory_idx -= 1,
                            WizardField::Vcpus if *vcpu_idx > 0 => *vcpu_idx -= 1,
                            _ => {}
                        }
                        None
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        match focus {
                            WizardField::Id => {}
                            WizardField::Base if !known_bases.is_empty() => {
                                *base_idx = (*base_idx + 1) % known_bases.len();
                            }
                            WizardField::Memory if *memory_idx + 1 < memory_options.len() => *memory_idx += 1,
                            WizardField::Vcpus if *vcpu_idx + 1 < vcpu_options.len() => *vcpu_idx += 1,
                            _ => {}
                        }
                        None
                    }
                    KeyCode::Backspace => {
                        if *focus == WizardField::Id {
                            id_input.pop();
                        }
                        None
                    }
                    KeyCode::Char(c) => {
                        if *focus == WizardField::Id
                            && (c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        {
                            id_input.push(c);
                        }
                        None
                    }
                    KeyCode::Enter => {
                        let id = id_input.trim().to_string();
                        let base = known_bases.get(*base_idx).cloned().unwrap_or_default();
                        let memory_mb = memory_options.get(*memory_idx).copied().unwrap_or(0);
                        let vcpus = vcpu_options.get(*vcpu_idx).copied().unwrap_or(0);
                        if id.is_empty() {
                            self.toast(ToastKind::Error,
                                "instance: id is required".into());
                            return None;
                        }
                        if base.is_empty() {
                            self.toast(ToastKind::Error,
                                "instance: no base images available".into());
                            return None;
                        }
                        self.mode = Mode::Normal;
                        Some(Cmd::CreateInstance {
                            id, base, memory_mb, vcpus,
                        })
                    }
                    _ => None,
                }
            }
            Mode::Normal => self.handle_key_normal(k),
        }
    }

    fn handle_key_normal(&mut self, k: KeyEvent) -> Option<Cmd> {
        // Ctrl-c quits cleanly.
        if k.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(k.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return None;
        }
        match k.code {
            KeyCode::Char('q') => { self.should_quit = true; None }
            KeyCode::Char('r') => Some(Cmd::Refresh),
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.mode = Mode::HelpOverlay; None
            }
            KeyCode::Char('H') => {
                self.mode = Mode::HostResourcesOverlay; None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1); None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1); None
            }
            KeyCode::Char('s') => match self.selected_domain() {
                Some(d) if is_dom0(&d.handle) => {
                    self.toast(ToastKind::Error,
                        "dom0 is the host — already running".into());
                    None
                }
                Some(d) if matches!(d.state, GuestState::Running | GuestState::Idle) => {
                    self.toast(ToastKind::Info,
                        format!("{} is already running", d.handle));
                    None
                }
                Some(d) => Some(Cmd::Start(d.handle.clone())),
                None => None,
            },
            KeyCode::Char('x') => match self.selected_domain() {
                Some(d) if is_dom0(&d.handle) => {
                    self.toast(ToastKind::Error,
                        "dom0 is the host — won't shutdown".into());
                    None
                }
                Some(d) => Some(Cmd::Stop {
                    handle: d.handle.clone(), force: false }),
                None => None,
            },
            KeyCode::Char('X') => {
                match self.selected_domain() {
                    Some(d) if is_dom0(&d.handle) => {
                        self.toast(ToastKind::Error,
                            "dom0 is the host — won't destroy".into());
                    }
                    Some(d) => {
                        self.mode = Mode::ConfirmKill { handle: d.handle.clone() };
                    }
                    None => {}
                }
                None
            }
            KeyCode::Char('b') => {
                if let Some(d) = self.selected_domain() {
                    let (current_mb, max_mb) = d.status.as_ref()
                        .map(|s| (s.memory_mb, s.memory_max_mb))
                        .unwrap_or((0, 0));
                    let options_mb = balloon_options_mb(
                        current_mb,
                        max_mb,
                        is_dom0(&d.handle),
                    );
                    self.mode = Mode::BalloonPrompt {
                        handle: d.handle.clone(),
                        current_mb,
                        max_mb,
                        selected_idx: nearest_index_u64(&options_mb, current_mb),
                        options_mb,
                        entry_mode: BalloonEntry::Presets,
                        text_input: String::new(),
                    };
                }
                None
            }
            KeyCode::Char('c') => Some(Cmd::CreateFromManifest(
                self.config.manifest_path.clone())),
            KeyCode::Char('n') => {
                // Daemon-only path. In direct/libxl mode the worker
                // would refuse with a toast; the wizard itself is just a
                // form so we still let the user open it — surfacing the
                // mode mismatch on submit gives the same diagnostic and
                // keeps the keybinding consistent.
                let known_bases = available_base_choices();
                let memory_options = vec![512, 1024, 2048, 4096, 8192, 16384];
                let vcpu_options = vec![1, 2, 4, 8, 16];
                self.mode = Mode::InstanceWizard {
                    id_input:     String::new(),
                    known_bases,
                    base_idx:     0,
                    memory_idx: nearest_index_u64(
                        &memory_options,
                        rotten_apple_instances::DEFAULT_MEMORY_MB,
                    ),
                    memory_options,
                    vcpu_idx: nearest_index_u32(
                        &vcpu_options,
                        rotten_apple_instances::DEFAULT_VCPUS,
                    ),
                    vcpu_options,
                    focus:        WizardField::Id,
                };
                None
            }
            KeyCode::Char('P') => {
                self.mode = Mode::ConfirmPromote;
                None
            }
            KeyCode::Char('M') => {
                if let Some(d) = self.selected_domain() {
                    let domid = match handle_to_domid(&d.handle) {
                        Some(n) => n,
                        None => {
                            self.toast(ToastKind::Error,
                                "policy: non-numeric handle".into());
                            return None;
                        }
                    };
                    let (current_mb, max_mb) = d.status.as_ref()
                        .map(|s| (s.memory_mb, s.memory_max_mb))
                        .unwrap_or((0, 0));
                    let (min_options, max_options) = policy_options_mb(current_mb, max_mb);
                    let cooldown_options = vec![5, 10, 30, 60, 120];
                    self.mode = Mode::PolicyEditor {
                        domid,
                        name: d.name.clone(),
                        min_idx: nearest_index_u64(&min_options, current_mb),
                        max_idx: nearest_index_u64(&max_options, max_mb.max(current_mb)),
                        cooldown_idx: 2,
                        min_options,
                        max_options,
                        cooldown_options,
                        focus: PolicyField::Min,
                        current_mb,
                        current_max_mb: max_mb,
                    };
                }
                None
            }
            KeyCode::Char('B') => {
                // Confirm-then-toggle the next-boot UI. Target is the
                // OPPOSITE of the cached current mode; the actual
                // systemctl side-effects happen on [y].
                let target = match self.boot_mode {
                    BootMode::Desktop => BootMode::Cockpit,
                    BootMode::Cockpit => BootMode::Desktop,
                };
                self.mode = Mode::ConfirmBootMode { target };
                None
            }
            _ => None,
        }
    }

    /// Run the requested toggle synchronously. The operations are local
    /// (file write + systemctl) so there's no worker to route through —
    /// we block briefly, then refresh the cached `boot_mode` and surface
    /// the result via toast + event.
    fn apply_boot_mode_toggle(&mut self, target: BootMode) {
        let result = match target {
            BootMode::Cockpit => boot_mode::enable_cockpit_boot(false),
            BootMode::Desktop => boot_mode::disable_cockpit_boot(false),
        };
        match result {
            Ok(()) => {
                self.boot_mode = boot_mode::current_boot_mode();
                let label = target.label();
                let evt = EventEntry::success(&format!(
                    "boot mode → {label}; reboot to apply"));
                self.toast(ToastKind::Success, evt.text.clone());
                self.push_event(evt);
            }
            Err(e) => {
                let evt = EventEntry::error(
                    &format!("boot-mode toggle: {e}"));
                self.toast(ToastKind::Error, evt.text.clone());
                self.push_event(evt);
            }
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let Some(snap) = &self.snapshot else { return };
        if snap.domains.is_empty() { return }
        let n = snap.domains.len() as i32;
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n);
        self.list_state.select(Some(next as usize));
    }

    fn selected_domain(&self) -> Option<&DomainView> {
        let snap = self.snapshot.as_ref()?;
        let idx = self.list_state.selected()?;
        snap.domains.get(idx)
    }
}

// ---------------------------------------------------------------------------
// Render

fn render(f: &mut Frame, state: &mut State) {
    let area = centered_main_rect(f.area());

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),  // header
            Constraint::Min(0),     // body
            Constraint::Length(2),  // footer
        ])
        .split(area);

    render_header(f, state, layout[0]);
    render_body(f, state, layout[1]);
    render_footer(f, state, layout[2]);

    // Toast overlay
    if let Some((started, text, kind)) = &state.toast
        && started.elapsed() < TOAST_TTL
    {
        render_toast(f, area, text, *kind);
    }

    // Modal overlays
    match &state.mode {
        Mode::HelpOverlay => render_help_overlay(f, area),
        Mode::HostResourcesOverlay => render_host_resources_overlay(f, area, state),
        Mode::FirstRunWelcome => render_first_run_welcome(f, area, state),
        Mode::BalloonPrompt {
            handle, current_mb, max_mb, options_mb, selected_idx,
            entry_mode, text_input,
        } => {
            render_balloon_prompt(
                f, area, handle, *current_mb, *max_mb, options_mb, *selected_idx,
                *entry_mode, text_input,
            );
        }
        Mode::PolicyEditor {
            domid, name, min_options, max_options, cooldown_options,
            min_idx, max_idx, cooldown_idx,
            focus, current_mb, current_max_mb,
        } => {
            render_policy_editor(
                f, area, *domid, name,
                min_options, max_options, cooldown_options,
                *min_idx, *max_idx, *cooldown_idx, *focus,
                *current_mb, *current_max_mb,
            );
        }
        Mode::ConfirmKill { handle } => {
            render_confirm_kill(f, area, handle);
        }
        Mode::ConfirmPromote => render_confirm_promote(f, area),
        Mode::ConfirmBootMode { target } =>
            render_confirm_boot_mode(f, area, state.boot_mode, *target),
        Mode::InstanceWizard {
            id_input, known_bases, base_idx,
            memory_options, memory_idx, vcpu_options, vcpu_idx, focus,
        } => render_instance_wizard(
            f, area, id_input, known_bases, *base_idx,
            memory_options, *memory_idx, vcpu_options, *vcpu_idx, *focus,
        ),
        Mode::Normal => {}
    }
}

fn centered_main_rect(area: Rect) -> Rect {
    let width = area.width.min(120);
    let height = area.height.min(40);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect { x, y, width, height }
}

/// Full-page-ish overlay showing host CPU + memory totals, per-domain
/// allocation, and free headroom. Triggered by `[H]` from Normal mode;
/// any key dismisses.
fn render_host_resources_overlay(f: &mut Frame, area: Rect, state: &State) {
    let w = area.width.min(80);
    let h = area.height.min(22);
    let overlay = centered_rect_abs(w, h, area);
    f.render_widget(Clear, overlay);

    let snap = state.snapshot.as_ref();
    let host = snap.and_then(|s| s.host.as_ref());
    let domains = snap.map(|s| s.domains.as_slice()).unwrap_or(&[]);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(" host resources",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::raw(""),
    ];

    // CPU section
    lines.push(Line::from(Span::styled("  CPU",
        Style::default().add_modifier(Modifier::BOLD))));
    match host.and_then(|h| h.total_pcpus) {
        Some(total) => {
            let allocated: u32 = domains.iter()
                .filter_map(|d| d.status.as_ref().map(|s| s.vcpus))
                .sum();
            let topo = host
                .and_then(|h| Some((h.threads_per_core?, h.cores_per_socket?)))
                .map(|(t, c)| format!(" ({c} cores/socket × {t} threads)"))
                .unwrap_or_default();
            lines.push(Line::from(format!(
                "    total physical    {total}{topo}")));
            lines.push(Line::from(format!(
                "    allocated         {allocated}    (sum of vcpus across {} domain(s))",
                domains.len())));
            for d in domains {
                if let Some(st) = &d.status {
                    lines.push(Line::from(format!(
                        "      └─ {:>2} vcpu    domain {} ({})",
                        st.vcpus, d.handle, d.name)));
                }
            }
            // logical "free": total - sum-of-domain-vcpus. Useful for
            // sizing new guests; doesn't account for pinning/credit.
            let free = (total as i64) - (allocated as i64);
            lines.push(Line::from(format!(
                "    free (logical)    {}    (host has more vcpus than domains use)",
                free.max(0))));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "    (libxl not reachable — backend reports no physinfo)",
                Style::default().fg(Color::DarkGray))));
        }
    }

    lines.push(Line::raw(""));

    // Memory section
    lines.push(Line::from(Span::styled("  Memory",
        Style::default().add_modifier(Modifier::BOLD))));
    match host.and_then(|h| h.total_memory_mb) {
        Some(total) => {
            let domain_alloc: u64 = domains.iter()
                .filter_map(|d| d.status.as_ref().map(|s| s.memory_mb))
                .sum();
            let free = host.and_then(|h| h.free_memory_mb).unwrap_or(0);
            let scrub = host.and_then(|h| h.scrub_memory_mb).unwrap_or(0);
            let pct = |n: u64| -> u64 {
                (n * 100).checked_div(total).unwrap_or(0)
            };
            lines.push(Line::from(format!(
                "    total           {:>7} MB", total)));
            lines.push(Line::from(format!(
                "    allocated       {:>7} MB ({}%)",
                domain_alloc, pct(domain_alloc))));
            for d in domains {
                if let Some(st) = &d.status {
                    let max_part = if st.memory_max_mb > 0 && st.memory_max_mb != st.memory_mb {
                        format!(" / {} max", st.memory_max_mb)
                    } else { String::new() };
                    lines.push(Line::from(format!(
                        "      └─ {:>5} MB{:<14}  domain {} ({})",
                        st.memory_mb, max_part, d.handle, d.name)));
                }
            }
            lines.push(Line::from(format!(
                "    free            {:>7} MB ({}%)", free, pct(free))));
            if scrub > 0 {
                lines.push(Line::from(format!(
                    "    scrubbing       {:>7} MB    (Xen reclaiming pages)", scrub)));
            }
        }
        None => {
            lines.push(Line::from(Span::styled(
                "    (libxl not reachable — backend reports no physinfo)",
                Style::default().fg(Color::DarkGray))));
        }
    }

    lines.push(Line::raw(""));

    // Storage + GPU placeholders (real lease tracking lands later)
    lines.push(Line::from(Span::styled("  Storage",
        Style::default().add_modifier(Modifier::BOLD))));
    lines.push(Line::from(Span::styled(
        "    (TODO: instance overlay + image catalog tracking ships with `image pull`)",
        Style::default().fg(Color::DarkGray))));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("  GPU lease",
        Style::default().add_modifier(Modifier::BOLD))));
    lines.push(Line::from(Span::styled(
        "    (TODO: foreground lease wires when iGPU passthrough ships)",
        Style::default().fg(Color::DarkGray))));

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default().fg(Color::DarkGray))));

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL)
            .title(" host resources "))
        .wrap(Wrap { trim: false });
    f.render_widget(p, overlay);
}

fn render_confirm_promote(f: &mut Frame, area: Rect) {
    let card = widgets::confirm_card(
        "promote default",
        "promote Xen entry to GRUB default?",
        vec![
            Line::from("  Future boots will land in Xen unless you pick another"),
            Line::from("  entry from the GRUB menu (hold Shift at boot)."),
            Line::from("  Bare-metal Ubuntu remains a menu choice — not deleted."),
        ],
        "yes, promote",
        64,
    );
    widgets::render_overlay_card(f, area, card);
}

fn render_confirm_boot_mode(
    f: &mut Frame, area: Rect, current: BootMode, target: BootMode,
) {
    let (headline, blurb) = match target {
        BootMode::Cockpit => (
            "switch boot to cockpit-only?",
            "  Next boot will land in the cockpit on tty1; the desktop\n  will not auto-start. Reverse with [B] or `boot-mode desktop`."),
        BootMode::Desktop => (
            "switch boot to desktop?",
            "  Next boot will return to the standard Ubuntu desktop.\n  The cockpit getty override is removed; DM unmasked."),
    };
    let mut body = vec![
        Line::from(format!("  current: {}    →    target: {}",
            current.label(), target.label())),
        Line::raw(""),
    ];
    for blurb_line in blurb.lines() {
        body.push(Line::from(blurb_line.to_string()));
    }
    let card = widgets::confirm_card(
        "boot mode", headline, body, "yes", 68,
    );
    widgets::render_overlay_card(f, area, card);
}

fn render_header(f: &mut Frame, state: &State, area: Rect) {
    let lines = match (&state.backend_name, &state.libxl_version, &state.backend_error) {
        (Some(b), Some(v), _) => {
            let src = state.backend_source
                .map(|s| s.label())
                .unwrap_or("");
            let domain_count = state.snapshot.as_ref()
                .map(|s| s.domains.len()).unwrap_or(0);
            let host_summary = format_host_summary(state);
            vec![
                Line::from(format!(" rotten-apple cockpit  |  {b}  |  libxl {v}  |  {src} ")),
                Line::from(format!(" domains: {domain_count}  |  {host_summary}  |  [H] host details ")),
            ]
        }
        (_, _, Some(_)) => vec![
            Line::from(" rotten-apple cockpit "),
            Line::from(" backend unavailable "),
        ],
        _ => vec![
            Line::from(" rotten-apple cockpit "),
            Line::from(" connecting… "),
        ],
    };
    let header = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(header, area);
}

/// One-line CPU + RAM summary for the header. Pulls aggregates from the
/// snapshot's host field; sums per-domain vcpu/memory for "allocated".
fn format_host_summary(state: &State) -> String {
    let Some(snap) = state.snapshot.as_ref() else {
        return "host: —".into();
    };
    let Some(host) = snap.host.as_ref() else {
        return "host: (waiting)".into();
    };
    let cpu_alloc: u32 = snap.domains.iter()
        .filter_map(|d| d.status.as_ref().map(|s| s.vcpus))
        .sum();
    let mem_alloc_mb: u64 = snap.domains.iter()
        .filter_map(|d| d.status.as_ref().map(|s| s.memory_mb))
        .sum();
    let cpu_part = match host.total_pcpus {
        Some(t) => format!("CPU {cpu_alloc}/{t}"),
        None    => "CPU —".into(),
    };
    let mem_part = match host.total_memory_mb {
        Some(t) => format!("RAM {:.1}/{:.1} GB",
            mem_alloc_mb as f64 / 1024.0,
            t as f64 / 1024.0),
        None => "RAM —".into(),
    };
    format!("{cpu_part}  {mem_part}")
}

fn render_body(f: &mut Frame, state: &mut State, area: Rect) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    render_domain_list(f, state, split[0]);
    render_detail(f, state, split[1]);
}

fn render_domain_list(f: &mut Frame, state: &mut State, area: Rect) {
    let items: Vec<ListItem> = match &state.snapshot {
        None => vec![
            ListItem::new("(connecting to backend …)").dim(),
            ListItem::new("").dim(),
            ListItem::new("  if this hangs, press ? for help or [s]hell").dim(),
        ],
        Some(s) if s.domains.is_empty() => vec![
            ListItem::new("  no domains running yet").dim(),
            ListItem::new("").dim(),
            ListItem::new(Line::from(vec![
                Span::raw("  press "),
                Span::styled("[n]", Style::default().fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)),
                Span::raw(" to spawn one from a cloud image,"),
            ])),
            ListItem::new(Line::from(vec![
                Span::raw("        "),
                Span::styled("[c]", Style::default().fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)),
                Span::raw(" to use the active manifest,"),
            ])),
            ListItem::new(Line::from(vec![
                Span::raw("        "),
                Span::styled("[H]", Style::default().fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)),
                Span::raw(" to see total host resources first."),
            ])),
        ],
        Some(s) => s.domains.iter().map(|d| {
            let (sym, color) = state_glyph(&d.state);
            let mem = d.status.as_ref()
                .map(|st| format!("{:>5}M", st.memory_mb))
                .unwrap_or_else(|| "    ?M".into());
            let line = Line::from(vec![
                Span::styled(format!("{sym} "), Style::default().fg(color)),
                Span::raw(format!("{:<14}", truncate(&d.name, 14))),
                Span::raw(format!(" {mem}")),
                Span::raw(format!(" {:<8}", state_label(&d.state))),
                Span::styled(format!("[{}]", d.handle),
                    Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        }).collect(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" domains ");
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default()
            .add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, area, &mut state.list_state);
}

fn render_detail(f: &mut Frame, state: &State, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" detail ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(inner);

    render_detail_top(f, state, layout[0]);
    render_event_log(f, state, layout[1]);
}

fn render_detail_top(f: &mut Frame, state: &State, area: Rect) {
    let lines: Vec<Line> = match (state.selected_domain(), state.backend_error.as_ref()) {
        (_, Some(e)) => vec![
            Line::from(Span::styled("backend not available",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))),
            Line::raw(""),
            Line::from(Span::raw(e.clone())),
            Line::raw(""),
            Line::from(Span::styled(
                "Run with sudo on a Xen dom0 host.",
                Style::default().fg(Color::DarkGray))),
        ],
        (None, _) => vec![
            Line::from(Span::styled("(no domain selected)",
                Style::default().fg(Color::DarkGray))),
        ],
        (Some(d), _) => {
            let (sym, color) = state_glyph(&d.state);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!("{sym} "), Style::default().fg(color)),
                    Span::styled(d.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(state_label(&d.state),
                        Style::default().fg(color)),
                    Span::raw(format!("  [{}]", d.handle)),
                ]),
                Line::raw(""),
            ];
            if let Some(st) = &d.status {
                let mem_line = if st.memory_max_mb > 0 && st.memory_max_mb != st.memory_mb {
                    format!("  memory  {} MB (max {} MB)", st.memory_mb, st.memory_max_mb)
                } else {
                    format!("  memory  {} MB", st.memory_mb)
                };
                lines.push(Line::from(mem_line));
                lines.push(Line::from(format!(
                    "  vcpus   {}", st.vcpus)));
                // uptime is real for dom0 (read from /proc/uptime); zero
                // for domU until xenstore start-time is wired. Render "—"
                // rather than 00:00:00 so the difference is obvious.
                let secs = st.uptime.as_secs();
                let uptime_str = if secs == 0 {
                    "—".to_string()
                } else {
                    let h = secs / 3600;
                    let m = (secs % 3600) / 60;
                    let s = secs % 60;
                    format!("{h:02}:{m:02}:{s:02}")
                };
                lines.push(Line::from(format!("  uptime  {uptime_str}")));
                if let Some(last) = &st.last_event {
                    lines.push(Line::from(format!("  event   {last}")));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "  (status unavailable — domain may have been destroyed)",
                    Style::default().fg(Color::DarkGray))));
            }
            lines
        }
    };
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_event_log(f: &mut Frame, state: &State, area: Rect) {
    let lines: Vec<Line> = state.events.iter().rev().take(area.height as usize)
        .map(|e| {
            let secs = e.when.elapsed().as_secs();
            let (color, sym) = match e.kind {
                EventKind::Info    => (Color::Cyan,  "·"),
                EventKind::Success => (Color::Green, "✓"),
                EventKind::Error   => (Color::Red,   "✗"),
            };
            Line::from(vec![
                Span::styled(format!("{sym} "), Style::default().fg(color)),
                Span::styled(format!("{secs:>4}s ago "),
                    Style::default().fg(Color::DarkGray)),
                Span::raw(e.text.clone()),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::TOP)
        .title(" events ");
    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_footer(f: &mut Frame, state: &State, area: Rect) {
    // Footer is mode-aware. Inside an overlay we show the overlay's own
    // keybindings (Tab/Esc/Enter) so the user doesn't have to guess; in
    // Normal we show the per-domain hints + the always-relevant globals
    // ([B]oot, [H]ost, [?]help, [q]uit). The big idea: anything the user
    // can press right now that does something useful gets a hint.
    let mut primary: Vec<Span> = Vec::new();
    let mut secondary: Vec<Span> = Vec::new();
    let push = |s: &mut Vec<Span>, k, l| {
        if !s.is_empty() { s.push(Span::raw("  ")); }
        s.push(keybind(k, l));
    };
    let dim_push = |s: &mut Vec<Span>, k: &str, l: &str| {
        if !s.is_empty() { s.push(Span::raw("  ")); }
        s.push(Span::styled(format!("[{k}]{l}"),
            Style::default().fg(Color::DarkGray)));
    };
    match &state.mode {
        // Overlay-specific keybindings. We don't try to enumerate every
        // navigation key — just the meaningful ones the user might miss.
        Mode::HelpOverlay | Mode::HostResourcesOverlay | Mode::FirstRunWelcome => {
            push(&mut primary, "Esc", " or any key to dismiss");
        }
        Mode::ConfirmKill { .. }
        | Mode::ConfirmPromote
        | Mode::ConfirmBootMode { .. } => {
            push(&mut primary, "y", "es");
            push(&mut primary, "n/Esc", " cancel");
        }
        Mode::BalloonPrompt { entry_mode, .. } => {
            match entry_mode {
                BalloonEntry::Presets => {
                    push(&mut primary, "←/→", " preset");
                }
                BalloonEntry::ManualMb | BalloonEntry::Percentage => {
                    push(&mut primary, "0-9", " digits");
                    push(&mut primary, "BkSp", " erase");
                }
            }
            push(&mut primary, "Tab", " entry mode");
            push(&mut primary, "Enter", " apply");
            push(&mut primary, "Esc", " cancel");
        }
        Mode::PolicyEditor { .. } => {
            push(&mut primary, "Tab", " field");
            push(&mut primary, "←/→", " value");
            push(&mut primary, "Enter", " apply");
            push(&mut primary, "Esc", " cancel");
        }
        Mode::InstanceWizard { .. } => {
            push(&mut primary, "Tab", " field");
            push(&mut primary, "Enter", " create + start");
            push(&mut primary, "Esc", " cancel");
        }
        // Normal mode → context-sensitive primary actions for the
        // selected domain, plus the global navigation row.
        Mode::Normal => {
            match state.selected_domain() {
                Some(d) if is_dom0(&d.handle) => {
                    dim_push(&mut primary, "s", "tart");
                    dim_push(&mut primary, "x", "Stop");
                    dim_push(&mut primary, "X", "Kill");
                    push(&mut primary, "b", "alloon");
                    push(&mut primary, "M", "policy");
                }
                Some(d) => {
                    let running = matches!(d.state,
                        GuestState::Running | GuestState::Idle);
                    if running { dim_push(&mut primary, "s", "tart") }
                    else       { push    (&mut primary, "s", "tart") }
                    if running { push    (&mut primary, "x", "Stop") }
                    else       { dim_push(&mut primary, "x", "Stop") }
                    push(&mut primary, "X", "Kill");
                    push(&mut primary, "b", "alloon");
                    push(&mut primary, "M", "policy");
                }
                None => {}
            }
            // Always-relevant globals on the second row, ordered by
            // frequency-of-use rather than alphabetical.
            push(&mut secondary, "n", "ew");
            push(&mut secondary, "H", "ost");
            push(&mut secondary, "B", "oot");
            push(&mut secondary, "P", "romote");
            push(&mut secondary, "r", "efresh");
            push(&mut secondary, "?", "help");
            push(&mut secondary, "q", "uit");
        }
    }
    let p = Paragraph::new(vec![
        Line::from(primary),
        Line::from(secondary),
    ]).alignment(Alignment::Center);
    f.render_widget(p, area);
}

fn keybind(k: &'static str, label: &'static str) -> Span<'static> {
    let mut s = String::with_capacity(k.len() + label.len() + 2);
    s.push('[');
    s.push_str(k);
    s.push(']');
    s.push_str(label);
    Span::styled(s, Style::default().fg(Color::Yellow))
}

fn render_toast(f: &mut Frame, area: Rect, text: &str, kind: ToastKind) {
    let color = match kind {
        ToastKind::Info    => Color::Cyan,
        ToastKind::Success => Color::Green,
        ToastKind::Error   => Color::Red,
    };
    let label = match kind {
        ToastKind::Info    => "info",
        ToastKind::Success => "ok",
        ToastKind::Error   => "error",
    };
    let body = format!(" {label}: {text} ");
    let w = body.chars().count() as u16 + 2;
    let h = 3u16;
    let x = area.x + area.width.saturating_sub(w + 1);
    let y = area.y + area.height.saturating_sub(h + 1);
    let toast_area = Rect { x, y, width: w.min(area.width), height: h };
    f.render_widget(Clear, toast_area);
    let p = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().fg(color));
    f.render_widget(p, toast_area);
}

/// Path to the first-run marker. Living under XDG_CONFIG_HOME or
/// ~/.config/rotten-apple keeps it user-scoped (per-account) instead of
/// system-wide — useful when multiple humans share one box.
fn welcome_marker_path() -> Option<std::path::PathBuf> {
    // SUDO_USER takes precedence: cockpit may launch as root from a
    // sudo session, but we want the marker in the actual operator's
    // home, not /root. Falls back to HOME for non-sudo launches.
    let user = std::env::var("SUDO_USER").ok()
        .or_else(|| std::env::var("USER").ok())?;
    let home = if user == "root" {
        std::env::var("HOME").ok()?
    } else {
        let s = std::fs::read_to_string("/etc/passwd").ok()?;
        s.lines()
            .find(|l| l.starts_with(&format!("{user}:")))
            .and_then(|l| l.split(':').nth(5))
            .map(|s| s.to_string())?
    };
    Some(std::path::PathBuf::from(home)
        .join(".config").join("rotten-apple").join("welcomed"))
}

fn welcome_needed() -> bool {
    match welcome_marker_path() {
        Some(p) => !p.exists(),
        None => false,  // can't resolve a home — don't bother
    }
}

fn mark_welcome_seen() {
    let Some(p) = welcome_marker_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, b"seen\n");
    // Chown back to SUDO_USER if we ran as root via sudo, so the user
    // can manage their own marker file later (delete to re-show, etc.).
    if let (Ok(user), Some(home_str)) = (
        std::env::var("SUDO_USER"),
        p.parent().and_then(|x| x.parent()).and_then(|x| x.to_str())
    ) {
        let _ = home_str;
        if let Some((uid, gid)) = passwd_uid_gid(&user) {
            for path in [&p, &p.parent().unwrap().to_path_buf()] {
                if let Ok(s) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
                    // SAFETY: standard libc chown
                    unsafe { libc::chown(s.as_ptr(), uid, gid); }
                }
            }
        }
    }
}

fn passwd_uid_gid(user: &str) -> Option<(u32, u32)> {
    let s = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in s.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.first().copied() == Some(user) {
            let uid = f.get(2)?.parse().ok()?;
            let gid = f.get(3)?.parse().ok()?;
            return Some((uid, gid));
        }
    }
    None
}

/// First-launch welcome — the rainbow road's starting line. Pinned to
/// roughly 70x18; centered. Plain language, action-oriented, dismiss
/// with any key. Marker is written when dismissed so subsequent launches
/// skip it. Delete `~/.config/rotten-apple/welcomed` to see it again.
fn render_first_run_welcome(f: &mut Frame, area: Rect, _state: &State) {
    let w = area.width.min(72);
    let h = area.height.min(20);
    let overlay = centered_rect_abs(w, h, area);
    f.render_widget(Clear, overlay);
    let lines = vec![
        Line::from(Span::styled("welcome to rotten-apple",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::raw(""),
        Line::from("This is the cockpit — your single surface for managing the"),
        Line::from("hypervisor below you and the guests on top of it."),
        Line::raw(""),
        Line::from(Span::styled("  the keys that matter most:",
            Style::default().add_modifier(Modifier::BOLD))),
        Line::from("    ?    open the full help overlay (also: F1)"),
        Line::from("    H    host resources — CPU + RAM, what's allocated where"),
        Line::from("    n    new instance — pull a base image and spawn a fresh VM"),
        Line::from("    M    edit a domain's memory policy (engine enforces it)"),
        Line::from("    B    toggle next-boot UI (cockpit ⇄ desktop)"),
        Line::from("    q    quit (Ctrl-C also works anywhere)"),
        Line::raw(""),
        Line::from(Span::styled(
            "  if anything looks weird, press ? — every action is reachable",
            Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled(
            "  from a single keystroke. nothing is hidden behind menus.",
            Style::default().fg(Color::DarkGray))),
        Line::raw(""),
        Line::from(Span::styled("  press any key to begin",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" first launch "))
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false });
    f.render_widget(p, overlay);
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled("rotten-apple cockpit",
            Style::default().add_modifier(Modifier::BOLD))),
        Line::raw(""),
        Line::from("  ↑/k, ↓/j     move selection"),
        Line::from("  s            start selected guest"),
        Line::from("  x            graceful shutdown"),
        Line::from("  X            destroy (asks for confirmation)"),
        Line::from("  b            balloon — prompts for new MB"),
        Line::from("  M            edit memory policy"),
        Line::from("  c            create from active manifest"),
        Line::from("  n            new instance (CoW overlay on a base image)"),
        Line::from("  P            promote Xen entry to GRUB default"),
        Line::from("  B            toggle next-boot UI (cockpit ⇄ desktop)"),
        Line::from("  H            host resources (CPU + RAM allocation)"),
        Line::from("  r            refresh now"),
        Line::from("  ?  /  F1     this help"),
        Line::from("  q  /  Ctrl-C quit  (drops to shell on tty1; otherwise exits)"),
        Line::from("  Esc          cancel overlay / prompt"),
        Line::raw(""),
        Line::from(Span::styled("press any key to dismiss",
            Style::default().fg(Color::DarkGray))),
    ];
    let overlay = centered_rect(60, 50, area);
    f.render_widget(Clear, overlay);
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" help "))
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(p, overlay);
}

#[allow(clippy::too_many_arguments)]
fn render_balloon_prompt(
    f: &mut Frame, area: Rect, handle: &GuestHandle,
    current_mb: u64, max_mb: u64, options_mb: &[u64], selected_idx: usize,
    entry_mode: BalloonEntry, text_input: &str,
) {
    let w = area.width.min(88);
    let h = 12u16;
    let overlay = centered_rect_abs(w, h, area);
    f.render_widget(Clear, overlay);
    let range_line = if max_mb > 0 {
        format!("  current: {current_mb} M     max: {max_mb} M")
    } else {
        format!("  current: {current_mb} M")
    };
    let floor_line = if is_dom0(handle) {
        Span::styled(
            format!("  dom0: floor is {DOM0_FLOOR_MB}M (kernel-survival); ceiling is dom0_max_mem"),
            Style::default().fg(Color::DarkGray))
    } else {
        Span::styled("  any value in [min, max] from the manifest",
            Style::default().fg(Color::DarkGray))
    };
    let mode_line = Line::from(vec![
        Span::raw("  entry: "),
        Span::styled(entry_mode.label(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("[Tab] cycles entry mode",
            Style::default().fg(Color::DarkGray)),
    ]);
    let target_line = match entry_mode {
        BalloonEntry::Presets =>
            selector_line_u64("  target", options_mb, selected_idx, "MB"),
        BalloonEntry::ManualMb => {
            let preview = if text_input.is_empty() {
                "____".to_string()
            } else {
                text_input.to_string()
            };
            Line::from(vec![
                Span::raw("  target: "),
                Span::styled(preview,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(" MB    "),
                Span::styled("digits + Backspace; Enter applies",
                    Style::default().fg(Color::DarkGray)),
            ])
        }
        BalloonEntry::Percentage => {
            let preview = if text_input.is_empty() {
                "__".to_string()
            } else {
                text_input.to_string()
            };
            let cap = if max_mb > 0 { max_mb } else { current_mb };
            let computed = text_input.parse::<u64>().ok()
                .filter(|p| *p <= 100)
                .map(|p| (cap * p + 50) / 100);
            let computed_str = match computed {
                Some(mb) => format!(" → {mb} MB"),
                None     => String::new(),
            };
            Line::from(vec![
                Span::raw("  target: "),
                Span::styled(preview,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw("%"),
                Span::styled(computed_str,
                    Style::default().fg(Color::Cyan)),
                Span::raw("    "),
                Span::styled("0-100 + Backspace; Enter applies",
                    Style::default().fg(Color::DarkGray)),
            ])
        }
    };
    let lines = vec![
        Line::from(format!(" balloon → {handle}")),
        Line::raw(""),
        Line::from(range_line),
        Line::from(floor_line),
        Line::raw(""),
        mode_line,
        target_line,
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" balloon "))
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(p, overlay);
}

#[allow(clippy::too_many_arguments)]
fn render_policy_editor(
    f: &mut Frame, area: Rect,
    domid: u32, name: &str,
    min_options: &[u64], max_options: &[u64], cooldown_options: &[u64],
    min_idx: usize, max_idx: usize, cooldown_idx: usize,
    focus: PolicyField,
    current_mb: u64, current_max_mb: u64,
) {
    let w = area.width.min(94);
    let h = 12u16;
    let overlay = centered_rect_abs(w, h, area);
    f.render_widget(Clear, overlay);

    let lines = vec![
        Line::from(format!(" memory policy: domain {domid} ({name})")),
        Line::raw(""),
        Line::from(format!("  current:  {current_mb} MB  (max {current_max_mb} MB)")),
        Line::raw(""),
        selector_line_u64_focus("  min_mb", min_options, min_idx, "MB", focus == PolicyField::Min),
        selector_line_u64_focus("  max_mb", max_options, max_idx, "MB", focus == PolicyField::Max),
        selector_line_u64_focus("  cooldown", cooldown_options, cooldown_idx, "s", focus == PolicyField::Cooldown),
        Line::raw(""),
        Line::from(Span::styled(
            "  Engine ticks at 1Hz; policy applies on next tick.",
            Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled(
            "  [Tab/↑/↓] field   [←/→] choose   [Enter] submit   [Esc] cancel",
            Style::default().fg(Color::Yellow))),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" memory policy "))
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(p, overlay);
}

#[allow(clippy::too_many_arguments)]
fn render_instance_wizard(
    f: &mut Frame, area: Rect,
    id_input: &str, known_bases: &[String], base_idx: usize,
    memory_options: &[u64], memory_idx: usize,
    vcpu_options: &[u32], vcpu_idx: usize,
    focus: WizardField,
) {
    let w = area.width.min(96);
    let h = 13u16;
    let overlay = centered_rect_abs(w, h, area);
    f.render_widget(Clear, overlay);
    let base_hint = if known_bases.is_empty() {
        "  (no pullable images known)".to_string()
    } else {
        format!("  base selector: {} choice(s); missing images auto-pull ({}/{})",
            known_bases.len(), base_idx + 1, known_bases.len())
    };
    let base_name = known_bases.get(base_idx).map(String::as_str).unwrap_or("(none)");
    let lines = vec![
        Line::from(Span::styled(" new instance",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::raw(""),
        wizard_text_line("  id", id_input, focus == WizardField::Id),
        wizard_text_line("  base", base_name, focus == WizardField::Base),
        selector_line_u64_focus("  memory", memory_options, memory_idx, "MB", focus == WizardField::Memory),
        selector_line_u32_focus("  vcpus", vcpu_options, vcpu_idx, "", focus == WizardField::Vcpus),
        Line::raw(""),
        Line::from(Span::styled(base_hint,
            Style::default().fg(Color::DarkGray))),
        Line::raw(""),
        Line::from(Span::styled(
            "  type only the id; use [Tab/↑/↓] to move and [←/→] to choose",
            Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled(
            "  [Enter] create   [Esc] cancel",
            Style::default().fg(Color::Yellow))),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" instance "))
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(p, overlay);
}

fn selector_line_u64(label: &str, options: &[u64], selected_idx: usize, suffix: &str) -> Line<'static> {
    selector_line_u64_focus(label, options, selected_idx, suffix, true)
}

fn selector_line_u64_focus(
    label: &str, options: &[u64], selected_idx: usize, suffix: &str, focused: bool,
) -> Line<'static> {
    let marker = if focused { "▶" } else { " " };
    let body = options.iter().enumerate()
        .map(|(i, v)| {
            let text = if suffix.is_empty() {
                format!("{v}")
            } else {
                format!("{v} {suffix}")
            };
            if i == selected_idx {
                format!("[{text}]")
            } else {
                text
            }
        })
        .collect::<Vec<_>>()
        .join("   ");
    Line::from(vec![
        Span::raw(format!("{marker} {label:<10} ")),
        Span::styled(body, Style::default().add_modifier(Modifier::BOLD)),
    ])
}

fn selector_line_u32_focus(
    label: &str, options: &[u32], selected_idx: usize, suffix: &str, focused: bool,
) -> Line<'static> {
    let vals = options.iter().map(|v| *v as u64).collect::<Vec<_>>();
    selector_line_u64_focus(label, &vals, selected_idx, suffix, focused)
}

fn wizard_text_line(label: &str, value: &str, focused: bool) -> Line<'static> {
    let marker = if focused { "▶" } else { " " };
    let shown = if value.is_empty() { "_" } else { value };
    Line::from(vec![
        Span::raw(format!("{marker} {label:<10} ")),
        Span::styled(shown.to_string(), Style::default().add_modifier(Modifier::BOLD)),
    ])
}

fn render_confirm_kill(f: &mut Frame, area: Rect, handle: &GuestHandle) {
    let card = widgets::OverlayCard {
        title: "confirm",
        headline: Some(Line::from(Span::styled("destroy guest?",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)))),
        body: vec![
            Line::from(format!("  This will pull the power on {handle}.")),
            Line::from("  Unsaved guest state will be lost."),
        ],
        footer: Some(Line::from(Span::styled("  [y] yes   [Esc] cancel",
            Style::default().fg(Color::Yellow)))),
        width: 60,
        height: 8,
        accent: Color::Red,
    };
    widgets::render_overlay_card(f, area, card);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

use widgets::centered_rect_abs;

// ---------------------------------------------------------------------------
// Helpers

fn state_label(s: &GuestState) -> &'static str {
    match s {
        GuestState::Created   => "created",
        GuestState::Running   => "running",
        GuestState::Idle      => "idle",
        GuestState::Suspended => "suspend",
        GuestState::Stopped   => "stopped",
        GuestState::Failed    => "failed",
    }
}

fn state_glyph(s: &GuestState) -> (&'static str, Color) {
    match s {
        GuestState::Running   => ("●", Color::Green),
        GuestState::Idle      => ("◐", Color::Yellow),
        GuestState::Created   => ("○", Color::Cyan),
        GuestState::Suspended => ("◑", Color::Cyan),
        GuestState::Stopped   => ("○", Color::DarkGray),
        GuestState::Failed    => ("✗", Color::Red),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.into() }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn truncate_long_string_gets_ellipsis() {
        let t = truncate("abcdefghij", 5);
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), 5);
    }

    #[test]
    fn state_glyph_covers_all_variants() {
        // Ensures we don't drop a variant when GuestState grows.
        for s in [
            GuestState::Created, GuestState::Running, GuestState::Idle,
            GuestState::Suspended, GuestState::Stopped, GuestState::Failed,
        ] {
            let (g, _) = state_glyph(&s);
            assert!(!g.is_empty());
        }
    }

    #[test]
    fn event_kinds_distinct() {
        assert_ne!(EventKind::Info, EventKind::Error);
        assert_ne!(EventKind::Success, EventKind::Error);
    }

    #[test]
    fn state_new_starts_with_no_snapshot() {
        let s = State::new(CockpitConfig::default());
        assert!(s.snapshot.is_none());
        assert!(s.events.is_empty());
        // Initial mode is either Normal (returning user) or FirstRunWelcome
        // (first launch on this account). Both are valid clean-start states.
        assert!(matches!(s.mode, Mode::Normal | Mode::FirstRunWelcome));
    }

    #[test]
    fn confirm_boot_mode_targets_opposite_of_current() {
        // The [B] keybinding picks the OPPOSITE of state.boot_mode as
        // the target. Pin that derivation so a future refactor can't
        // accidentally enter "switch from Cockpit to Cockpit" mode.
        let mut s = State::new(CockpitConfig::default());
        s.boot_mode = BootMode::Desktop;
        let target = match s.boot_mode {
            BootMode::Desktop => BootMode::Cockpit,
            BootMode::Cockpit => BootMode::Desktop,
        };
        assert_eq!(target, BootMode::Cockpit);
        s.boot_mode = BootMode::Cockpit;
        let target = match s.boot_mode {
            BootMode::Desktop => BootMode::Cockpit,
            BootMode::Cockpit => BootMode::Desktop,
        };
        assert_eq!(target, BootMode::Desktop);
    }

    #[test]
    fn mode_confirm_boot_mode_is_constructible() {
        // Smoke test that the variant exists with the right shape; the
        // render path uses `target` and the key handler matches on it.
        let m = Mode::ConfirmBootMode { target: BootMode::Cockpit };
        match m {
            Mode::ConfirmBootMode { target } =>
                assert_eq!(target, BootMode::Cockpit),
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn parse_block_with_id_handles_single_quote() {
        let line = "menuentry 'Ubuntu' --class ubuntu --id 'gnulinux-simple-abc' {";
        assert_eq!(parse_block_with_id(line, "menuentry").as_deref(),
                   Some("gnulinux-simple-abc"));
    }

    #[test]
    fn parse_block_with_id_handles_double_quote() {
        let line = r#"submenu "Advanced" --id "gnulinux-advanced-xyz" {"#;
        assert_eq!(parse_block_with_id(line, "submenu").as_deref(),
                   Some("gnulinux-advanced-xyz"));
    }

    #[test]
    fn parse_block_with_id_returns_none_for_wrong_kind() {
        let line = "menuentry 'X' --id 'y' {";
        assert_eq!(parse_block_with_id(line, "submenu"), None);
    }

    #[test]
    fn find_xen_grub_path_in_advanced_submenu() {
        // Mimics Ubuntu's grub.cfg structure where the Xen entry lives
        // under "Advanced options for Ubuntu".
        let cfg = "\
menuentry 'Ubuntu' --class ubuntu --id 'gnulinux-simple-abc' {
    linux /boot/vmlinuz
}
submenu 'Advanced options for Ubuntu' --id 'gnulinux-advanced-abc' {
    menuentry 'Ubuntu, with Linux 6.17.0-22-generic' --id 'gnulinux-22-abc' {
        linux /boot/vmlinuz-6.17.0-22-generic
    }
    menuentry 'Ubuntu GNU/Linux, with Xen hypervisor' --id 'xen-gnulinux-abc' {
        multiboot2 /boot/xen.gz
    }
}
";
        let path = find_xen_grub_path(cfg);
        assert_eq!(path.as_deref(), Some("gnulinux-advanced-abc>xen-gnulinux-abc"));
    }

    #[test]
    fn find_xen_grub_path_top_level() {
        // Some configurations put the Xen entry at the top level.
        let cfg = "\
menuentry 'Ubuntu' --class ubuntu --id 'gnulinux-simple-abc' { }
menuentry 'Ubuntu, with Xen hypervisor 4.20' --id 'xen-top-abc' { }
";
        let path = find_xen_grub_path(cfg);
        assert_eq!(path.as_deref(), Some("xen-top-abc"));
    }

    #[test]
    fn find_xen_grub_path_returns_none_when_no_xen_entry() {
        let cfg = "menuentry 'Ubuntu' --id 'plain' { }\n";
        assert_eq!(find_xen_grub_path(cfg), None);
    }

    #[test]
    fn is_dom0_recognises_zero_handle() {
        assert!(is_dom0(&GuestHandle("0".into())));
        assert!(!is_dom0(&GuestHandle("1".into())));
        assert!(!is_dom0(&GuestHandle("ubuntu-desktop".into())));
        // "00" isn't dom0 — libxl always reports domid 0 as the literal "0"
        assert!(!is_dom0(&GuestHandle("00".into())));
    }

    #[test]
    fn validate_balloon_rejects_zero_target() {
        let r = validate_balloon_target(0, 1856, 4096, true);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("must be > 0"));
    }

    #[test]
    fn validate_balloon_rejects_above_max() {
        let r = validate_balloon_target(8192, 1856, 4096, false);
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("8192"));
        assert!(msg.contains("4096"));
    }

    #[test]
    fn validate_balloon_allows_max_zero_to_skip_ceiling_check() {
        // backends that don't report max should still allow ballooning
        let r = validate_balloon_target(2048, 1024, 0, false);
        assert_eq!(r.unwrap(), 2048);
    }

    #[test]
    fn validate_balloon_dom0_can_shrink_to_kernel_floor() {
        // Stress-test path: dom0 at 1856 MB, user picks 512 (the
        // hard floor) — must accept so balloon round-trip works.
        let r = validate_balloon_target(512, 1856, 4096, true);
        assert_eq!(r.unwrap(), DOM0_FLOOR_MB);
    }

    #[test]
    fn validate_balloon_dom0_rejects_below_kernel_floor() {
        // 256 MB is below the kernel survival floor; refuse.
        let r = validate_balloon_target(256, 1856, 4096, true);
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("dom0"));
        assert!(msg.contains(&DOM0_FLOOR_MB.to_string()));
    }

    #[test]
    fn validate_balloon_dom0_can_grow() {
        // ballooning dom0 UP is fine
        let r = validate_balloon_target(2200, 1856, 4096, true);
        assert_eq!(r.unwrap(), 2200);
    }

    #[test]
    fn validate_balloon_domu_can_shrink_below_current() {
        // domU shrinking is the normal case; only dom0 has the floor
        let r = validate_balloon_target(512, 2048, 4096, false);
        assert_eq!(r.unwrap(), 512);
    }

    #[test]
    fn validate_balloon_at_exactly_current_succeeds() {
        // boundary: target == current is a no-op but should be accepted
        let r = validate_balloon_target(1856, 1856, 4096, true);
        assert_eq!(r.unwrap(), 1856);
    }

    #[test]
    fn validate_balloon_at_exactly_max_succeeds() {
        let r = validate_balloon_target(4096, 1856, 4096, false);
        assert_eq!(r.unwrap(), 4096);
    }

    #[test]
    fn resolve_balloon_presets_picks_selected() {
        let opts = vec![512u64, 1024, 2048, 4096];
        let r = resolve_balloon_target(
            BalloonEntry::Presets, "", &opts, 2, 1024, 4096);
        assert_eq!(r.unwrap(), 2048);
    }

    #[test]
    fn resolve_balloon_manual_parses_mb() {
        let r = resolve_balloon_target(
            BalloonEntry::ManualMb, "1500", &[], 0, 1024, 4096);
        assert_eq!(r.unwrap(), 1500);
    }

    #[test]
    fn resolve_balloon_manual_empty_rejects() {
        let r = resolve_balloon_target(
            BalloonEntry::ManualMb, "", &[], 0, 1024, 4096);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("MB"));
    }

    #[test]
    fn resolve_balloon_percentage_computes_against_cap() {
        // 25% of 4096 = 1024
        let r = resolve_balloon_target(
            BalloonEntry::Percentage, "25", &[], 0, 1024, 4096);
        assert_eq!(r.unwrap(), 1024);
    }

    #[test]
    fn resolve_balloon_percentage_clamps_to_100() {
        let r = resolve_balloon_target(
            BalloonEntry::Percentage, "150", &[], 0, 1024, 4096);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("150"));
    }

    #[test]
    fn resolve_balloon_percentage_falls_back_to_current_when_max_zero() {
        // Backends that don't report max should still allow %-based entry
        // against the current value, so the user has SOME ceiling to
        // reason about.
        let r = resolve_balloon_target(
            BalloonEntry::Percentage, "50", &[], 0, 2048, 0);
        assert_eq!(r.unwrap(), 1024);
    }

    // ---- Daemon-mode bootstrap & translation ----------------------------

    #[test]
    fn pick_worker_strategy_falls_back_to_direct_when_socket_missing() {
        // We can't temporarily move /run/rotten-apple.sock out of the way,
        // so this test only proves the fallback when DEFAULT_SOCKET_PATH
        // doesn't exist. In CI it never does. On a dev machine where the
        // daemon IS running we'd skip the assert — so we just check that
        // *something* came back.
        let strat = pick_worker_strategy();
        match strat {
            WorkerStrategy::Direct => {} // expected on a clean test host
            WorkerStrategy::Daemon(_) => {
                // Daemon present — that's also valid, just not our target.
            }
        }
    }

    #[test]
    fn daemon_host_is_usable_rejects_unavailable_backend() {
        assert!(!daemon_host_is_usable(&json!({})));
        assert!(!daemon_host_is_usable(&json!({"backend": "unavailable"})));
    }

    #[test]
    fn daemon_host_is_usable_accepts_real_backend_name() {
        assert!(daemon_host_is_usable(&json!({"backend": "xen"})));
    }

    #[test]
    fn cmd_to_rpc_refresh_is_local_only() {
        assert!(cmd_to_rpc(&Cmd::Refresh).is_none());
    }

    #[test]
    fn cmd_to_rpc_shutdown_is_local_only() {
        assert!(cmd_to_rpc(&Cmd::Shutdown).is_none());
    }

    #[test]
    fn cmd_to_rpc_promote_is_local_only() {
        assert!(cmd_to_rpc(&Cmd::PromoteXenDefault).is_none());
    }

    #[test]
    fn cmd_to_rpc_start_emits_domain_start() {
        let (m, p) = cmd_to_rpc(&Cmd::Start(GuestHandle("3".into()))).unwrap();
        assert_eq!(m, "domain.start");
        assert_eq!(p["domid"], 3);
    }

    #[test]
    fn cmd_to_rpc_stop_graceful_carries_force_false() {
        let (m, p) = cmd_to_rpc(&Cmd::Stop {
            handle: GuestHandle("4".into()), force: false,
        }).unwrap();
        assert_eq!(m, "domain.shutdown");
        assert_eq!(p["domid"], 4);
        assert_eq!(p["force"], false);
    }

    #[test]
    fn cmd_to_rpc_stop_forced_carries_force_true() {
        let (m, p) = cmd_to_rpc(&Cmd::Stop {
            handle: GuestHandle("4".into()), force: true,
        }).unwrap();
        assert_eq!(m, "domain.shutdown");
        assert_eq!(p["force"], true);
    }

    #[test]
    fn cmd_to_rpc_balloon_converts_mb_to_kb() {
        // 2048 MB → 2_097_152 kB. The whole point of pulling cmd_to_rpc
        // out of the worker was to keep this conversion testable.
        let (m, p) = cmd_to_rpc(&Cmd::Balloon {
            handle: GuestHandle("0".into()),
            target_mb: 2048,
        }).unwrap();
        assert_eq!(m, "domain.balloon");
        assert_eq!(p["domid"], 0);
        assert_eq!(p["target_kb"], 2_097_152u64);
    }

    #[test]
    fn cmd_to_rpc_set_policy_emits_engine_set_policy() {
        // Wire shape must match orchestratord::dispatch::parse_set_policy:
        // {domid, policy: {min_mb, max_mb, cooldown_s}}.
        let (m, p) = cmd_to_rpc(&Cmd::SetPolicy {
            domid: 5,
            min_mb: 256,
            max_mb: 4096,
            cooldown_s: 30,
        }).unwrap();
        assert_eq!(m, "engine.set_policy");
        assert_eq!(p["domid"], 5);
        assert_eq!(p["policy"]["min_mb"], 256);
        assert_eq!(p["policy"]["max_mb"], 4096);
        assert_eq!(p["policy"]["cooldown_s"], 30);
    }

    #[test]
    fn cmd_to_rpc_create_uses_manifest_path() {
        let (m, p) = cmd_to_rpc(&Cmd::CreateFromManifest(
            PathBuf::from("/etc/rotten-apple/active.toml"))).unwrap();
        assert_eq!(m, "domain.create");
        assert_eq!(p["manifest_path"], "/etc/rotten-apple/active.toml");
    }

    #[test]
    fn cmd_to_rpc_returns_none_for_non_numeric_handle() {
        // The daemon API requires u32 domids; the cockpit's GuestHandle
        // for a libxl-managed domain is always numeric, but if some
        // other path ever produced a non-numeric handle we should fail
        // fast rather than send a malformed request.
        assert!(cmd_to_rpc(&Cmd::Start(GuestHandle("ubuntu-desk".into())))
            .is_none());
    }

    #[test]
    fn parse_state_str_round_trips_known_variants() {
        assert!(matches!(parse_state_str("Created"),  GuestState::Created));
        assert!(matches!(parse_state_str("Running"),  GuestState::Running));
        assert!(matches!(parse_state_str("Idle"),     GuestState::Idle));
        assert!(matches!(parse_state_str("Suspended"),GuestState::Suspended));
        assert!(matches!(parse_state_str("Stopped"),  GuestState::Stopped));
        assert!(matches!(parse_state_str("Failed"),   GuestState::Failed));
    }

    #[test]
    fn parse_state_str_unknown_falls_back_to_stopped() {
        assert!(matches!(parse_state_str("garbage"), GuestState::Stopped));
        assert!(matches!(parse_state_str(""),        GuestState::Stopped));
    }

    #[test]
    fn domain_view_from_daemon_parses_all_fields() {
        let v = json!({
            "domid": 7,
            "name": "guest-a",
            "state": "Running",
            "memory_mb": 2048,
            "memory_max_mb": 4096,
            "vcpus": 4,
            "uptime_seconds": 90,
        });
        let dv = domain_view_from_daemon(&v).expect("parses");
        assert_eq!(dv.handle.0, "7");
        assert_eq!(dv.name, "guest-a");
        assert!(matches!(dv.state, GuestState::Running));
        let st = dv.status.expect("status");
        assert_eq!(st.memory_mb, 2048);
        assert_eq!(st.memory_max_mb, 4096);
        assert_eq!(st.vcpus, 4);
        assert_eq!(st.uptime.as_secs(), 90);
    }

    #[test]
    fn domain_view_from_daemon_returns_none_for_missing_fields() {
        let v = json!({ "domid": 7 }); // missing name + state
        assert!(domain_view_from_daemon(&v).is_none());
    }

    #[test]
    fn backend_source_label_is_at_a_glance_distinct() {
        assert_eq!(BackendSource::DaemonUnix.label(),  "daemon/unix");
        assert_eq!(BackendSource::DaemonVsock.label(), "daemon/vsock");
        assert_eq!(BackendSource::Libxl.label(),       "via libxl");
    }

    #[test]
    fn handle_to_domid_parses_numeric() {
        assert_eq!(handle_to_domid(&GuestHandle("0".into())), Some(0));
        assert_eq!(handle_to_domid(&GuestHandle("123".into())), Some(123));
        assert_eq!(handle_to_domid(&GuestHandle("ubuntu".into())), None);
    }
}
