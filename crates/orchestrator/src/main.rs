//! rotten-apple-orchestrator — minimal v0.1 daemon.
//!
//! Two ways this gets invoked:
//!
//!   1. Inside dom0 as PID 1: the dom0 image's init script execs us
//!      directly with the manifest path baked into the initramfs.
//!      `--manifest /etc/rotten-apple/active.toml`. Exit means kernel
//!      panic, so on shutdown we reboot the host instead of returning.
//!
//!   2. On a dev box, manually: `rotten-apple-orchestrator --manifest
//!      ./manifests/whatever.toml`. SIGTERM/SIGINT trigger graceful
//!      shutdown; the binary exits cleanly when done.
//!
//! Mode is detected by `getpid() == 1`.

use std::path::PathBuf;
use std::process::ExitCode;

use rotten_apple_orchestrator::{
    check, install_signal_handlers, plan, run, ShutdownFlag,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let check_only = args.iter().any(|a| a == "--check");
    let manifest = match parse_manifest_arg(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("usage: rotten-apple-orchestrator --manifest <path> [--check]");
            eprintln!("       (running as PID {}); error: {e}", unsafe { libc::getpid() });
            return ExitCode::FAILURE;
        }
    };

    let am_pid_one = unsafe { libc::getpid() } == 1;
    eprintln!("[orchestrator] starting (pid {}, mode={})",
              unsafe { libc::getpid() },
              if am_pid_one { "init" } else { "user" });

    let (profile, plan_info) = match plan(&manifest) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[orchestrator] plan: {e}");
            return panic_or_exit(am_pid_one, ExitCode::from(2));
        }
    };
    eprintln!("[orchestrator] manifest:    {}", plan_info.manifest_path);
    eprintln!("[orchestrator] guest:       {}", plan_info.profile_name);
    eprintln!("[orchestrator] backend:     {}", plan_info.backend_name);
    eprintln!("[orchestrator] libxl:       {}", plan_info.libxl_version);

    if check_only {
        eprintln!("[orchestrator] --check: connect, list, exit");
        return match check(&profile) {
            Ok(()) => panic_or_exit(am_pid_one, ExitCode::SUCCESS),
            Err(e) => {
                eprintln!("[orchestrator] check FATAL: {e}");
                panic_or_exit(am_pid_one, ExitCode::from(2))
            }
        };
    }

    let shutdown = ShutdownFlag::new();
    install_signal_handlers(shutdown.clone());

    match run(&profile, shutdown) {
        Ok(()) => {
            eprintln!("[orchestrator] clean shutdown complete");
            panic_or_exit(am_pid_one, ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("[orchestrator] FATAL: {e}");
            panic_or_exit(am_pid_one, ExitCode::from(2))
        }
    }
}

fn parse_manifest_arg(args: &[String]) -> Result<PathBuf, String> {
    let mut iter = args.iter().skip(1);
    while let Some(a) = iter.next() {
        if a == "--manifest" || a == "-m" {
            let p = iter.next().ok_or("--manifest requires a path argument")?;
            return Ok(PathBuf::from(p));
        }
    }
    Err("--manifest <path> is required".into())
}

/// PID 1 cannot exit — the kernel panics. If we're init, reboot instead;
/// otherwise return the exit code normally.
fn panic_or_exit(am_pid_one: bool, code: ExitCode) -> ExitCode {
    if !am_pid_one { return code }
    eprintln!("[orchestrator] running as PID 1; rebooting in 3s instead of exiting");
    std::thread::sleep(std::time::Duration::from_secs(3));
    // SAFETY: standard libc reboot. RB_AUTOBOOT triggers a normal reboot.
    // If this returns, we fell through to the loop below which also won't
    // return — the kernel will panic when main returns to its caller.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT);
    }
    loop { std::thread::sleep(std::time::Duration::from_secs(60)); }
}
