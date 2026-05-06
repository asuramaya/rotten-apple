//! Auto-wire optional adjuncts at install time.
//!
//! When `rotten-apple install` finds the daemon (`rotten-apple-orchestratord`)
//! or the MCP stdio server (`rotten-apple-mcp`) sitting next to the cli
//! binary, install them too — and glue them into systemd / Claude Code so
//! the user doesn't have to think about the lifecycle. Anything missing
//! is skipped with a log line; the cli install never fails on adjunct
//! absence.
//!
//! Idempotent: running install twice produces the same end state. The
//! Claude Code config merge preserves any other `mcpServers` entries the
//! user has set up.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::LiftError;

/// Public API: glue everything that's available next to `cli_binary`.
/// Returns the list of components actually installed (for the install
/// summary line). Errors here are non-fatal — a single broken adjunct
/// shouldn't block the rest.
///
/// Order matters:
///   1. Cleanup legacy units (so the new daemon doesn't fight an old one)
///   2. Stop the daemon if running (so binary swap is atomic)
///   3. Copy fresh binaries
///   4. Write/refresh systemd unit
///   5. daemon-reload + restart (picks up the fresh binary AND any unit edits)
pub fn install_adjuncts(cli_binary: &Path, dry_run: bool)
    -> Result<Vec<&'static str>, LiftError>
{
    let mut installed = Vec::new();
    let dir = cli_binary.parent().unwrap_or(Path::new("/usr/local/bin"));

    // Always run, even when no daemon binary is present — the legacy
    // --check service should be removed regardless of the new install
    // path, since we don't ship that mode anymore.
    cleanup_legacy_units(dry_run)?;

    let orchestratord = dir.join("rotten-apple-orchestratord");
    if orchestratord.exists() {
        // Stop first so the binary file isn't being executed while we copy
        // over it. systemd will return success even if the unit isn't
        // currently active.
        stop_daemon_if_running(dry_run);
        install_orchestratord_binary(&orchestratord, dry_run)?;
        write_daemon_systemd_unit(dry_run)?;
        // restart-not-just-start so an already-running instance picks up
        // the fresh binary; first install will simply start it.
        enable_and_restart_daemon(dry_run)?;
        installed.push("orchestratord (systemd unit enabled, restarted)");
    } else {
        eprintln!("    [autowire] orchestratord binary not found at {} — skipping",
                  orchestratord.display());
    }

    let mcp = dir.join("rotten-apple-mcp");
    if mcp.exists() {
        install_mcp_binary(&mcp, dry_run)?;
        match register_mcp_with_claude(dry_run) {
            Ok(true)  => installed.push("MCP server (registered with Claude Code)"),
            Ok(false) => installed.push("MCP server (binary only — no Claude config found)"),
            Err(e)    => eprintln!("    [autowire mcp] registration skipped: {e}"),
        }
    } else {
        eprintln!("    [autowire] mcp binary not found at {} — skipping",
                  mcp.display());
    }

    Ok(installed)
}

// ---------------------------------------------------------------------------
// Daemon binary + systemd

fn install_orchestratord_binary(src: &Path, dry_run: bool) -> Result<(), LiftError> {
    let dst = Path::new("/usr/local/bin/rotten-apple-orchestratord");
    if dry_run {
        eprintln!("    [autowire daemon] would copy {} -> {} (chmod 755)",
                  src.display(), dst.display());
        return Ok(());
    }
    if crate::same_inode(src, dst) {
        eprintln!("    [autowire daemon] {} (already current — skipping copy)",
                  dst.display());
        return Ok(());
    }
    crate::atomic_swap(src, dst, "install orchestratord binary")?;
    eprintln!("    [autowire daemon] → {}", dst.display());
    Ok(())
}

/// systemd unit for the long-running daemon. Replaces the old `--check`
/// orchestrator unit semantics: this one is the actual libxl owner that
/// cockpit/MCP-server/CLI talk to. Restart=always so a libxl crash brings
/// the daemon back; cockpit's auto-detect re-attaches to the new socket.
fn write_daemon_systemd_unit(dry_run: bool) -> Result<(), LiftError> {
    let unit = r#"[Unit]
Description=rotten-apple orchestratord — single libxl owner, JSON-RPC over unix socket + vsock
After=xen.service xen-init-dom0.service xenstored.service xenconsoled.service network-online.target
Wants=xen.service xen-init-dom0.service

[Service]
Type=simple
ExecStart=/usr/local/bin/rotten-apple-orchestratord --socket /run/rotten-apple.sock
Restart=always
RestartSec=3
StandardOutput=journal
StandardError=journal
# Socket is at /run, owned root:root mode 0660. A `rotten-apple` group can
# be added later so cockpit can run un-sudo'd; for v0.0.2 root-only is fine.

[Install]
WantedBy=multi-user.target
"#;
    let path = Path::new("/etc/systemd/system/rotten-apple-orchestratord.service");
    if dry_run {
        eprintln!("    [autowire daemon] would write {} ({} bytes)",
                  path.display(), unit.len());
        return Ok(());
    }
    std::fs::write(path, unit).map_err(|e|
        LiftError::Command {
            step: "install orchestratord systemd unit",
            detail: format!("write {}: {e}", path.display()),
        })?;
    eprintln!("    [autowire daemon] → {}", path.display());
    Ok(())
}

fn enable_and_restart_daemon(dry_run: bool) -> Result<(), LiftError> {
    if dry_run {
        eprintln!("    [autowire daemon] would daemon-reload + enable + restart");
        return Ok(());
    }
    // daemon-reload picks up unit edits. `enable` is idempotent. `restart`
    // (rather than `start`) ensures any already-running instance picks up
    // the fresh binary we just copied — this was a real bug pre-v0.0.3
    // where install left the old binary running.
    run("systemctl daemon-reload",       &["daemon-reload"])?;
    run("systemctl enable daemon",       &["enable",
        "rotten-apple-orchestratord.service"])?;
    run("systemctl restart daemon",      &["restart",
        "rotten-apple-orchestratord.service"])?;
    eprintln!("    [autowire daemon] enabled + restarted");
    Ok(())
}

/// Stop the daemon if it's currently running. Idempotent — `stop` on an
/// inactive unit is a no-op. We pre-stop before binary copy so we never
/// rewrite an executable that's actively running (Linux allows it but
/// it's ugly and on some filesystems the running process pins the inode).
fn stop_daemon_if_running(dry_run: bool) {
    if dry_run {
        eprintln!("    [autowire daemon] would stop daemon (if running)");
        return;
    }
    // Best-effort: if the unit doesn't exist yet (first install), this
    // exits non-zero and we ignore it.
    let _ = Command::new("systemctl")
        .args(["stop", "rotten-apple-orchestratord.service"])
        .output();
}

/// Pre-v0.0.3 installs wrote `rotten-apple-orchestrator.service` (the
/// `--check` one-shot). v0.0.3 renamed it to `rotten-apple-orchestratord.service`
/// (the long-running daemon). If the legacy unit is still on disk it'll
/// keep running on boot and either fight the new daemon for libxl or just
/// noisy-fail every 10s. Detect + remove cleanly.
///
/// Idempotent: no-op when the legacy file isn't present.
fn cleanup_legacy_units(dry_run: bool) -> Result<(), LiftError> {
    let legacy = Path::new("/etc/systemd/system/rotten-apple-orchestrator.service");
    if !legacy.exists() {
        return Ok(());
    }
    if dry_run {
        eprintln!("    [autowire] would remove legacy unit {}", legacy.display());
        return Ok(());
    }
    // Best-effort stop + disable, then unlink. systemctl returns non-zero
    // when the unit was never enabled or already inactive — treat as ok.
    let _ = Command::new("systemctl")
        .args(["stop", "rotten-apple-orchestrator.service"]).output();
    let _ = Command::new("systemctl")
        .args(["disable", "rotten-apple-orchestrator.service"]).output();
    std::fs::remove_file(legacy).map_err(|e|
        LiftError::Command {
            step: "remove legacy systemd unit",
            detail: format!("unlink {}: {e}", legacy.display()),
        })?;
    eprintln!("    [autowire] removed legacy unit (renamed to ...orchestratord.service)");
    Ok(())
}

fn run(label: &'static str, args: &[&str]) -> Result<(), LiftError> {
    let out = Command::new("systemctl").args(args).output().map_err(|e|
        LiftError::Command { step: label, detail: format!("spawn: {e}") })?;
    if !out.status.success() {
        return Err(LiftError::Command {
            step: label,
            detail: format!("exit={}; stderr: {}", out.status,
                            String::from_utf8_lossy(&out.stderr)),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MCP binary + Claude Code registration

fn install_mcp_binary(src: &Path, dry_run: bool) -> Result<(), LiftError> {
    let dst = Path::new("/usr/local/bin/rotten-apple-mcp");
    if dry_run {
        eprintln!("    [autowire mcp] would copy {} -> {} (chmod 755)",
                  src.display(), dst.display());
        return Ok(());
    }
    if crate::same_inode(src, dst) {
        eprintln!("    [autowire mcp] {} (already current — skipping copy)",
                  dst.display());
        return Ok(());
    }
    crate::atomic_swap(src, dst, "install mcp binary")?;
    eprintln!("    [autowire mcp] → {}", dst.display());
    Ok(())
}

/// Merge `mcpServers.rotten-apple` into the SUDO_USER's `~/.claude.json`.
/// Returns Ok(true) if registration happened, Ok(false) if the user has
/// no Claude config dir yet (we don't proactively create one — only edit
/// what exists, since dropping a `.claude.json` into a home that doesn't
/// use Claude Code is noise).
fn register_mcp_with_claude(dry_run: bool) -> Result<bool, String> {
    let target_user = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .map_err(|_| "no SUDO_USER or USER in env".to_string())?;
    let home = home_for_user(&target_user)
        .ok_or_else(|| format!("no home dir for user {target_user}"))?;
    let path = home.join(".claude.json");
    if dry_run {
        eprintln!("    [autowire mcp] would ensure {} contains mcpServers.rotten-apple",
                  path.display());
        return Ok(true);
    }
    // Pre-v0.0.3 we skipped registration when ~/.claude.json was absent.
    // That made install order-dependent: if the user installed Claude
    // Code AFTER rotten-apple, the MCP entry was missing and they had
    // to re-run our install. Now we create the file with `{}` if absent;
    // Claude Code reads it tolerantly, will merge its own settings on
    // first launch, and our entry is already there.
    let raw = if path.exists() {
        std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?
    } else {
        std::fs::write(&path, "{}\n")
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        chown_to_user(&path, &target_user)?;
        eprintln!("    [autowire mcp] created {} (Claude Code can be installed later)",
                  path.display());
        "{}\n".to_string()
    };
    let merged = merge_mcp_into_claude_config(&raw)
        .map_err(|e| format!("merge {}: {e}", path.display()))?;
    if raw == merged {
        eprintln!("    [autowire mcp] {} already up to date", path.display());
        return Ok(true);
    }
    std::fs::write(&path, &merged)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    chown_to_user(&path, &target_user)?;
    eprintln!("    [autowire mcp] → merged into {}", path.display());
    Ok(true)
}

/// Read /etc/passwd and pluck the home dir for `user`. Avoids pulling in
/// nss-style libs; getpwnam_r in libc would be nicer but linking it
/// portably across glibc/musl is more trouble than the parse.
fn home_for_user(user: &str) -> Option<PathBuf> {
    let s = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in s.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        // user:x:uid:gid:gecos:home:shell
        if fields.first().copied() == Some(user) {
            return fields.get(5).map(PathBuf::from);
        }
    }
    None
}

fn chown_to_user(path: &Path, user: &str) -> Result<(), String> {
    // We changed the file owned by `user` while running as root; chown
    // it back so future Claude Code launches (running as user) can write
    // their own session state without permission errors.
    let s = std::fs::read_to_string("/etc/passwd")
        .map_err(|e| format!("read /etc/passwd: {e}"))?;
    for line in s.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.first().copied() == Some(user) {
            let uid: u32 = fields.get(2).and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("bad uid for {user}"))?;
            let gid: u32 = fields.get(3).and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("bad gid for {user}"))?;
            let cstr = std::ffi::CString::new(path.to_string_lossy().as_bytes())
                .map_err(|e| format!("path nul: {e}"))?;
            // SAFETY: standard libc chown with valid c-string + uid/gid.
            unsafe { libc::chown(cstr.as_ptr(), uid, gid); }
            return Ok(());
        }
    }
    Err(format!("user {user} not in /etc/passwd"))
}

/// Pure JSON merge — the only piece worth unit-testing. Takes the
/// existing `~/.claude.json` content (or an empty string) and returns
/// the merged JSON as a pretty-printed string. Idempotent: feeding the
/// output back in produces byte-identical output.
pub(crate) fn merge_mcp_into_claude_config(existing: &str)
    -> Result<String, serde_json::Error>
{
    let mut config: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(existing)?
    };
    // If the root isn't an object, replace it. This handles the case
    // where a user has corrupted or hand-trimmed the file.
    if !config.is_object() {
        config = serde_json::json!({});
    }
    let entry = serde_json::json!({
        "command": "/usr/local/bin/rotten-apple-mcp",
        "args": []
    });
    let obj = config.as_object_mut().expect("just-replaced root is an object");
    let servers = obj.entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        *servers = serde_json::json!({});
    }
    servers.as_object_mut().expect("just-replaced servers is an object")
        .insert("rotten-apple".to_string(), entry);
    let mut out = serde_json::to_string_pretty(&config)?;
    out.push('\n');
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_into_empty_string_creates_full_structure() {
        let merged = merge_mcp_into_claude_config("").unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert!(v.get("mcpServers")
                 .and_then(|s| s.get("rotten-apple"))
                 .is_some());
    }

    #[test]
    fn merge_into_empty_object_adds_mcp_servers() {
        let merged = merge_mcp_into_claude_config("{}").unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["mcpServers"]["rotten-apple"]["command"],
                   "/usr/local/bin/rotten-apple-mcp");
        assert!(v["mcpServers"]["rotten-apple"]["args"]
            .as_array().is_some_and(|a| a.is_empty()));
    }

    #[test]
    fn merge_preserves_other_mcp_servers() {
        let existing = r#"{
            "mcpServers": {
                "filesystem": { "command": "/usr/local/bin/mcp-filesystem" }
            }
        }"#;
        let merged = merge_mcp_into_claude_config(existing).unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert!(v["mcpServers"].get("filesystem").is_some(),
                "filesystem entry should be preserved");
        assert!(v["mcpServers"].get("rotten-apple").is_some(),
                "rotten-apple entry should be added");
    }

    #[test]
    fn merge_preserves_unrelated_top_level_keys() {
        let existing = r#"{ "theme": "dark", "fontSize": 13 }"#;
        let merged = merge_mcp_into_claude_config(existing).unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["fontSize"], 13);
        assert!(v["mcpServers"]["rotten-apple"].is_object());
    }

    #[test]
    fn merge_overwrites_stale_rotten_apple_entry() {
        // Older versions might have written a different command path;
        // re-running install must replace, not duplicate.
        let existing = r#"{
            "mcpServers": {
                "rotten-apple": { "command": "/old/path/rotten-apple-mcp" }
            }
        }"#;
        let merged = merge_mcp_into_claude_config(existing).unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["mcpServers"]["rotten-apple"]["command"],
                   "/usr/local/bin/rotten-apple-mcp");
    }

    #[test]
    fn merge_is_idempotent() {
        let once  = merge_mcp_into_claude_config("{}").unwrap();
        let twice = merge_mcp_into_claude_config(&once).unwrap();
        assert_eq!(once, twice, "merging an already-merged config should be a no-op");
    }

    #[test]
    fn merge_recovers_from_non_object_root() {
        // Pathological case: someone wrote `[]` or a string. We don't
        // want to crash; we replace with a fresh object.
        let merged = merge_mcp_into_claude_config(r#"["not an object"]"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert!(v["mcpServers"]["rotten-apple"].is_object());
    }

    #[test]
    fn merge_recovers_from_non_object_mcpservers() {
        let merged = merge_mcp_into_claude_config(r#"{"mcpServers": "broken"}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert!(v["mcpServers"]["rotten-apple"].is_object());
    }

    #[test]
    fn home_for_unknown_user_returns_none() {
        // Unlikely a user named this exists in /etc/passwd on any host.
        assert!(home_for_user("definitely-not-a-real-user-zzz9999").is_none());
    }
}
