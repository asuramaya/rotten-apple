//! Boot-mode toggle: "land directly in the cockpit on tty1" vs the
//! standard Ubuntu desktop. Reversible via the inverse function.
//!
//! Two on-disk side-effects when enabled:
//!   1. The display manager (gdm/lightdm/sddm/…) is `systemctl mask`ed
//!      so it doesn't grab the framebuffer at boot.
//!   2. `getty@tty1.service` gets a drop-in override that replaces the
//!      login prompt with `/usr/local/bin/rotten-apple cockpit`.
//!
//! Disable reverses both. The masked DM name is recorded in
//! `/var/lib/rotten-apple/boot-mode-state.toml` so we know what to
//! unmask. If the state file is missing or corrupt, disable falls back
//! to unmasking every known DM — disable must always succeed so the
//! user can recover.
//!
//! Tests poke at the override-path indirection rather than running
//! systemctl; integration testing belongs to a real dom0.
//!
//! No new crate deps — the state file is hand-rolled key=value, not
//! TOML-parsed.
//!
//! NOTE for v0.0.1: only the override file is consulted by
//! `current_boot_mode`. If the user manually masks/unmasks the DM
//! outside this tool, we won't notice — which is fine, because the
//! cockpit-on-tty1 effect is the override file's, not the mask's.

use std::path::Path;
use std::process::Command;

use crate::{LiftError, Result};

// ---------------------------------------------------------------------------
// Public types

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    /// Standard Ubuntu desktop on next boot (gdm/lightdm/etc. visible).
    Desktop,
    /// rotten-apple cockpit on tty1; display manager masked.
    Cockpit,
}

impl BootMode {
    pub fn label(self) -> &'static str {
        match self {
            BootMode::Desktop => "Desktop",
            BootMode::Cockpit => "Cockpit",
        }
    }
}

// ---------------------------------------------------------------------------
// Constants

const OVERRIDE_PATH: &str =
    "/etc/systemd/system/getty@tty1.service.d/00-rotten-apple-cockpit.conf";
const STATE_PATH: &str = "/var/lib/rotten-apple/boot-mode-state.toml";

/// Display managers we know about, tried in order. Concrete names FIRST:
/// on Ubuntu/Debian, `display-manager.service` is a symlink to the real
/// unit (e.g. `gdm3.service`), and `systemctl mask` refuses to overwrite
/// that symlink with the /dev/null mask-link. Masking the concrete unit
/// achieves the same effect and works cleanly. The alias stays in the
/// list as a final fallback for distros where it's a regular unit.
const KNOWN_DMS: &[&str] = &[
    "gdm3.service",
    "gdm.service",
    "lightdm.service",
    "sddm.service",
    "lxdm.service",
    "display-manager.service",
];

// Why each line is the way it is:
//
// [Unit].After  — orchestratord owns libxl. If cockpit launches before
//                 the daemon binds /run/rotten-apple.sock, the cockpit
//                 falls back to direct libxl, which might also fail
//                 mid-init and exit. Wait for the daemon's socket first.
// [Unit].Wants  — pull the daemon up if it isn't already (won't fail
//                 the cockpit if the daemon errors).
// ExecStartPre setfont — on 4K displays the kernel framebuffer console
//                 picks an 8x16 font and the cockpit's TUI is unreadable
//                 (tiny text, possibly missing box-drawing glyphs). Load
//                 the largest Unicode-capable PSF that ships with the
//                 console-setup package; failure is non-fatal so we don't
//                 trap people on hosts without that package.
// Restart=on-failure — DON'T respawn on clean exit ([q] from cockpit).
//   Pre-v0.0.5 we used `always` and it created a fail-loop that bricked
//   tty1 when backend init failed.
// StartLimit*    — three failed launches inside 60s and systemd stops
//                  respawning. tty1 then drops back to a usable getty
//                  and the user can recover with `boot-mode desktop`
//                  instead of having to Ctrl-Alt-F2 to another tty.
// TTYVTDisallocate=yes — clear scrollback between runs so a panic from
//                        a previous launch doesn't haunt the screen.
// Pre-rendered baseline override body (DEFAULT font). Tests still pin
// against this so the historical shape is documented; the live writer
// uses `render_override_body(font)` which can splice in a custom font.
#[allow(dead_code)]
const OVERRIDE_BODY: &str = "[Unit]
After=rotten-apple-orchestratord.service network-online.target
Wants=rotten-apple-orchestratord.service
StartLimitBurst=3
StartLimitIntervalSec=60

[Service]
ExecStartPre=-/usr/bin/setfont Uni3-TerminusBold32x16
ExecStart=
ExecStart=-/usr/local/bin/rotten-apple cockpit
StandardInput=tty
StandardOutput=tty
TTYPath=/dev/tty1
TTYReset=yes
TTYVHangup=yes
TTYVTDisallocate=yes
Restart=on-failure
RestartSec=5
";

// ---------------------------------------------------------------------------
// Public API

pub fn enable_cockpit_boot(dry_run: bool) -> Result<()> {
    enable_cockpit_boot_with_font(dry_run, default_console_font_for_host())
}

/// Default console font baked into the override at v0.0.5+. Large enough
/// to be legible on 4K displays where the kernel's 8x16 default would
/// be unreadably small. Operators can override via `--font` on the CLI.
pub const DEFAULT_CONSOLE_FONT: &str = "Uni3-TerminusBold32x16";
pub const DEFAULT_CONSOLE_FONT_1080P: &str = "Uni3-TerminusBold24x12";

/// Same as `enable_cockpit_boot` but lets the operator pick the console
/// font that gets loaded by the override's `ExecStartPre`. Empty string
/// or `"none"` skips the setfont line entirely (useful when the kernel
/// has already been told a good font via cmdline `fbcon=font:...`).
pub fn enable_cockpit_boot_with_font(dry_run: bool, font: &str) -> Result<()> {
    eprintln!("==> rotten-apple boot-mode → cockpit-on-tty1: {}",
              if dry_run { "DRY RUN" } else { "EXECUTE" });
    if !font.is_empty() && font != "none" {
        eprintln!("    [boot-mode] console font: {font}");
    }

    // 1. Mask the active display manager so it doesn't claim the
    //    framebuffer at boot.
    let masked = mask_display_manager(dry_run)?;

    // 2. Drop-in the getty@tty1 override that swaps login for cockpit.
    write_getty_override_with_font(dry_run, font)?;

    // 3. Persist what we masked so disable can unmask the same one.
    if !dry_run {
        if let Some(name) = &masked {
            write_state_file(name)?;
        }
    } else if let Some(name) = &masked {
        eprintln!("    [boot-mode] would record masked DM '{name}' to {STATE_PATH}");
    }

    // 4. Reload systemd so the override is picked up.
    daemon_reload(dry_run)?;

    eprintln!("==> cockpit boot enabled — reboot to apply");
    Ok(())
}

pub fn disable_cockpit_boot(dry_run: bool) -> Result<()> {
    eprintln!("==> rotten-apple boot-mode → desktop: {}",
              if dry_run { "DRY RUN" } else { "EXECUTE" });

    // 1. Unmask the DM we recorded — fall back to every known one if
    //    the state file is missing or corrupt. Disable must always
    //    succeed; recovery beats correctness.
    let to_unmask = read_state_file(Path::new(STATE_PATH))
        .map(|s| vec![s])
        .unwrap_or_else(|| KNOWN_DMS.iter().map(|s| s.to_string()).collect());
    for name in &to_unmask {
        unmask_display_manager(name, dry_run);
    }

    // 2. Remove the getty@tty1 override.
    remove_getty_override(dry_run)?;

    // 3. Drop the state file (best-effort; never fail on this).
    if !dry_run {
        let _ = std::fs::remove_file(STATE_PATH);
    } else {
        eprintln!("    [boot-mode] would remove {STATE_PATH}");
    }

    // 4. Reload systemd.
    daemon_reload(dry_run)?;

    eprintln!("==> cockpit boot disabled — reboot to return to desktop");
    Ok(())
}

pub fn current_boot_mode() -> BootMode {
    current_boot_mode_at(Path::new(OVERRIDE_PATH))
}

/// Test seam: same logic as `current_boot_mode` but lets the caller
/// pin the path it inspects. The public fn is a one-line wrapper.
fn current_boot_mode_at(override_path: &Path) -> BootMode {
    if override_path.exists() { BootMode::Cockpit } else { BootMode::Desktop }
}

// ---------------------------------------------------------------------------
// Display manager mask / unmask

/// Find the first DM in KNOWN_DMS that exists on this host and mask it.
/// Returns the name we masked (or None if none of them exist). Dry-run
/// prints what it would do without invoking systemctl.
fn mask_display_manager(dry_run: bool) -> Result<Option<String>> {
    let Some(name) = detect_display_manager() else {
        eprintln!("    [boot-mode] no known display manager found — nothing to mask");
        return Ok(None);
    };
    if dry_run {
        eprintln!("    [boot-mode] would: systemctl mask {name}");
        return Ok(Some(name));
    }
    let out = Command::new("systemctl").args(["mask", &name]).output()
        .map_err(|e| LiftError::Command {
            step: "boot-mode mask DM",
            detail: format!("spawn systemctl: {e}"),
        })?;
    if !out.status.success() {
        return Err(LiftError::Command {
            step: "boot-mode mask DM",
            detail: format!("systemctl mask {name}: exit={} stderr={}",
                out.status, String::from_utf8_lossy(&out.stderr)),
        });
    }
    eprintln!("    [boot-mode] masked {name}");
    Ok(Some(name))
}

/// Best-effort unmask. Doesn't fail the disable path if a particular
/// name wasn't masked or doesn't exist — we may be running through the
/// fallback "unmask every known DM" path where most won't apply.
fn unmask_display_manager(name: &str, dry_run: bool) {
    if dry_run {
        eprintln!("    [boot-mode] would: systemctl unmask {name}");
        return;
    }
    match Command::new("systemctl").args(["unmask", name]).output() {
        Ok(out) if out.status.success() =>
            eprintln!("    [boot-mode] unmasked {name}"),
        Ok(_) | Err(_) => {
            // Silent — name probably wasn't masked or unit doesn't exist.
        }
    }
}

/// Probe systemctl for each DM name; first one that returns a clean
/// `cat` exit is the one we'll mask. `cat` is cheap and doesn't require
/// the unit to be active or enabled.
fn detect_display_manager() -> Option<String> {
    for name in KNOWN_DMS {
        let out = Command::new("systemctl").args(["cat", name]).output().ok()?;
        if out.status.success() {
            return Some((*name).to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Getty override file

#[allow(dead_code)]
fn write_getty_override(dry_run: bool) -> Result<()> {
    write_getty_override_with_font(dry_run, default_console_font_for_host())
}

pub fn refresh_cockpit_boot_override_if_present(dry_run: bool) -> Result<bool> {
    if current_boot_mode() != BootMode::Cockpit {
        return Ok(false);
    }
    write_getty_override_with_font(dry_run, default_console_font_for_host())?;
    Ok(true)
}

pub fn default_console_font_for_host() -> &'static str {
    match framebuffer_size() {
        Some((w, h)) if w <= 1920 || h <= 1080 => DEFAULT_CONSOLE_FONT_1080P,
        _ => DEFAULT_CONSOLE_FONT,
    }
}

fn framebuffer_size() -> Option<(u32, u32)> {
    let raw = std::fs::read_to_string("/sys/class/graphics/fb0/virtual_size").ok()?;
    let mut parts = raw.trim().split(',');
    let w = parts.next()?.trim().parse().ok()?;
    let h = parts.next()?.trim().parse().ok()?;
    Some((w, h))
}

fn write_getty_override_with_font(dry_run: bool, font: &str) -> Result<()> {
    let path = Path::new(OVERRIDE_PATH);
    let body = render_override_body(font);
    if dry_run {
        eprintln!("    [boot-mode] would write {} ({} bytes)",
                  path.display(), body.len());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LiftError::Command {
            step: "boot-mode getty override",
            detail: format!("mkdir {}: {e}", parent.display()),
        })?;
    }
    std::fs::write(path, &body).map_err(|e| LiftError::Command {
        step: "boot-mode getty override",
        detail: format!("write {}: {e}", path.display()),
    })?;
    eprintln!("    [boot-mode] → {}", path.display());
    Ok(())
}

/// Build the override file body with a per-call font choice. `font`
/// empty or `"none"` skips the setfont line; otherwise it's an
/// `ExecStartPre=-/usr/bin/setfont <font>` (the `-` prefix makes it
/// best-effort so the unit doesn't fail when the font isn't installed).
fn render_override_body(font: &str) -> String {
    let setfont_line = if font.is_empty() || font == "none" {
        String::new()
    } else {
        format!("ExecStartPre=-/usr/bin/setfont {font}\n")
    };
    format!("[Unit]
After=rotten-apple-orchestratord.service network-online.target
Wants=rotten-apple-orchestratord.service
StartLimitBurst=3
StartLimitIntervalSec=60

[Service]
{setfont_line}ExecStart=
ExecStart=-/usr/local/bin/rotten-apple cockpit
StandardInput=tty
StandardOutput=tty
TTYPath=/dev/tty1
TTYReset=yes
TTYVHangup=yes
TTYVTDisallocate=yes
Restart=on-failure
RestartSec=5
")
}

fn remove_getty_override(dry_run: bool) -> Result<()> {
    let path = Path::new(OVERRIDE_PATH);
    if dry_run {
        eprintln!("    [boot-mode] would remove {}", path.display());
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Ok(()) => eprintln!("    [boot-mode] removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Already gone — that's the desired state.
        }
        Err(e) => return Err(LiftError::Command {
            step: "boot-mode getty override",
            detail: format!("remove {}: {e}", path.display()),
        }),
    }
    // Clean up empty parent dir so we don't leave stale .d directories.
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// State file (hand-rolled key=value; no TOML parser dep)

fn write_state_file(masked_name: &str) -> Result<()> {
    let path = Path::new(STATE_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LiftError::Command {
            step: "boot-mode state file",
            detail: format!("mkdir {}: {e}", parent.display()),
        })?;
    }
    let ts = current_timestamp_iso8601();
    let s = format!(
        "masked_display_manager = \"{masked_name}\"\nenabled_at = \"{ts}\"\n");
    std::fs::write(path, s).map_err(|e| LiftError::Command {
        step: "boot-mode state file",
        detail: format!("write {}: {e}", path.display()),
    })?;
    eprintln!("    [boot-mode] state recorded at {}", path.display());
    Ok(())
}

/// Read `masked_display_manager` out of the state file. Returns None if
/// the file doesn't exist, can't be read, or doesn't have the expected
/// shape — caller treats all three the same.
fn read_state_file(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let line = content.lines().find(|l| l.starts_with("masked_display_manager"))?;
    let val = line.split('"').nth(1)?;
    if val.is_empty() { None } else { Some(val.to_string()) }
}

/// Best-effort ISO-8601-ish UTC timestamp. We don't pull chrono for one
/// cosmetic field — `time_t` plus a hand-rolled break-down gets us the
/// shape we need ("2026-05-05T12:34:56Z") to within a second.
fn current_timestamp_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_seconds(now)
}

/// Civil-date breakdown of a unix timestamp using the standard
/// proleptic-Gregorian algorithm. Pulled out so it's unit-testable.
fn format_unix_seconds(secs: u64) -> String {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86_400;
    let (y, mo, d) = days_to_civil(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's days-from-civil inverse — converts days since
/// 1970-01-01 to (year, month, day). Cheap, exact, no leap-year tables.
fn days_to_civil(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146_096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

// ---------------------------------------------------------------------------
// systemctl daemon-reload

fn daemon_reload(dry_run: bool) -> Result<()> {
    if dry_run {
        eprintln!("    [boot-mode] would: systemctl daemon-reload");
        return Ok(());
    }
    let out = Command::new("systemctl").arg("daemon-reload").output()
        .map_err(|e| LiftError::Command {
            step: "boot-mode daemon-reload",
            detail: format!("spawn systemctl: {e}"),
        })?;
    if !out.status.success() {
        return Err(LiftError::Command {
            step: "boot-mode daemon-reload",
            detail: format!("exit={} stderr={}",
                out.status, String::from_utf8_lossy(&out.stderr)),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
//
// Coverage rule: only the parts that don't need root + systemd. The
// systemctl branches are tested by running the tool on a real host.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_boot_mode_returns_desktop_when_override_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.conf");
        assert_eq!(current_boot_mode_at(&path), BootMode::Desktop);
    }

    #[test]
    fn current_boot_mode_returns_cockpit_when_override_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00-rotten-apple-cockpit.conf");
        std::fs::write(&path, OVERRIDE_BODY).unwrap();
        assert_eq!(current_boot_mode_at(&path), BootMode::Cockpit);
    }

    #[test]
    fn state_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let s = format!(
            "masked_display_manager = \"{}\"\nenabled_at = \"{}\"\n",
            "gdm.service", "2026-05-05T12:34:56Z");
        std::fs::write(&path, s).unwrap();
        let got = read_state_file(&path);
        assert_eq!(got.as_deref(), Some("gdm.service"));
    }

    #[test]
    fn state_file_robust_to_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert!(read_state_file(&path).is_none());
    }

    #[test]
    fn state_file_robust_to_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.toml");
        std::fs::write(&path, "not even kinda toml\n!!! garbage\n").unwrap();
        assert!(read_state_file(&path).is_none());
    }

    #[test]
    fn override_body_includes_cockpit_invocation() {
        // Drift guard — the override has to exec the cockpit subcommand
        // we install at /usr/local/bin/rotten-apple. Renaming either
        // side without the other reboots into a broken getty.
        assert!(OVERRIDE_BODY.contains("/usr/local/bin/rotten-apple cockpit"));
        assert!(OVERRIDE_BODY.contains("ExecStart=\n"),
            "must have a clearing ExecStart= line before the new value");
    }

    #[test]
    fn override_body_puts_start_limit_in_unit_section() {
        let unit_idx = OVERRIDE_BODY.find("[Unit]").unwrap();
        let service_idx = OVERRIDE_BODY.find("[Service]").unwrap();
        let burst_idx = OVERRIDE_BODY.find("StartLimitBurst=3").unwrap();
        let interval_idx = OVERRIDE_BODY.find("StartLimitIntervalSec=60").unwrap();
        assert!(unit_idx < burst_idx && burst_idx < service_idx);
        assert!(unit_idx < interval_idx && interval_idx < service_idx);
    }

    #[test]
    fn boot_mode_label_is_stable() {
        // The CLI status command and the cockpit modal both render
        // these labels — keep them pinned.
        assert_eq!(BootMode::Desktop.label(), "Desktop");
        assert_eq!(BootMode::Cockpit.label(), "Cockpit");
    }

    #[test]
    fn timestamp_format_shape() {
        // Pin the shape, not the value (clock changes every second).
        let s = format_unix_seconds(1_762_345_096);
        assert_eq!(s.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ shape");
        assert!(s.ends_with('Z'));
        assert!(s.chars().nth(4) == Some('-'));
        assert!(s.chars().nth(10) == Some('T'));
    }
}
