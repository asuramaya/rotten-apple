//! rotten-apple image catalog.
//!
//! Step 1 of the "spawn fresh Linux instances quickly" pipeline: keep a
//! local catalog of upstream cloud images. This crate does three things:
//!
//!   1. defines the on-disk catalog (`/var/lib/rotten-apple/images/index.toml`)
//!      with `Catalog` + `ImageEntry` (TOML, serde-driven),
//!   2. ships a hard-coded registry of known sources (distro shorthand →
//!      URL + sha256), keyed by names like "ubuntu:24.04",
//!   3. drives `pull(name)` — curl the file into the cache dir, verify
//!      the sha256 if pinned, hand back an `ImageEntry` for the caller
//!      to register.
//!
//! No image creation here. That's `crates/instance` (separate agent).
//! No tokio, no reqwest — `curl` subprocess + std + sha2.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default catalog file. Operators can pass another path explicitly.
pub const DEFAULT_CATALOG_PATH: &str = "/var/lib/rotten-apple/images/index.toml";

/// One row in the catalog. Mirrors the [[image]] table-array in
/// index.toml. `pulled_at` is an RFC-3339 string; we don't pull in
/// chrono just to format a timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageEntry {
    pub name:       String,
    pub backing:    PathBuf,
    pub format:     String,
    pub distro:     String,
    pub version:    String,
    pub arch:       String,
    pub sha256:     String,
    pub source:     String,
    pub size_bytes: u64,
    pub pulled_at:  String,
}

/// The catalog file as a whole. `[[image]]` table-array under the key
/// `image`; serde renames the field so the TOML stays human-friendly.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default, rename = "image")]
    pub images: Vec<ImageEntry>,
}

impl Catalog {
    /// Load the catalog at `path`. Missing file or empty file → empty
    /// catalog (first-run friendly). Malformed file → empty catalog
    /// with a stderr warning; operators can recover by `image rm`-ing
    /// or hand-editing.
    pub fn load_or_empty(path: &Path) -> Self {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Catalog::default(),
            Err(e) => {
                eprintln!("catalog: read {}: {e} (treating as empty)", path.display());
                return Catalog::default();
            }
        };
        if raw.trim().is_empty() { return Catalog::default(); }
        match toml::from_str::<Catalog>(&raw) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("catalog: parse {}: {e} (treating as empty)", path.display());
                Catalog::default()
            }
        }
    }

    /// Serialise + atomic-ish write. Creates parent dirs as needed.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, body)
    }

    pub fn find(&self, name: &str) -> Option<&ImageEntry> {
        self.images.iter().find(|e| e.name == name)
    }

    /// Insert or replace by `name`. Newest wins.
    pub fn upsert(&mut self, entry: ImageEntry) {
        if let Some(slot) = self.images.iter_mut().find(|e| e.name == entry.name) {
            *slot = entry;
        } else {
            self.images.push(entry);
        }
    }

    /// Remove by name. Returns true if an entry was actually removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.images.len();
        self.images.retain(|e| e.name != name);
        self.images.len() != before
    }
}

/// One supported upstream image. Pre-baked at compile time so the CLI
/// works offline for `image list-known` and so the sha256 is pinned in
/// source review (when we have one).
///
/// TODO: operators should pin every sha256 before production — empty
/// strings here mean "skip verification", which is fine for getting the
/// pipe wired but unsafe long-term.
#[derive(Debug, Clone, Copy)]
pub struct SourceSpec {
    pub name:    &'static str,
    pub distro:  &'static str,
    pub version: &'static str,
    pub arch:    &'static str,
    pub url:     &'static str,
    pub sha256:  &'static str,
    pub format:  &'static str,
}

// Hard-coded source registry. sha256s left empty for now — upstream
// rebuilds these images without notice, so we'd ship stale hashes. The
// pull path skips verification when the spec's sha256 is empty; flip on
// verification by editing this table when an operator wants a pinned
// release.
//
// TODO: pin sha256s for production. The current SHA256SUMS files are at:
//   https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS
//   https://cloud-images.ubuntu.com/releases/22.04/release/SHA256SUMS
//   https://cloud.debian.org/images/cloud/bookworm/latest/SHA256SUMS
const SOURCES: &[SourceSpec] = &[
    SourceSpec {
        name:    "ubuntu:24.04",
        distro:  "ubuntu",
        version: "24.04",
        arch:    "amd64",
        url:     "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img",
        sha256:  "",
        format:  "qcow2",
    },
    SourceSpec {
        name:    "ubuntu:22.04",
        distro:  "ubuntu",
        version: "22.04",
        arch:    "amd64",
        url:     "https://cloud-images.ubuntu.com/releases/22.04/release/ubuntu-22.04-server-cloudimg-amd64.img",
        sha256:  "",
        format:  "qcow2",
    },
    SourceSpec {
        name:    "debian:12",
        distro:  "debian",
        version: "12",
        arch:    "amd64",
        url:     "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2",
        sha256:  "",
        format:  "qcow2",
    },
];

/// All supported shorthand → upstream specs.
pub fn known_sources() -> &'static [SourceSpec] { SOURCES }

/// Errors from `pull()`. Display impl is short and operator-friendly.
#[derive(Debug)]
pub enum PullError {
    UnknownSource(String),
    Mkdir(PathBuf, io::Error),
    CurlSpawn(io::Error),
    CurlExit(i32),
    Hash(io::Error),
    ChecksumMismatch { expected: String, actual: String },
    Stat(io::Error),
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullError::UnknownSource(n) => write!(f, "unknown source: {n} (try `image list-known`)"),
            PullError::Mkdir(p, e)      => write!(f, "mkdir -p {}: {e}", p.display()),
            PullError::CurlSpawn(e)     => write!(f, "spawn curl: {e} (is curl installed?)"),
            PullError::CurlExit(c)      => write!(f, "curl exited {c}"),
            PullError::Hash(e)          => write!(f, "hash: {e}"),
            PullError::ChecksumMismatch { expected, actual } =>
                write!(f, "sha256 mismatch: expected {expected}, got {actual}"),
            PullError::Stat(e)          => write!(f, "stat downloaded file: {e}"),
        }
    }
}

impl std::error::Error for PullError {}

/// Resolve `name` against `known_sources()`, then download the image
/// into `dest_dir` and produce an `ImageEntry`. The caller owns the
/// catalog — we don't write it here. `dry_run` prints the curl command
/// and returns a synthetic entry so the caller can show what would land.
pub fn pull(name: &str, dest_dir: &Path, dry_run: bool) -> Result<ImageEntry, PullError> {
    let spec = known_sources().iter().find(|s| s.name == name)
        .ok_or_else(|| PullError::UnknownSource(name.to_string()))?;

    fs::create_dir_all(dest_dir)
        .map_err(|e| PullError::Mkdir(dest_dir.to_path_buf(), e))?;

    let filename = url_basename(spec.url);
    let dst = dest_dir.join(filename);

    if dry_run {
        println!("[dry-run] curl -fsSL --retry 3 -o {} {}", dst.display(), spec.url);
        return Ok(synth_entry(spec, &dst, 0));
    }

    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&dst)
        .arg(spec.url)
        .status()
        .map_err(PullError::CurlSpawn)?;
    if !status.success() {
        return Err(PullError::CurlExit(status.code().unwrap_or(-1)));
    }

    if !spec.sha256.is_empty() {
        let actual = hash_file(&dst).map_err(PullError::Hash)?;
        if !actual.eq_ignore_ascii_case(spec.sha256) {
            return Err(PullError::ChecksumMismatch {
                expected: spec.sha256.to_string(),
                actual,
            });
        }
    }

    let size_bytes = fs::metadata(&dst).map_err(PullError::Stat)?.len();
    Ok(synth_entry(spec, &dst, size_bytes))
}

fn synth_entry(spec: &SourceSpec, dst: &Path, size_bytes: u64) -> ImageEntry {
    ImageEntry {
        name:       spec.name.to_string(),
        backing:    dst.to_path_buf(),
        format:     spec.format.to_string(),
        distro:     spec.distro.to_string(),
        version:    spec.version.to_string(),
        arch:       spec.arch.to_string(),
        sha256:     spec.sha256.to_string(),
        source:     spec.url.to_string(),
        size_bytes,
        pulled_at:  now_rfc3339(),
    }
}

/// Last path component of a URL, sans query string. We don't want a
/// real URL parser for one job; cloud-image URLs don't have queries.
fn url_basename(url: &str) -> &str {
    let no_query = url.split('?').next().unwrap_or(url);
    no_query.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("image.bin")
}

/// SHA-256 of a file, lowercase hex. Streamed — we don't know image
/// sizes ahead of time and a 1 GiB qcow2 in RAM is rude.
fn hash_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut f, &mut hasher)?;
    let digest = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in digest { s.push_str(&format!("{b:02x}")); }
    Ok(s)
}

/// Best-effort RFC-3339 UTC timestamp without pulling chrono. Uses
/// libc's `time` + `gmtime_r`. If the syscall fails we return "unknown"
/// — the catalog still parses, the operator just loses the audit trail
/// for that one row.
fn now_rfc3339() -> String {
    // SAFETY: time(NULL) and gmtime_r are signal-safe, thread-safe wrt
    // the buffer the caller owns; we pass a stack tm.
    unsafe {
        let t = libc_time();
        if t < 0 { return "unknown".into(); }
        let mut tm: TmShim = std::mem::zeroed();
        if gmtime_r_shim(&t, &mut tm).is_null() { return "unknown".into(); }
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
            tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

// Minimal libc shims, kept private so we don't add a `libc` dep just
// for a timestamp. This crate's Cargo.toml deliberately avoids libc.
#[repr(C)]
struct TmShim {
    tm_sec:   i32, tm_min:  i32, tm_hour: i32,
    tm_mday:  i32, tm_mon:  i32, tm_year: i32,
    tm_wday:  i32, tm_yday: i32, tm_isdst: i32,
    tm_gmtoff: i64, tm_zone: *const i8,
}

unsafe extern "C" {
    #[link_name = "time"]
    fn libc_time_inner(tloc: *mut i64) -> i64;
    #[link_name = "gmtime_r"]
    fn gmtime_r_inner(timep: *const i64, result: *mut TmShim) -> *mut TmShim;
}

unsafe fn libc_time() -> i64 { unsafe { libc_time_inner(std::ptr::null_mut()) } }
unsafe fn gmtime_r_shim(t: *const i64, tm: *mut TmShim) -> *mut TmShim {
    unsafe { gmtime_r_inner(t, tm) }
}

// ---------------------------------------------------------------------
// tests

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(name: &str) -> ImageEntry {
        ImageEntry {
            name:       name.into(),
            backing:    PathBuf::from(format!("/var/lib/rotten-apple/images/{name}.qcow2")),
            format:     "qcow2".into(),
            distro:     "ubuntu".into(),
            version:    "24.04".into(),
            arch:       "amd64".into(),
            sha256:     "abc123".into(),
            source:     "https://example.com/img".into(),
            size_bytes: 614_400_000,
            pulled_at:  "2026-05-05T16:00:00Z".into(),
        }
    }

    #[test]
    fn catalog_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.toml");
        let mut c = Catalog::default();
        c.upsert(sample_entry("ubuntu-24.04"));
        c.save(&path).unwrap();

        let loaded = Catalog::load_or_empty(&path);
        assert_eq!(loaded.images.len(), 1);
        assert_eq!(loaded.images[0], c.images[0]);
    }

    #[test]
    fn catalog_load_or_empty_handles_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let c = Catalog::load_or_empty(&path);
        assert!(c.images.is_empty());
    }

    #[test]
    fn catalog_upsert_replaces_existing() {
        let mut c = Catalog::default();
        c.upsert(sample_entry("ubuntu-24.04"));
        let mut second = sample_entry("ubuntu-24.04");
        second.size_bytes = 999;
        c.upsert(second);
        assert_eq!(c.images.len(), 1);
        assert_eq!(c.images[0].size_bytes, 999);
    }

    #[test]
    fn catalog_find_returns_entry() {
        let mut c = Catalog::default();
        c.upsert(sample_entry("ubuntu-24.04"));
        assert!(c.find("ubuntu-24.04").is_some());
        assert!(c.find("nope").is_none());
    }

    #[test]
    fn catalog_remove_returns_true_when_present() {
        let mut c = Catalog::default();
        c.upsert(sample_entry("ubuntu-24.04"));
        assert!(c.remove("ubuntu-24.04"));
        assert!(!c.remove("ubuntu-24.04"));
        assert!(c.images.is_empty());
    }

    #[test]
    fn known_sources_includes_ubuntu_24_04() {
        let names: Vec<&str> = known_sources().iter().map(|s| s.name).collect();
        assert!(names.contains(&"ubuntu:24.04"),
            "expected ubuntu:24.04 in known_sources, got {names:?}");
    }

    #[test]
    fn url_basename_extracts_filename() {
        assert_eq!(url_basename("https://example.com/path/to/foo.qcow2"), "foo.qcow2");
        assert_eq!(url_basename("https://example.com/foo.img?x=1"), "foo.img");
        assert_eq!(url_basename("https://example.com/"), "image.bin");
    }
}
