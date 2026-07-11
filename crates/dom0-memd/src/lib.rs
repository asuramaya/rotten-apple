//! dom0 memory governor — the dynamic-RAM policy for a ThinDom0's Domain-0.
//!
//! # Why this exists
//!
//! A ThinDom0 must boot with enough RAM to unpack the initramfs into a
//! tmpfs rootfs (the transient peak that historically bricked boots when set
//! too low — see `bootstrapper::thin_dom0::THINDOM0_DOM0_MEM_MB`). But that
//! boot-time size is *wasteful* at steady state: every MiB dom0 holds is a
//! MiB the user-desktop guest can't have. The doctrine
//! ([[feedback_dom0_sizing]]) is a 1 GiB steady-state dom0.
//!
//! So dom0 boots high and then this governor balloons it **down** to the
//! steady floor once the unpack memory is freed, and **up** again on demand
//! when guests or memory pressure need the headroom. The whole policy is the
//! pure [`decide_target`] function; everything else here is thin I/O around
//! `/proc/meminfo` and `xl`.
//!
//! # Policy (events + watermark floor)
//!
//! Each tick we compute a target from two independent inputs, and the more
//! demanding one wins:
//!
//!   * **Reservation floor (declarative "events"):** every running guest
//!     imposes dom0-side cost (blkback/netback grant tables, xenstore, and a
//!     qemu device model for HVM guests). We reserve `per_guest_mb` per guest
//!     on top of the steady floor. Declarative — recomputed from the live
//!     `xl list` each tick, so a missed create/destroy event can't leave the
//!     reservation stale.
//!   * **Watermark pressure:** if dom0's own `MemAvailable` drops below the
//!     low watermark we grow a step; if it rises comfortably above the high
//!     watermark we reclaim a step back toward the floor. A dead-band between
//!     the two, plus the minimum step, keeps it from thrashing.
//!
//! The target is always clamped to `[floor, max]` — the reservation floor and
//! the `max` ceiling both override the watermark, so a fresh guest forces an
//! immediate grow even with no pressure, and pressure can never push dom0
//! past the `dom0_mem=...,max:` ceiling the hypervisor was booted with.

use std::io;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

/// Tunable governor policy. All memory values are MiB.
///
/// Defaults mirror the ThinDom0 doctrine: `steady_mb` is the 1 GiB resting
/// floor, `max_mb` matches the `dom0_mem=...,max:` ceiling baked into the Xen
/// cmdline by `bootstrapper::thin_dom0` (keep them in sync — the governor
/// clamps to `max_mb`, but if it exceeds the hypervisor's ceiling the balloon
/// grow simply fails and is logged).
#[derive(Debug, Clone, Copy)]
pub struct GovernorConfig {
    /// Resting target; dom0 is never ballooned below this (plus reservation).
    pub steady_mb: u64,
    /// Hard ceiling; dom0 is never ballooned above this.
    pub max_mb: u64,
    /// dom0-side RAM reserved per running guest domain.
    pub per_guest_mb: u64,
    /// Grow when dom0 `MemAvailable` falls below this percent of `MemTotal`.
    pub low_watermark_pct: u64,
    /// Reclaim toward the floor when `MemAvailable` rises above this percent.
    pub high_watermark_pct: u64,
    /// Grow/shrink increment applied per adjustment.
    pub step_mb: u64,
    /// Ignore target moves smaller than this — the anti-thrash dead-band.
    pub dead_band_mb: u64,
    /// Poll/adjust interval.
    pub tick: Duration,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            steady_mb: 1024,
            max_mb: 4096,
            per_guest_mb: 256,
            low_watermark_pct: 12,
            high_watermark_pct: 30,
            step_mb: 256,
            dead_band_mb: 128,
            tick: Duration::from_secs(2),
        }
    }
}

/// One reading of dom0's own memory, in MiB. `total_mb` is dom0's *current*
/// allocation (Xen updates `MemTotal` as the balloon moves), so we treat it
/// as the current target — this makes the loop self-correcting even if
/// something outside the governor runs `xl mem-set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemSample {
    pub available_mb: u64,
    pub total_mb: u64,
}

impl MemSample {
    /// `MemAvailable` as a percent of `MemTotal`. Empty total reads as 100%
    /// (fully slack) so a bogus sample never triggers a spurious grow.
    pub fn available_pct(&self) -> u64 {
        if self.total_mb == 0 {
            100
        } else {
            self.available_mb.saturating_mul(100) / self.total_mb
        }
    }
}

/// The entire policy, as a pure function so it can be exhaustively tested
/// without touching `/proc` or `xl`. Returns the next dom0 memory target in
/// MiB, or `None` to hold (the move would be within the dead-band).
pub fn decide_target(
    cfg: &GovernorConfig,
    sample: MemSample,
    guests: u64,
    current_mb: u64,
) -> Option<u64> {
    let reserve = guests.saturating_mul(cfg.per_guest_mb);
    // The floor is steady + reservation, but can never exceed the ceiling.
    let floor = cfg.steady_mb.saturating_add(reserve).min(cfg.max_mb);
    let pct = sample.available_pct();

    // Watermark nudge around the current size.
    let mut desired = current_mb;
    if pct < cfg.low_watermark_pct {
        desired = current_mb.saturating_add(cfg.step_mb);
    } else if pct > cfg.high_watermark_pct {
        desired = current_mb.saturating_sub(cfg.step_mb);
    }

    // Floor + ceiling always win over the watermark: this is what forces an
    // immediate grow when a new guest lifts the reservation floor above the
    // current size, and what caps pressure-driven growth at the ceiling.
    let desired = desired.clamp(floor, cfg.max_mb);

    if desired.abs_diff(current_mb) < cfg.dead_band_mb {
        None
    } else {
        Some(desired)
    }
}

/// Parse `/proc/meminfo` text into a [`MemSample`] (kB fields → MiB). Returns
/// `None` if either `MemTotal` or `MemAvailable` is absent/unparseable.
pub fn parse_meminfo(s: &str) -> Option<MemSample> {
    let mut total = None;
    let mut avail = None;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb_field(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb_field(v);
        }
    }
    Some(MemSample {
        total_mb: total?,
        available_mb: avail?,
    })
}

/// Parse a `/proc/meminfo` value field like `"  4096000 kB"` → MiB.
fn parse_kb_field(v: &str) -> Option<u64> {
    v.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kb| kb / 1024)
}

/// Count guest domains (everything except Domain-0) from `xl list` stdout.
/// Columns are `Name  ID  Mem  VCPUs  State  Time(s)`; Domain-0 is ID 0. A
/// line whose second column isn't a number (the header, blank lines) is
/// skipped, so a malformed/empty capture yields 0 guests — the safe default
/// (floor collapses to steady; the watermark still protects dom0).
pub fn parse_guest_count(xl_list_stdout: &str) -> u64 {
    xl_list_stdout
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter_map(|id| id.parse::<u64>().ok())
        .filter(|&id| id != 0)
        .count() as u64
}

// --------------------------------------------------------------------------
// I/O wrappers — the only impure surface.

fn read_sample() -> io::Result<MemSample> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    parse_meminfo(&text)
        .ok_or_else(|| io::Error::other("meminfo missing MemTotal/MemAvailable"))
}

fn read_guest_count() -> u64 {
    match Command::new("xl").arg("list").output() {
        Ok(out) if out.status.success() => {
            parse_guest_count(&String::from_utf8_lossy(&out.stdout))
        }
        // `xl list` failing (control plane not up yet) → assume no guests.
        _ => 0,
    }
}

/// Set Domain-0's balloon target. `xl mem-set <domid> <n>` takes MiB when the
/// value is unsuffixed — this is the same target the libxl backend sets via
/// `libxl_set_memory_target`, just driven through the tool so this crate
/// carries no libxl linkage.
fn set_dom0_target(target_mb: u64) -> io::Result<()> {
    let status = Command::new("xl")
        .args(["mem-set", "0", &target_mb.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "xl mem-set 0 {target_mb} exited {status}"
        )))
    }
}

/// Run the governor loop forever. Each tick: sample dom0 memory, census the
/// guests, decide, and apply. Never returns under normal operation; a fatal
/// I/O error reading `/proc/meminfo` propagates so the caller can log + exit
/// (init respawns nothing here — a dead governor just means dom0 holds its
/// last size, which is safe).
pub fn run(cfg: GovernorConfig) -> io::Result<()> {
    eprintln!(
        "[dom0-memd] governor up: steady={} max={} per_guest={} low={}% high={}% step={} tick={:?}",
        cfg.steady_mb, cfg.max_mb, cfg.per_guest_mb,
        cfg.low_watermark_pct, cfg.high_watermark_pct, cfg.step_mb, cfg.tick,
    );
    loop {
        let sample = read_sample()?;
        let guests = read_guest_count();
        let current = sample.total_mb;
        if let Some(target) = decide_target(&cfg, sample, guests, current) {
            let dir = if target > current { "grow" } else { "shrink" };
            match set_dom0_target(target) {
                Ok(()) => eprintln!(
                    "[dom0-memd] {dir} {current}->{target} MiB (avail {}%, {guests} guest(s))",
                    sample.available_pct(),
                ),
                Err(e) => eprintln!("[dom0-memd] set target {target} failed: {e}"),
            }
        }
        sleep(cfg.tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GovernorConfig {
        GovernorConfig::default()
    }

    fn sample(available_mb: u64, total_mb: u64) -> MemSample {
        MemSample { available_mb, total_mb }
    }

    #[test]
    fn boot_high_reclaims_toward_steady_when_slack() {
        // dom0 booted at 1536 with tons free (>30% avail) → shrink a step.
        let c = cfg();
        let s = sample(1400, 1536); // ~91% available
        assert_eq!(decide_target(&c, s, 0, 1536), Some(1536 - c.step_mb));
    }

    #[test]
    fn reclaim_stops_at_steady_floor() {
        let c = cfg();
        // At 1152 with slack, one step down would be 896 but the floor is 1024.
        let s = sample(1000, 1152);
        assert_eq!(decide_target(&c, s, 0, 1152), Some(1024));
        // Already at the floor with slack → nothing to do.
        let s2 = sample(950, 1024);
        assert_eq!(decide_target(&c, s2, 0, 1024), None);
    }

    #[test]
    fn pressure_grows_a_step() {
        let c = cfg();
        // 5% available (< 12% low) → grow.
        let s = sample(51, 1024);
        assert_eq!(decide_target(&c, s, 0, 1024), Some(1024 + c.step_mb));
    }

    #[test]
    fn pressure_growth_capped_at_max() {
        let c = cfg();
        let s = sample(100, 4096); // low avail at the ceiling
        assert_eq!(decide_target(&c, s, 0, 4096), None); // already max, can't grow
        let s2 = sample(100, 3968);
        assert_eq!(decide_target(&c, s2, 0, 3968), Some(4096)); // grow clamps to max
    }

    #[test]
    fn new_guest_forces_grow_to_reservation_floor_without_pressure() {
        let c = cfg();
        // Mid-band availability (no watermark trigger), but 2 guests lift the
        // floor to 1024 + 2*256 = 1536, above the current 1024 → forced grow.
        let s = sample(200, 1024); // ~19% — between 12 and 30
        assert_eq!(decide_target(&c, s, 2, 1024), Some(1536));
    }

    #[test]
    fn guest_reservation_never_exceeds_ceiling() {
        let c = cfg();
        // 20 guests would want 1024 + 20*256, but the ceiling is 4096.
        let s = sample(100, 1024);
        assert_eq!(decide_target(&c, s, 20, 1024), Some(4096));
    }

    #[test]
    fn dead_band_holds_small_moves() {
        let mut c = cfg();
        c.step_mb = 64; // smaller than the 128 dead-band
        let s = sample(1000, 1152); // slack → would shrink 64, below dead-band
        assert_eq!(decide_target(&c, s, 0, 1152), None);
    }

    #[test]
    fn mid_band_holds_when_above_floor() {
        let c = cfg();
        // Comfortable size, mid-band availability, no guests → hold, don't
        // reclaim memory that's in use.
        let s = sample(300, 1536); // ~19%
        assert_eq!(decide_target(&c, s, 0, 1536), None);
    }

    #[test]
    fn meminfo_parses_kb_to_mib() {
        let text = "MemTotal:        1048576 kB\nMemFree:  10 kB\nMemAvailable:  524288 kB\n";
        assert_eq!(parse_meminfo(text), Some(sample(512, 1024)));
    }

    #[test]
    fn meminfo_missing_field_is_none() {
        assert_eq!(parse_meminfo("MemTotal: 1048576 kB\n"), None);
    }

    #[test]
    fn guest_count_skips_dom0_and_header() {
        let xl = "\
Name                                        ID   Mem VCPUs      State   Time(s)
Domain-0                                     0  1024     1     r-----      12.3
ubuntu-desktop                               3  4096     4     -b----       4.5
scratch                                      7   512     1     -b----       0.1
";
        assert_eq!(parse_guest_count(xl), 2);
    }

    #[test]
    fn guest_count_empty_is_zero() {
        assert_eq!(parse_guest_count(""), 0);
        assert_eq!(parse_guest_count("Name ID Mem VCPUs State Time(s)\n"), 0);
    }

    #[test]
    fn available_pct_zero_total_is_slack() {
        assert_eq!(sample(0, 0).available_pct(), 100);
    }
}
