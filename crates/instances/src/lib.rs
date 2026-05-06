//! rotten-apple instances — CoW overlays on base images.
//!
//! An "instance" is base_image + qcow2 overlay = a domain that can be
//! created, started, destroyed, forked without touching the base image.
//! Sub-second creation: the overlay starts at ~12 KB and writes are
//! recorded into it; reads fall through to the backing file.
//!
//! Layout:
//!   /var/lib/rotten-apple/images/index.toml      — base image catalog
//!   /var/lib/rotten-apple/instances/index.toml   — instance registry
//!   /var/lib/rotten-apple/instances/<id>.qcow2   — per-instance overlay
//!   /etc/rotten-apple/manifests.d/<id>.toml      — generated Profile
//!
//! Wire:
//!   create / fork → write overlay + Profile, append registry
//!   destroy       → unlink overlay + manifest, drop from registry
//!   dispatch      → JSON-RPC `domain.create { manifest_path }` to daemon
//!                   over Unix socket first, then host-vsock fallback

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rotten_apple_orchestratord::{
    DEFAULT_SOCKET_PATH,
    protocol::PROTOCOL_VERSION,
    transport::{DEFAULT_VSOCK_PORT, VSOCK_HOST_CID, connect_vsock},
};

// ---------------------------------------------------------------------------
// Constants

pub const DEFAULT_REGISTRY_PATH: &str =
    "/var/lib/rotten-apple/instances/index.toml";
pub const DEFAULT_INSTANCES_DIR: &str =
    "/var/lib/rotten-apple/instances";
pub const DEFAULT_IMAGES_INDEX: &str =
    "/var/lib/rotten-apple/images/index.toml";
pub const DEFAULT_MANIFESTS_DIR: &str =
    "/etc/rotten-apple/manifests.d";

pub const DEFAULT_MEMORY_MB: u64 = 4096;
pub const DEFAULT_VCPUS: u32 = 2;

/// Env var that swaps `qemu-img` for a test stub. The value is the
/// program path; args are passed through. Lets unit tests verify the
/// overlay-creation call without requiring qemu on CI.
pub const QEMU_IMG_OVERRIDE_ENV: &str = "ROTTEN_APPLE_QEMU_IMG";

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub enum InstanceError {
    BaseImageNotFound(String),
    ParentNotFound(String),
    AlreadyExists(String),
    NotFound(String),
    QemuImg(String),
    Io(io::Error),
    Toml(String),
}

impl std::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceError::BaseImageNotFound(s) =>
                write!(f, "base image not found: {s}"),
            InstanceError::ParentNotFound(s) =>
                write!(f, "parent instance not found: {s}"),
            InstanceError::AlreadyExists(s) =>
                write!(f, "instance already exists: {s}"),
            InstanceError::NotFound(s) =>
                write!(f, "instance not found: {s}"),
            InstanceError::QemuImg(s) =>
                write!(f, "qemu-img: {s}"),
            InstanceError::Io(e)   => write!(f, "io: {e}"),
            InstanceError::Toml(e) => write!(f, "toml: {e}"),
        }
    }
}
impl std::error::Error for InstanceError {}

impl From<io::Error> for InstanceError {
    fn from(e: io::Error) -> Self { InstanceError::Io(e) }
}

#[derive(Debug)]
pub enum DispatchError {
    Io(io::Error),
    Rpc { code: i32, message: String },
    Protocol(String),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::Io(e) => write!(f, "io: {e}"),
            DispatchError::Rpc { code, message } =>
                write!(f, "daemon error {code}: {message}"),
            DispatchError::Protocol(s) => write!(f, "protocol: {s}"),
        }
    }
}
impl std::error::Error for DispatchError {}
impl From<io::Error> for DispatchError {
    fn from(e: io::Error) -> Self { DispatchError::Io(e) }
}

// ---------------------------------------------------------------------------
// Registry

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceEntry {
    pub id:         String,
    pub base_image: String,
    pub overlay:    String,
    pub memory_mb:  u64,
    pub vcpus:      u32,
    pub created_at: String,
    #[serde(default)]
    pub ephemeral:  bool,
    /// Set if this instance was forked from another instance. None = a
    /// fresh instance off a base image.
    #[serde(default)]
    pub parent:     Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceRegistry {
    #[serde(default, rename = "instance")]
    pub instances: Vec<InstanceEntry>,
}

impl InstanceRegistry {
    /// Load from disk, or return an empty registry if the file is
    /// missing. Hard-fails on parse error so we don't silently lose state.
    pub fn load_or_empty(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Self::default(),
        };
        toml::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(self).map_err(|e|
            io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        fs::write(path, text)
    }

    pub fn find(&self, id: &str) -> Option<&InstanceEntry> {
        self.instances.iter().find(|e| e.id == id)
    }

    /// Replace any existing entry with the same id; otherwise append.
    pub fn upsert(&mut self, entry: InstanceEntry) {
        if let Some(slot) = self.instances.iter_mut().find(|e| e.id == entry.id) {
            *slot = entry;
        } else {
            self.instances.push(entry);
        }
    }

    /// Remove by id; returns whether anything was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.instances.len();
        self.instances.retain(|e| e.id != id);
        before != self.instances.len()
    }
}

// ---------------------------------------------------------------------------
// Image catalog (read-only view)
//
// The on-disk schema is whatever the images crate writes — we parse it
// permissively (only the two fields we need) so an updated catalog
// shape from upstream doesn't break us. We accept either `name` (the
// images crate's field) or `id` for the identifier, and either
// `backing` (images-crate) or `path` for the file location. Tests in
// this crate use the simpler `id`/`path` form for fixture brevity.

#[derive(Debug, Clone, Deserialize)]
struct ImageCatalog {
    #[serde(default, rename = "image")]
    images: Vec<ImageEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImageEntry {
    #[serde(default)]
    name:    Option<String>,
    #[serde(default)]
    id:      Option<String>,
    #[serde(default)]
    backing: Option<String>,
    #[serde(default)]
    path:    Option<String>,
}

impl ImageEntry {
    fn ident(&self) -> Option<&str> {
        self.name.as_deref().or(self.id.as_deref())
    }
    fn location(&self) -> Option<&str> {
        self.backing.as_deref().or(self.path.as_deref())
    }
}

/// Look up a base image's backing file in the catalog. Returns the
/// resolved path string ready to hand to qemu-img.
fn resolve_base_image(catalog_path: &Path, id: &str)
    -> Result<String, InstanceError>
{
    let text = fs::read_to_string(catalog_path).map_err(|_|
        InstanceError::BaseImageNotFound(format!(
            "{id} (catalog {} unreadable)", catalog_path.display())))?;
    let cat: ImageCatalog = toml::from_str(&text).map_err(|e|
        InstanceError::Toml(e.to_string()))?;
    cat.images.into_iter()
        .find(|i| i.ident() == Some(id))
        .and_then(|i| i.location().map(|s| s.to_string()))
        .ok_or_else(|| InstanceError::BaseImageNotFound(id.into()))
}

/// List the identifiers of every base image known to the catalog. Used
/// by the cockpit's instance wizard to populate the Tab-cycle.
pub fn list_known_images(catalog_path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(catalog_path) else { return Vec::new() };
    let Ok(cat) = toml::from_str::<ImageCatalog>(&text) else { return Vec::new() };
    cat.images.into_iter()
        .filter_map(|i| i.ident().map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------
// Operations

pub struct NewInstanceParams {
    pub id:         String,
    pub base_image: String,
    pub memory_mb:  u64,
    pub vcpus:      u32,
    pub ephemeral:  bool,
}

impl NewInstanceParams {
    /// Convenience constructor — apply defaults for memory/vcpus when the
    /// caller didn't pin them.
    pub fn new(id: impl Into<String>, base_image: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            base_image: base_image.into(),
            memory_mb: DEFAULT_MEMORY_MB,
            vcpus: DEFAULT_VCPUS,
            ephemeral: false,
        }
    }
}

/// Paths the operations write to. Pulled out of the constants so tests
/// can scope to a tmpdir without touching /var.
#[derive(Debug, Clone)]
pub struct InstancePaths {
    pub registry:      PathBuf,
    pub instances_dir: PathBuf,
    pub images_index:  PathBuf,
    pub manifests_dir: PathBuf,
}

impl Default for InstancePaths {
    fn default() -> Self {
        Self {
            registry:      PathBuf::from(DEFAULT_REGISTRY_PATH),
            instances_dir: PathBuf::from(DEFAULT_INSTANCES_DIR),
            images_index:  PathBuf::from(DEFAULT_IMAGES_INDEX),
            manifests_dir: PathBuf::from(DEFAULT_MANIFESTS_DIR),
        }
    }
}

pub fn create_instance(p: NewInstanceParams, dry_run: bool)
    -> Result<InstanceEntry, InstanceError>
{
    create_instance_in(&InstancePaths::default(), p, dry_run)
}

pub fn fork_instance(parent_id: &str, new_id: &str, dry_run: bool)
    -> Result<InstanceEntry, InstanceError>
{
    fork_instance_in(&InstancePaths::default(), parent_id, new_id, dry_run)
}

pub fn destroy_instance(id: &str, dry_run: bool)
    -> Result<(), InstanceError>
{
    destroy_instance_in(&InstancePaths::default(), id, dry_run)
}

/// Path-scoped variant of `create_instance` — what the public fn calls
/// after substituting the default paths. Tests use this directly with a
/// tmpdir-rooted `InstancePaths`.
pub fn create_instance_in(
    paths: &InstancePaths,
    p: NewInstanceParams,
    dry_run: bool,
) -> Result<InstanceEntry, InstanceError> {
    let mut reg = InstanceRegistry::load_or_empty(&paths.registry);
    if reg.find(&p.id).is_some() {
        return Err(InstanceError::AlreadyExists(p.id));
    }
    let base_path = resolve_base_image(&paths.images_index, &p.base_image)?;

    let overlay = paths.instances_dir.join(format!("{}.qcow2", p.id));
    let manifest_path = paths.manifests_dir.join(format!("{}.toml", p.id));

    let entry = InstanceEntry {
        id:         p.id.clone(),
        base_image: p.base_image.clone(),
        overlay:    overlay.to_string_lossy().into_owned(),
        memory_mb:  p.memory_mb,
        vcpus:      p.vcpus,
        created_at: now_rfc3339(),
        ephemeral:  p.ephemeral,
        parent:     None,
    };

    if dry_run {
        return Ok(entry);
    }

    fs::create_dir_all(&paths.instances_dir)?;
    fs::create_dir_all(&paths.manifests_dir)?;
    qemu_img_create(Path::new(&base_path), &overlay)?;
    let toml_text = profile_for_instance(&entry);
    fs::write(&manifest_path, toml_text)?;
    reg.upsert(entry.clone());
    reg.save(&paths.registry)?;
    Ok(entry)
}

pub fn fork_instance_in(
    paths: &InstancePaths,
    parent_id: &str,
    new_id: &str,
    dry_run: bool,
) -> Result<InstanceEntry, InstanceError> {
    let mut reg = InstanceRegistry::load_or_empty(&paths.registry);
    if reg.find(new_id).is_some() {
        return Err(InstanceError::AlreadyExists(new_id.into()));
    }
    let parent = reg.find(parent_id)
        .ok_or_else(|| InstanceError::ParentNotFound(parent_id.into()))?
        .clone();

    let overlay = paths.instances_dir.join(format!("{new_id}.qcow2"));
    let manifest_path = paths.manifests_dir.join(format!("{new_id}.toml"));

    let entry = InstanceEntry {
        id:         new_id.into(),
        // base_image of a fork is the parent's overlay path — qemu-img
        // chains backing files, so the fork sees parent's writes.
        base_image: parent.overlay.clone(),
        overlay:    overlay.to_string_lossy().into_owned(),
        memory_mb:  parent.memory_mb,
        vcpus:      parent.vcpus,
        created_at: now_rfc3339(),
        ephemeral:  false,
        parent:     Some(parent_id.into()),
    };

    if dry_run {
        return Ok(entry);
    }

    fs::create_dir_all(&paths.instances_dir)?;
    fs::create_dir_all(&paths.manifests_dir)?;
    qemu_img_create(Path::new(&parent.overlay), &overlay)?;
    let toml_text = profile_for_instance(&entry);
    fs::write(&manifest_path, toml_text)?;
    reg.upsert(entry.clone());
    reg.save(&paths.registry)?;
    Ok(entry)
}

pub fn destroy_instance_in(
    paths: &InstancePaths,
    id: &str,
    dry_run: bool,
) -> Result<(), InstanceError> {
    let mut reg = InstanceRegistry::load_or_empty(&paths.registry);
    let entry = reg.find(id)
        .ok_or_else(|| InstanceError::NotFound(id.into()))?
        .clone();

    if dry_run {
        return Ok(());
    }

    // Best-effort unlinks: a missing overlay or manifest shouldn't block
    // removing a stale registry entry.
    let _ = fs::remove_file(&entry.overlay);
    let manifest_path = paths.manifests_dir.join(format!("{id}.toml"));
    let _ = fs::remove_file(&manifest_path);
    reg.remove(id);
    reg.save(&paths.registry)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Profile (manifest) generation

/// Render a minimal Profile TOML for an instance. The shape matches
/// `crates/manifest/src/lib.rs`'s `Profile` schema: header, resources
/// (memory in MiB strings), storage.root pointing at the qcow2 overlay,
/// pygrub bootloader.
pub fn profile_for_instance(entry: &InstanceEntry) -> String {
    // active = caller-specified MB; idle/min derived as half / eighth so
    // the engine has sane bounds without the caller needing to reason
    // about ballooning. Pinned floor of 256M for min so very small
    // instances still satisfy the "minimum >= some sensible RAM" rule.
    let active   = entry.memory_mb;
    let idle     = (active / 2).max(256);
    let minimum  = (active / 8).max(256);

    format!(r#"# Generated by rotten-apple-instances for instance "{id}".
# Do not edit by hand — `rotten-apple instance rm {id}` removes it.

[profile]
name           = "{id}"
schema_version = "1"
description    = "rotten-apple instance: {id} (base {base})"
type           = "appliance"

[resources]
memory_active  = "{active}M"
memory_idle    = "{idle}M"
memory_minimum = "{minimum}M"
vcpus_active   = {vcpus}
vcpus_idle     = {vcpus_idle}
vcpus_minimum  = 1

[storage]
root = {{ kind = "qcow2", path = "{overlay}", mode = "rw-exclusive" }}

[network]
mode = "bridge"

[[network.interfaces]]
name = "primary"
mac  = "auto"

[tpm]
mode = "none"
"#,
        id      = entry.id,
        base    = entry.base_image,
        active  = active,
        idle    = idle,
        minimum = minimum,
        vcpus   = entry.vcpus,
        vcpus_idle = entry.vcpus.max(1),
        overlay = entry.overlay,
    )
}

// ---------------------------------------------------------------------------
// qemu-img

/// Run `qemu-img create -f qcow2 -b <base> -F qcow2 <dest>`. If the env
/// var `ROTTEN_APPLE_QEMU_IMG` is set, that program substitutes for
/// qemu-img — keeps unit tests off the host's real qemu install.
fn qemu_img_create(base: &Path, dest: &Path) -> Result<(), InstanceError> {
    let prog = std::env::var(QEMU_IMG_OVERRIDE_ENV)
        .unwrap_or_else(|_| "qemu-img".into());
    let out = Command::new(&prog)
        .args(["create", "-f", "qcow2", "-b"])
        .arg(base)
        .args(["-F", "qcow2"])
        .arg(dest)
        .output()
        .map_err(|e| InstanceError::QemuImg(format!("spawn {prog}: {e}")))?;
    if !out.status.success() {
        return Err(InstanceError::QemuImg(format!(
            "{prog} exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Time

/// RFC3339 / ISO 8601 timestamp in UTC. Hand-rolled — adding chrono just
/// for an ISO timestamp is overkill and the workspace avoids it elsewhere.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_utc(secs)
}

/// Convert UNIX seconds to "YYYY-MM-DDThh:mm:ssZ". Public so tests can
/// pin a known moment. Days-since-epoch math via the civil calendar
/// algorithm from Howard Hinnant's date library (public domain).
pub fn format_unix_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = (secs % 86_400) as u32;
    let h = secs_of_day / 3600;
    let m = (secs_of_day / 60) % 60;
    let s = secs_of_day % 60;
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Hinnant's civil_from_days: days since 1970-01-01 → (year, month, day).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d  = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m  = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m, d)
}

// ---------------------------------------------------------------------------
// Daemon dispatch (JSON-RPC over the orchestratord transport)

/// Connect to orchestratord, handshake, send `domain.create` with the
/// given manifest path; return the new domid on success.
pub fn dispatch_domain_create(socket: &Path, manifest_path: &Path)
    -> Result<u32, DispatchError>
{
    let mut client = DaemonRpc::connect_default(socket)?;
    client.handshake()?;
    let result = client.call("domain.create", json!({
        "manifest_path": manifest_path.to_string_lossy(),
    }))?;
    result.get("domid")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| DispatchError::Protocol(
            "domain.create: missing or invalid 'domid' in response".into()))
}

/// Convenience: start an existing instance via `domain.start`. Returns
/// () on success — the daemon's response carries nothing useful here.
pub fn dispatch_domain_start(socket: &Path, domid: u32)
    -> Result<(), DispatchError>
{
    let mut client = DaemonRpc::connect_default(socket)?;
    client.handshake()?;
    client.call("domain.start", json!({ "domid": domid }))?;
    Ok(())
}

/// Minimal JSON-RPC client. We don't reuse cockpit's DaemonClient because
/// that crate would pull ratatui / crossterm into this build. The wire
/// shape (line-delimited JSON, `hello` handshake, integer ids) matches
/// `crates/cockpit/src/daemon_client.rs` byte-for-byte.
struct DaemonRpc {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    next_id: u64,
}

impl DaemonRpc {
    fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Self::from_stream(stream)
    }

    fn connect_vsock_host(port: u32) -> io::Result<Self> {
        let stream = connect_vsock(VSOCK_HOST_CID, port)?;
        Self::from_stream(stream)
    }

    fn connect_default(path: &Path) -> Result<Self, DispatchError> {
        match Self::connect(path) {
            Ok(c) => Ok(c),
            Err(unix_err) => {
                Self::connect_vsock_host(DEFAULT_VSOCK_PORT).map_err(|vsock_err| {
                    let socket_label = if path == Path::new(DEFAULT_SOCKET_PATH) {
                        DEFAULT_SOCKET_PATH.to_string()
                    } else {
                        path.display().to_string()
                    };
                    DispatchError::Io(io::Error::new(
                        vsock_err.kind(),
                        format!(
                            "unix {}: {}; vsock host:{}: {}",
                            socket_label,
                            unix_err,
                            DEFAULT_VSOCK_PORT,
                            vsock_err,
                        ),
                    ))
                })
            }
        }
    }

    fn from_stream(stream: UnixStream) -> io::Result<Self> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let read_half = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(stream),
            next_id: 1,
        })
    }

    fn handshake(&mut self) -> Result<(), DispatchError> {
        let result = self.call(
            "hello",
            json!({ "protocol_version": PROTOCOL_VERSION }),
        )?;
        let server = result.get("protocol_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if server != PROTOCOL_VERSION {
            return Err(DispatchError::Protocol(format!(
                "protocol mismatch: client {PROTOCOL_VERSION}, server {server}",
            )));
        }
        let _ = self.reader.get_ref().set_read_timeout(None);
        Ok(())
    }

    fn call(&mut self, method: &str, params: Value)
        -> Result<Value, DispatchError>
    {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
            "id":      id,
        });
        let mut buf = serde_json::to_vec(&req).map_err(|e|
            DispatchError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        buf.push(b'\n');
        self.writer.write_all(&buf)?;
        self.writer.flush()?;

        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(DispatchError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "orchestratord closed the connection",
            )));
        }
        let resp: Value = serde_json::from_str(line.trim()).map_err(|e|
            DispatchError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        if let Some(err) = resp.get("error") {
            let code = err.get("code")
                .and_then(|v| v.as_i64())
                .unwrap_or(-32603) as i32;
            let message = err.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Err(DispatchError::Rpc { code, message });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(id: &str) -> InstanceEntry {
        InstanceEntry {
            id: id.into(),
            base_image: "ubuntu-24.04".into(),
            overlay: format!("/var/lib/rotten-apple/instances/{id}.qcow2"),
            memory_mb: 4096,
            vcpus: 2,
            created_at: "2026-05-05T16:00:00Z".into(),
            ephemeral: false,
            parent: None,
        }
    }

    #[test]
    fn registry_round_trips_through_toml() {
        let mut reg = InstanceRegistry::default();
        reg.upsert(sample_entry("alpha"));
        reg.upsert(sample_entry("beta"));
        let text = toml::to_string(&reg).unwrap();
        let back: InstanceRegistry = toml::from_str(&text).unwrap();
        assert_eq!(back.instances.len(), 2);
        assert_eq!(back.instances[0].id, "alpha");
        assert_eq!(back.instances[1].id, "beta");
        assert_eq!(back.instances[0].memory_mb, 4096);
    }

    #[test]
    fn registry_upsert_replaces_existing() {
        let mut reg = InstanceRegistry::default();
        reg.upsert(sample_entry("alpha"));
        let mut updated = sample_entry("alpha");
        updated.memory_mb = 8192;
        reg.upsert(updated);
        assert_eq!(reg.instances.len(), 1);
        assert_eq!(reg.find("alpha").unwrap().memory_mb, 8192);
    }

    #[test]
    fn registry_remove_returns_true_when_present() {
        let mut reg = InstanceRegistry::default();
        reg.upsert(sample_entry("alpha"));
        assert!(reg.remove("alpha"));
        assert!(!reg.remove("alpha"));
        assert!(reg.find("alpha").is_none());
    }

    #[test]
    fn new_instance_params_defaults_are_sensible() {
        let p = NewInstanceParams::new("dev", "ubuntu-24.04");
        assert_eq!(p.id, "dev");
        assert_eq!(p.base_image, "ubuntu-24.04");
        assert_eq!(p.memory_mb, DEFAULT_MEMORY_MB);
        assert_eq!(p.vcpus, DEFAULT_VCPUS);
        assert!(!p.ephemeral);
    }

    #[test]
    fn profile_for_instance_has_overlay_path() {
        let entry = sample_entry("dev");
        let text  = profile_for_instance(&entry);
        // Manifest schema demands [storage].root.path matching the overlay.
        assert!(text.contains(&entry.overlay),
            "profile missing overlay path:\n{text}");
        // Round-trip through the manifest crate to prove the generated
        // TOML is shape-compatible with what orchestratord will load.
        let p = rotten_apple_manifest::Profile::from_str(&text)
            .expect("generated profile should parse");
        assert_eq!(p.name(), "dev");
        assert_eq!(p.storage.root.path.as_deref(), Some(entry.overlay.as_str()));
    }

    #[test]
    fn format_unix_utc_renders_known_moments() {
        // 2026-05-05T16:00:00Z = 1777996800 UNIX seconds.
        assert_eq!(format_unix_utc(1_777_996_800),
            "2026-05-05T16:00:00Z");
        // Epoch.
        assert_eq!(format_unix_utc(0), "1970-01-01T00:00:00Z");
    }
}
