//! rotten-apple-orchestratord — JSON-RPC daemon owning libxl.
//!
//! Scaffold: today the daemon answers `hello` and `ping` and rejects
//! everything else. The libxl actor lands next.
//!
//! Usage:
//!   rotten-apple-orchestratord [--socket /run/rotten-apple.sock] [--vsock-port 47000]

use std::path::PathBuf;
use std::process::ExitCode;

use rotten_apple_orchestratord::{run, DaemonConfig};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cfg = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "usage: rotten-apple-orchestratord [--socket <path>] [--vsock-port <port>|--no-vsock]"
            );
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("orchestratord: unix {}", cfg.socket_path.display());
    match cfg.vsock_port {
        Some(port) => println!("orchestratord: vsock port {port}"),
        None => println!("orchestratord: vsock disabled"),
    }

    match run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[orchestratord] FATAL: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(args: &[String]) -> Result<DaemonConfig, String> {
    let mut iter = args.iter().skip(1);
    let mut cfg = DaemonConfig::default();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" | "-s" => {
                let p = iter.next().ok_or("--socket requires a path argument")?;
                cfg.socket_path = PathBuf::from(p);
            }
            "--vsock-port" => {
                let p = iter.next().ok_or("--vsock-port requires a port argument")?;
                let port = p.parse::<u32>()
                    .map_err(|_| format!("invalid --vsock-port value: {p}"))?;
                cfg.vsock_port = Some(port);
            }
            "--no-vsock" => cfg.vsock_port = None,
            "--help" | "-h" => {
                println!(
                    "usage: rotten-apple-orchestratord [--socket <path>] [--vsock-port <port>|--no-vsock]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(cfg)
}
