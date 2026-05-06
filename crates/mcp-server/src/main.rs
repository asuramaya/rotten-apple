//! rotten-apple-mcp — MCP stdio server, child of an MCP-aware client.
//!
//! Spawned by Claude Code (or any MCP-aware client) over stdio. Speaks
//! MCP JSON-RPC on stdin/stdout and translates `tools/call` into
//! orchestratord JSON-RPC calls over the Unix socket.
//!
//! Usage:
//!   rotten-apple-mcp [--socket /run/rotten-apple.sock]

use std::path::PathBuf;
use std::process::ExitCode;

use rotten_apple_mcp_server::{run, McpConfig, DEFAULT_SOCKET_PATH};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let socket_path = match parse_socket_arg(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("usage: rotten-apple-mcp [--socket <path>]");
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match run(McpConfig { socket_path }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[rotten-apple-mcp] FATAL: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_socket_arg(args: &[String]) -> Result<PathBuf, String> {
    // Last --socket wins; --help exits before returning. The loop iterates
    // for forward-compat with future flags rather than early-returning on
    // the first match.
    let mut iter = args.iter().skip(1);
    let mut socket = PathBuf::from(DEFAULT_SOCKET_PATH);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" | "-s" => {
                let p = iter.next().ok_or("--socket requires a path argument")?;
                socket = PathBuf::from(p);
            }
            "--help" | "-h" => {
                println!("usage: rotten-apple-mcp [--socket <path>]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(socket)
}
