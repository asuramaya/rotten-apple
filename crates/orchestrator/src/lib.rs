//! rotten-apple orchestrator core.
//!
//! v0.1 scope: read one manifest, create the guest, start it, block until
//! a termination signal, gracefully destroy guests, exit. No IPC API yet,
//! no policy loop, no resource arbitration. Just enough to lift this
//! laptop and have the Ubuntu domU come up.
//!
//! When running as PID 1 inside dom0:
//!   - exit(0) from PID 1 = kernel panic; the run loop never returns
//!     normally — it either reboots the box or stays parked forever.
//!   - The dom0 image's init script execs this binary directly with
//!     `--manifest /etc/rotten-apple/active.toml` (the path baked into
//!     the initramfs by `crates/dom0-image`).
//!
//! Outside dom0 (development): the same binary runs as a normal process
//! and SIGTERM / SIGINT trigger graceful shutdown.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rotten_apple_backend::{BackendError, HypervisorBackend};
use rotten_apple_backend_xen::XenBackend;
use rotten_apple_manifest::Profile;

#[derive(Debug)]
pub enum OrchError {
    LoadManifest(String),
    BackendInit(BackendError),
    CreateGuest(BackendError),
    StartGuest(BackendError),
}

impl std::fmt::Display for OrchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrchError::LoadManifest(s) => write!(f, "load manifest: {s}"),
            OrchError::BackendInit(e)  => write!(f, "backend init: {e}"),
            OrchError::CreateGuest(e)  => write!(f, "create_guest: {e}"),
            OrchError::StartGuest(e)   => write!(f, "start_guest: {e}"),
        }
    }
}

impl std::error::Error for OrchError {}

/// What the orchestrator does on startup. Pure-data so callers can
/// log + dry-run without taking action.
#[derive(Debug, Clone)]
pub struct StartupPlan {
    pub manifest_path: String,
    pub profile_name: String,
    pub backend_name: String,
    pub libxl_version: &'static str,
}

/// Termination signal — set by the signal handler, polled by run().
#[derive(Default)]
pub struct ShutdownFlag(AtomicBool);

impl ShutdownFlag {
    pub fn new() -> Arc<Self> { Arc::new(ShutdownFlag(AtomicBool::new(false))) }
    pub fn raise(&self) { self.0.store(true, Ordering::SeqCst); }
    pub fn is_raised(&self) -> bool { self.0.load(Ordering::SeqCst) }
}

/// Install SIGTERM / SIGINT handlers that flip the shutdown flag.
///
/// SAFETY: signal handlers must be async-signal-safe. We only call
/// `Arc::clone` (no allocator) ... actually Arc::clone IS safe inside
/// a handler since it's an atomic refcount bump. The store on the
/// AtomicBool is async-signal-safe. We do NOT print or allocate.
pub fn install_signal_handlers(flag: Arc<ShutdownFlag>) {
    static mut FLAG: Option<Arc<ShutdownFlag>> = None;
    extern "C" fn handler(_sig: libc::c_int) {
        // SAFETY: the orchestrator installs handlers exactly once, before
        // any concurrent reader exists. This is single-process single-thread
        // initialisation; the static FLAG is set by install() only.
        unsafe {
            #[allow(static_mut_refs)]
            if let Some(f) = &FLAG {
                f.raise();
            }
        }
    }
    // SAFETY: standard libc setup; SA_RESTART so syscalls in the run loop
    // (sleep, etc.) restart cleanly after the handler returns.
    unsafe {
        FLAG = Some(flag);
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

/// Load the manifest and produce a startup plan. No side effects.
pub fn plan(manifest_path: &Path) -> Result<(Profile, StartupPlan), OrchError> {
    let profile = Profile::load(manifest_path)
        .map_err(|e| OrchError::LoadManifest(e.to_string()))?;
    let plan = StartupPlan {
        manifest_path: manifest_path.display().to_string(),
        profile_name: profile.name().to_string(),
        backend_name: "xen".into(),
        libxl_version: rotten_apple_backend_xen::compat::LIBXL_BUILD_VERSION,
    };
    Ok((profile, plan))
}

/// One-shot health check. v0.0.1 mode for the systemd service: open
/// libxl, log the connected version, list domains, exit cleanly.
/// Doesn't create or start anything. Lets the systemd service prove
/// "Xen is up, libxl is reachable, our binary works" on every boot
/// without needing a working guest disk image.
pub fn check(profile: &Profile) -> Result<(), OrchError> {
    let backend = XenBackend::new().map_err(OrchError::BackendInit)?;
    eprintln!("[orchestrator] check: backend={} libxl={}",
              backend.name(),
              rotten_apple_backend_xen::compat::LIBXL_BUILD_VERSION);
    let domains = backend.list();
    eprintln!("[orchestrator] check: {} domain(s) currently running",
              domains.len());
    for d in &domains {
        eprintln!("    {} {:?} {}", d.handle, d.state, d.name);
    }
    eprintln!("[orchestrator] check: manifest '{}' loaded but not acted on \
               (--check mode)", profile.name());
    Ok(())
}

/// Bring up the backend, create + start the guest, block until the
/// shutdown flag is raised, then tear down.
///
/// Returns Ok(()) on a clean shutdown. Inside dom0 as PID 1, the caller
/// must NOT exit on Ok — it must reboot or hang. Outside dom0 (tests,
/// dev), Ok means "shutdown completed cleanly."
pub fn run(profile: &Profile, shutdown: Arc<ShutdownFlag>) -> Result<(), OrchError> {
    let backend = XenBackend::new().map_err(OrchError::BackendInit)?;

    eprintln!("[orchestrator] backend ready: {}", backend.name());

    let handle = backend.create_guest(profile)
        .map_err(OrchError::CreateGuest)?;
    eprintln!("[orchestrator] created: {handle}");

    backend.start_guest(&handle)
        .map_err(OrchError::StartGuest)?;
    eprintln!("[orchestrator] started: {handle}");

    eprintln!("[orchestrator] entering main loop; SIGTERM/SIGINT to shut down");
    park_until_shutdown(&shutdown);

    eprintln!("[orchestrator] shutdown signal received; destroying {handle}");
    if let Err(e) = backend.stop_guest(&handle, false) {
        eprintln!("[orchestrator] graceful shutdown failed: {e}; forcing");
        if let Err(e) = backend.stop_guest(&handle, true) {
            eprintln!("[orchestrator] force destroy also failed: {e}");
        }
    }
    Ok(())
}

/// Poll the shutdown flag with a short sleep. Avoids spinning.
/// Doesn't use thread::park because PID 1 may not have a thread runtime
/// configured the way std assumes — sleep is a syscall, well-defined.
fn park_until_shutdown(shutdown: &ShutdownFlag) {
    while !shutdown.is_raised() {
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Convenience: format a timestamp for log lines (rough, no chrono dep).
pub fn elapsed_str(since: Instant) -> String {
    let d = since.elapsed();
    format!("+{}.{:03}s", d.as_secs(), d.subsec_millis())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_flag_round_trip() {
        let f = ShutdownFlag::new();
        assert!(!f.is_raised());
        f.raise();
        assert!(f.is_raised());
    }

    #[test]
    fn plan_returns_profile_and_metadata() {
        let toml = r#"
            [profile]
            name = "test-domu"
            type = "desktop"
            [meta]
            [resources]
            memory_active = "2G"
            memory_idle = "1G"
            memory_minimum = "512M"
            vcpus_active = 2
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "block", source = "/dev/null", mode = "rw-exclusive" }
            [tpm]
            mode = "none"
            [autostart]
        "#;
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), toml).unwrap();
        let (profile, plan) = plan(f.path()).unwrap();
        assert_eq!(profile.name(), "test-domu");
        assert_eq!(plan.profile_name, "test-domu");
        assert_eq!(plan.backend_name, "xen");
        assert!(!plan.libxl_version.is_empty());
    }

    #[test]
    fn plan_load_error_is_typed() {
        let err = plan(Path::new("/nonexistent/path/to/missing.toml")).unwrap_err();
        assert!(matches!(err, OrchError::LoadManifest(_)));
    }
}
