//! rotten-apple profile manifest schema.
//!
//! Mirrors the TOML schema in `manifests/*.example.toml` and the contract
//! in `design/architecture.md`. Hypervisor-agnostic: one [`Profile`] value
//! is consumed by every backend.
//!
//! # Example
//!
//! ```no_run
//! use rotten_apple_manifest::{Profile, BackendCapabilities};
//!
//! let p = Profile::load("manifests/ubuntu-desktop.example.toml").unwrap();
//! let caps = BackendCapabilities::xen_reference();
//! let issues = p.validate_against(&caps);
//! assert!(issues.is_empty(), "ubuntu-desktop should be valid on xen");
//! ```

use serde::{Deserialize, Deserializer, Serialize};
use std::fs;
use std::path::Path;

pub const SCHEMA_VERSION: &str = "1";

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    Parse(ParseError),
}

#[derive(Debug, Clone)]
pub enum ParseError {
    BadSize(String),
    BadDuration(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::BadSize(s)     => write!(f, "unparseable size: {s:?}"),
            ParseError::BadDuration(s) => write!(f, "unparseable duration: {s:?}"),
        }
    }
}
impl std::error::Error for ParseError {}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e)    => write!(f, "io: {e}"),
            LoadError::Toml(e)  => write!(f, "toml: {e}"),
            LoadError::Parse(e) => write!(f, "parse: {e:?}"),
        }
    }
}
impl std::error::Error for LoadError {}

impl From<std::io::Error>     for LoadError { fn from(e: std::io::Error)     -> Self { LoadError::Io(e) } }
impl From<toml::de::Error>    for LoadError { fn from(e: toml::de::Error)    -> Self { LoadError::Toml(e) } }
impl From<ParseError>         for LoadError { fn from(e: ParseError)         -> Self { LoadError::Parse(e) } }

// ---------------------------------------------------------------------------
// Primitive parsers

/// Parse `"56G"` / `"256M"` / `"1.5G"` → bytes (binary multipliers, kB=1024).
/// Bare numbers (no suffix) are bytes.
pub fn parse_size_bytes(spec: &str) -> Result<u64, ParseError> {
    let s = spec.trim();
    let split_at = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (digits, unit) = s.split_at(split_at);
    let value: f64 = digits.parse().map_err(|_| ParseError::BadSize(spec.into()))?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B"                 => 1,
        "K" | "KB" | "KIB"       => 1 << 10,
        "M" | "MB" | "MIB"       => 1 << 20,
        "G" | "GB" | "GIB"       => 1 << 30,
        "T" | "TB" | "TIB"       => 1 << 40,
        _                        => return Err(ParseError::BadSize(spec.into())),
    };
    Ok((value * mult as f64) as u64)
}

/// Parse `"30s"` / `"5m"` / `"1h"` / `"never"` → seconds (None = never).
pub fn parse_duration_seconds(spec: &str) -> Result<Option<u64>, ParseError> {
    let s = spec.trim();
    if s.is_empty() || s == "never" { return Ok(None); }
    let split_at = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split_at);
    let value: u64 = digits.parse().map_err(|_| ParseError::BadDuration(spec.into()))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | ""  => 1,
        "m"       => 60,
        "h"       => 3600,
        "d"       => 86400,
        _         => return Err(ParseError::BadDuration(spec.into())),
    };
    Ok(Some(value * mult))
}

// serde adapters

fn deser_size<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    parse_size_bytes(&s).map_err(serde::de::Error::custom)
}

fn deser_opt_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let s: Option<String> = Option::deserialize(d)?;
    match s {
        None    => Ok(None),
        Some(s) => parse_duration_seconds(&s).map_err(serde::de::Error::custom),
    }
}

fn default_true() -> bool { true }
fn default_personal() -> String { "personal".into() }
fn default_schema_version() -> String { SCHEMA_VERSION.into() }
fn default_rw_exclusive() -> String { "rw-exclusive".into() }
fn default_bridge() -> String { "bridge".into() }
fn default_none() -> String { "none".into() }
fn default_swtpm_mode() -> TpmMode { TpmMode::Swtpm }
fn default_follow_focus() -> String { "follow_focus".into() }
fn default_policy() -> String { "policy".into() }
fn default_background() -> String { "background".into() }

// ---------------------------------------------------------------------------
// Enums (only where the small fixed set helps dispatch)

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind { Desktop, Appliance, Service }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TpmMode {
    Swtpm,
    HardwarePassthrough,
    None,
}

// ---------------------------------------------------------------------------
// Sections — these mirror the TOML structure 1:1

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileHeader {
    pub name: String,
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type")]
    pub kind: ProfileKind,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Meta {
    #[serde(default = "default_personal")]
    pub license: String,
    #[serde(default)]
    pub attestation_required: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Resources {
    #[serde(rename = "memory_active",  deserialize_with = "deser_size")]
    pub memory_active_bytes: u64,
    #[serde(rename = "memory_idle",    deserialize_with = "deser_size")]
    pub memory_idle_bytes: u64,
    #[serde(rename = "memory_minimum", deserialize_with = "deser_size")]
    pub memory_minimum_bytes: u64,
    pub vcpus_active: u32,
    pub vcpus_idle: u32,
    pub vcpus_minimum: u32,
    #[serde(default = "default_true")]
    pub prefer_p_cores: bool,
    #[serde(default = "default_true")]
    pub idle_on_e_cores: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageSpec {
    pub kind: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_rw_exclusive")]
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Storage {
    pub root: StorageSpec,
    #[serde(default)]
    pub extra_disks: Vec<StorageSpec>,
}

/// `egress` may be either `"any"` or `["host1", "host2", ...]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Egress {
    Single(String),
    Many(Vec<String>),
}
impl Default for Egress { fn default() -> Self { Egress::Single("any".into()) } }

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkInterface {
    #[serde(default = "primary_name")]
    pub name: String,
    #[serde(default = "auto_mac")]
    pub mac: String,
    #[serde(default)]
    pub egress: Egress,
}
fn primary_name() -> String { "primary".into() }
fn auto_mac() -> String { "auto".into() }

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Network {
    #[serde(default = "default_bridge")]
    pub mode: String,
    #[serde(default)]
    pub interfaces: Vec<NetworkInterface>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Gpu {
    #[serde(default = "default_none")]
    pub mode: String,           // "passthrough" | "paravirt" | "none"
    #[serde(default)]
    pub device: Option<String>, // PCI BDF, e.g. "0000:00:02.0"
    #[serde(default)]
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Audio {
    #[serde(default = "default_none")]
    pub mode: String,
    #[serde(default)]
    pub default_sink: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    #[serde(default = "default_follow_focus")]
    pub keyboard: String,
    #[serde(default = "default_follow_focus")]
    pub mouse: String,
}
impl Default for Input {
    fn default() -> Self {
        Input { keyboard: "follow_focus".into(), mouse: "follow_focus".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsbRoute {
    pub vendor: String,
    pub product: String,
    pub route: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usb {
    #[serde(default = "default_policy")]
    pub mode: String,
    #[serde(default = "default_follow_focus")]
    pub default_route: String,
    #[serde(default)]
    pub explicit_routes: Vec<UsbRoute>,
}
impl Default for Usb {
    fn default() -> Self {
        Usb { mode: "policy".into(), default_route: "follow_focus".into(),
              explicit_routes: vec![] }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tpm {
    #[serde(default = "default_swtpm_mode")]
    pub mode: TpmMode,
}
impl Default for Tpm {
    fn default() -> Self { Tpm { mode: TpmMode::Swtpm } }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Autostart {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, deserialize_with = "deser_opt_duration")]
    pub delay_after_boot: Option<u64>,         // seconds
    #[serde(default, deserialize_with = "deser_opt_duration")]
    pub suspend_after_idle: Option<u64>,       // seconds; None = never
}

#[derive(Debug, Clone, Deserialize)]
pub struct Trigger {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub manifest_name: Option<String>,
    #[serde(default)]
    pub browsers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntegrationSocket {
    pub kind: String,
    pub port: u16,
    pub role: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Integration {
    #[serde(default)]
    pub sockets: Vec<IntegrationSocket>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub clipboard: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Orchestration {
    #[serde(default = "default_background")]
    pub priority: String,
    #[serde(default)]
    pub exclusive_resources: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Trust {
    #[serde(default)]
    pub documented_capabilities: Vec<String>,
    #[serde(default)]
    pub documented_limitations: Vec<String>,
}

/// Memory-side control-loop policy for the engine. All fields optional —
/// when every field is `None` the engine treats this domain as manual-only
/// and emits no Apply events.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyMemory {
    /// Floor; engine won't shrink below this many MB.
    #[serde(default)]
    pub min_mb: Option<u64>,
    /// Ceiling; engine won't grow above this many MB.
    #[serde(default)]
    pub max_mb: Option<u64>,
    /// Try to keep this much free in the guest. (Reserved for v1; the v0
    /// engine ignores it and only honours min/max.)
    #[serde(default)]
    pub target_headroom_pct: Option<u32>,
    /// Minimum seconds between same-direction balloon moves.
    #[serde(default)]
    pub cooldown_s: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub memory: PolicyMemory,
}

// ---------------------------------------------------------------------------
// Mesh-aware sections (Phase 0 of the rotten-apple Xen mesh).
//
// All three default to "no constraint" so existing single-host manifests
// parse unchanged. The Phase 0 scheduler honors them locally; Phase 2+
// gives them mesh-wide meaning. Schema is intentionally minimal — the
// load-bearing decisions are: anchor (where can this run?), capabilities
// required (which nodes are eligible?), lease policy (which controller
// is allowed to do what?). Everything else is over-engineering.

/// Where this manifest is allowed to run, and whether the controller may
/// move it. The classic stateful-vs-ephemeral split.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct Anchor {
    /// Node ID this manifest is pinned to. `None` = no pin (scheduler
    /// may run on any capable node). The literal string `"this-host"`
    /// is a magic value that the scheduler resolves to the node where
    /// the manifest was first authored — useful for "stays where I
    /// made it" without hardcoding a node id.
    #[serde(default)]
    pub node: Option<String>,
    /// Whether the controller is allowed to move this manifest to a
    /// different node. `false` (default, safe) = the controller may
    /// NOT migrate without explicit operator action; `true` = the
    /// scheduler may move freely if `node` is also `None`.
    /// Stateful workloads (your encrypted Ubuntu desktop) leave this
    /// false; stateless compute (CI runners, scratch dev VMs) opt in.
    #[serde(default)]
    pub migratable: bool,
}

/// What the host must provide for this manifest to run there. Phase 0
/// uses these for local validation only; Phase 1+ uses them as the
/// scheduler's eligibility filter when picking a node.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct CapabilitiesRequired {
    /// Host must have IOMMU enabled (intel_iommu or amd_iommu present
    /// in the running kernel cmdline). Required for any GPU/PCI
    /// passthrough; harmless to set on hosts that already have it.
    #[serde(default)]
    pub need_iommu: bool,
    /// Host must expose a GPU of this class. `"iGPU"` | `"dGPU"` |
    /// `"any"`. `None` = no GPU requirement.
    #[serde(default)]
    pub need_gpu_class: Option<String>,
    /// Host must have at least this many physical cpus available.
    /// `None` = no minimum (manifest's own `resources.vcpus_active`
    /// is enforced separately).
    #[serde(default)]
    pub need_pcpu_min: Option<u32>,
    /// Host must have at least this many MB of RAM free for new
    /// guests. Typically equals or slightly exceeds
    /// `resources.memory_active`; use it to demand headroom for
    /// bursty workloads.
    #[serde(default)]
    pub need_memory_mb_min: Option<u64>,
}

/// Who may control this manifest, and what they're allowed to do.
/// The agent's refusal layer — defense-in-depth so a compromised
/// controller can't immediately destroy the fleet.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct LeasePolicy {
    /// Node IDs of controllers permitted to issue events for this
    /// manifest. Empty = any node holding a valid lease may control
    /// (the permissive default — fine on a single-user laptop, tighten
    /// when you add untrusted peers).
    #[serde(default)]
    pub controllers_allowed: Vec<String>,
    /// Wire-protocol operation names the agent will accept from a
    /// controller for this manifest. Empty = default safe set
    /// (`assign`, `unassign`, `balloon`, `start`, `shutdown`).
    /// Sensitive operations (`destroy`, `attach_disk`, `migrate_to`)
    /// MUST be listed explicitly.
    #[serde(default)]
    pub operations_allowed: Vec<String>,
}

// ---------------------------------------------------------------------------
// The top-level Profile

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub profile: ProfileHeader,
    #[serde(default)]
    pub meta: Meta,
    pub resources: Resources,
    pub storage: Storage,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub gpu: Gpu,
    #[serde(default)]
    pub audio: Audio,
    #[serde(default)]
    pub input: Input,
    #[serde(default)]
    pub usb: Usb,
    #[serde(default)]
    pub tpm: Tpm,
    #[serde(default)]
    pub autostart: Autostart,
    #[serde(default)]
    pub trigger: Option<Trigger>,
    #[serde(default)]
    pub integration: Integration,
    #[serde(default)]
    pub orchestration: Orchestration,
    #[serde(default)]
    pub trust: Trust,
    #[serde(default)]
    pub policy: Policy,
    /// Where this manifest may run; whether the controller may move it.
    /// Defaults to "no pin, not migratable" — stateful behavior, safe.
    #[serde(default)]
    pub anchor: Anchor,
    /// Host requirements (IOMMU, GPU class, capacity floors). Empty by
    /// default; the scheduler treats absent fields as "no constraint."
    #[serde(default, rename = "capabilities_required")]
    pub capabilities_required: CapabilitiesRequired,
    /// Refusal layer: which controllers are allowed and which operations
    /// they may issue. Empty = permissive defaults (single-user laptop).
    #[serde(default, rename = "lease_policy")]
    pub lease_policy: LeasePolicy,
}

impl Profile {
    /// Read and parse a manifest from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Profile, LoadError> {
        let text = fs::read_to_string(path.as_ref())?;
        Self::from_str(&text)
    }

    /// Parse a manifest from a TOML string.
    // Inherent `from_str` keeps callers from needing `use std::str::FromStr`;
    // the public API has shipped under this name.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Result<Profile, LoadError> {
        Ok(toml::from_str(text)?)
    }

    /// Convenience accessors that flatten the nested structure for callers.
    pub fn name(&self) -> &str            { &self.profile.name }
    pub fn kind(&self) -> &ProfileKind    { &self.profile.kind }
    pub fn description(&self) -> &str     { &self.profile.description }
    pub fn schema_version(&self) -> &str  { &self.profile.schema_version }
    pub fn license_tier(&self) -> &str    { &self.meta.license }
    pub fn attestation_required(&self) -> bool { self.meta.attestation_required }

    /// Memory-side engine policy. Returns `None` when no fields are set,
    /// signalling "manual only" — the engine takes no action on this domain.
    pub fn policy_memory(&self) -> Option<&PolicyMemory> {
        let m = &self.policy.memory;
        if m.min_mb.is_none()
            && m.max_mb.is_none()
            && m.target_headroom_pct.is_none()
            && m.cooldown_s.is_none()
        {
            None
        } else {
            Some(m)
        }
    }

    /// Return human-readable strings describing mismatches between this
    /// profile's declared needs and what `caps` advertises. Empty list =
    /// the profile is satisfiable on this backend.
    pub fn validate_against(&self, caps: &BackendCapabilities) -> Vec<String> {
        let mut issues = Vec::new();
        let r = &self.resources;

        // ---- internal consistency (independent of backend) ----
        if r.memory_minimum_bytes > r.memory_idle_bytes {
            issues.push(format!(
                "resources: memory_minimum ({}) exceeds memory_idle ({})",
                r.memory_minimum_bytes, r.memory_idle_bytes,
            ));
        }
        if r.memory_idle_bytes > r.memory_active_bytes {
            issues.push(format!(
                "resources: memory_idle ({}) exceeds memory_active ({})",
                r.memory_idle_bytes, r.memory_active_bytes,
            ));
        }
        if r.vcpus_minimum > r.vcpus_idle {
            issues.push("resources: vcpus_minimum exceeds vcpus_idle".into());
        }
        if r.vcpus_idle > r.vcpus_active {
            issues.push("resources: vcpus_idle exceeds vcpus_active".into());
        }

        // ---- backend capability checks ----
        if matches!(self.tpm.mode, TpmMode::HardwarePassthrough)
            && !caps.supports_hardware_tpm_passthrough
        {
            issues.push(format!(
                "tpm.mode=hardware-passthrough requires hardware-TPM passthrough; \
                 {} does not provide it.", caps.backend_name,
            ));
        }
        if self.gpu.mode == "passthrough" && !caps.supports_pci_passthrough_at_boot {
            issues.push(format!(
                "gpu.mode=passthrough requires PCI passthrough; {} does not.",
                caps.backend_name,
            ));
        }
        if self.attestation_required() && !caps.supports_hyperv_compatible_attestation {
            issues.push(format!(
                "attestation_required=true: {} does not provide hyperv-compatible \
                 attestation chain.", caps.backend_name,
            ));
        }
        if self.autostart.suspend_after_idle.is_some() && !caps.supports_suspend_resume {
            issues.push(format!(
                "autostart.suspend_after_idle requires suspend/resume; {} does not.",
                caps.backend_name,
            ));
        }
        if self.usb.mode == "policy" && !self.usb.explicit_routes.is_empty()
            && !caps.supports_usb_passthrough
        {
            issues.push(format!(
                "usb.explicit_routes require USB passthrough; {} does not.",
                caps.backend_name,
            ));
        }
        if matches!(self.tpm.mode, TpmMode::Swtpm) && !caps.supports_swtpm {
            issues.push(format!(
                "tpm.mode=swtpm requires swtpm support; {} does not.",
                caps.backend_name,
            ));
        }
        issues
    }
}

// ---------------------------------------------------------------------------
// BackendCapabilities — mirror of design/backend-trait.md

#[derive(Debug, Clone, Default)]
pub struct BackendCapabilities {
    pub backend_name: String,
    pub supports_balloon: bool,
    pub supports_hot_pci_passthrough: bool,
    pub supports_pci_passthrough_at_boot: bool,
    pub supports_usb_passthrough: bool,
    pub supports_swtpm: bool,
    pub supports_hardware_tpm_passthrough: bool,
    pub supports_hyperv_compatible_attestation: bool,
    pub supports_live_migration: bool,
    pub supports_suspend_resume: bool,
    pub max_guests: Option<u32>,
}

impl BackendCapabilities {
    /// Reference Xen capabilities for validation in tests / dry-run.
    pub fn xen_reference() -> Self {
        Self {
            backend_name: "xen".into(),
            supports_balloon: true,
            supports_hot_pci_passthrough: true,
            supports_pci_passthrough_at_boot: true,
            supports_usb_passthrough: true,
            supports_swtpm: true,
            supports_hardware_tpm_passthrough: false,
            supports_hyperv_compatible_attestation: false,
            supports_live_migration: false,
            supports_suspend_resume: true,
            max_guests: None,
        }
    }

    /// Reference Hyper-V capabilities for validation in tests / dry-run.
    pub fn hyperv_reference() -> Self {
        Self {
            backend_name: "hyperv".into(),
            supports_balloon: true,
            supports_hot_pci_passthrough: false,        // DDA needs guest stop
            supports_pci_passthrough_at_boot: true,
            supports_usb_passthrough: true,
            supports_swtpm: true,
            supports_hardware_tpm_passthrough: false,   // vTPM only
            supports_hyperv_compatible_attestation: true,  // the headline diff
            supports_live_migration: true,
            supports_suspend_resume: true,
            max_guests: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_handles_units() {
        assert_eq!(parse_size_bytes("56G").unwrap(),    56u64 << 30);
        assert_eq!(parse_size_bytes("256M").unwrap(),   256u64 << 20);
        assert_eq!(parse_size_bytes("1.5G").unwrap(),   (1.5 * (1u64 << 30) as f64) as u64);
        assert_eq!(parse_size_bytes("128").unwrap(),    128);
        assert_eq!(parse_size_bytes("1KB").unwrap(),    1024);
        assert!(parse_size_bytes("garbage").is_err());
    }

    #[test]
    fn parse_duration_handles_units_and_never() {
        assert_eq!(parse_duration_seconds("30s").unwrap(), Some(30));
        assert_eq!(parse_duration_seconds("5m").unwrap(),  Some(300));
        assert_eq!(parse_duration_seconds("1h").unwrap(),  Some(3600));
        assert_eq!(parse_duration_seconds("never").unwrap(), None);
        assert_eq!(parse_duration_seconds("").unwrap(),    None);
        assert!(parse_duration_seconds("garbage").is_err());
    }

    #[test]
    fn policy_memory_absent_returns_none() {
        let toml = r#"
            [profile]
            name = "x"
            type = "appliance"
            [resources]
            memory_active = "1G"
            memory_idle = "256M"
            memory_minimum = "128M"
            vcpus_active = 1
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/tmp/x.qcow2" }
        "#;
        let p = Profile::from_str(toml).unwrap();
        assert!(p.policy_memory().is_none());
    }

    #[test]
    fn policy_memory_round_trips() {
        let toml = r#"
            [profile]
            name = "x"
            type = "appliance"
            [resources]
            memory_active = "1G"
            memory_idle = "256M"
            memory_minimum = "128M"
            vcpus_active = 1
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/tmp/x.qcow2" }
            [policy.memory]
            min_mb = 256
            max_mb = 4096
            cooldown_s = 30
        "#;
        let p = Profile::from_str(toml).unwrap();
        let pm = p.policy_memory().expect("policy memory");
        assert_eq!(pm.min_mb, Some(256));
        assert_eq!(pm.max_mb, Some(4096));
        assert_eq!(pm.cooldown_s, Some(30));
        assert!(pm.target_headroom_pct.is_none());
    }

    #[test]
    fn cap_consistency_with_python_validator() {
        // The same constructed-bad profile that the Python suite uses: an
        // unmodifiable hardware-TPM passthrough should produce 1 issue on
        // Hyper-V (no hardware-TPM passthrough) and 2 on Xen (also no
        // hyperv-compat attestation).
        let toml = r#"
            [profile]
            name = "tester"
            type = "appliance"
            [meta]
            attestation_required = true
            [resources]
            memory_active = "1G"
            memory_idle = "256M"
            memory_minimum = "128M"
            vcpus_active = 2
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/tmp/x.qcow2" }
            [tpm]
            mode = "hardware-passthrough"
        "#;
        let p = Profile::from_str(toml).unwrap();
        let xen = p.validate_against(&BackendCapabilities::xen_reference());
        let hv  = p.validate_against(&BackendCapabilities::hyperv_reference());
        assert_eq!(xen.len(), 2, "xen issues: {xen:?}");
        assert_eq!(hv.len(), 1,  "hyperv issues: {hv:?}");
    }

    // -----------------------------------------------------------------------
    // Mesh-aware sections (Phase 0): anchor, capabilities_required,
    // lease_policy. Pin defaults + parsing + that absence stays absent.

    fn minimal_profile_toml() -> &'static str {
        r#"
            [profile]
            name = "x"
            type = "appliance"
            [resources]
            memory_active = "1G"
            memory_idle = "256M"
            memory_minimum = "128M"
            vcpus_active = 1
            vcpus_idle = 1
            vcpus_minimum = 1
            [storage]
            root = { kind = "qcow2", path = "/tmp/x.qcow2" }
        "#
    }

    #[test]
    fn anchor_defaults_to_no_pin_not_migratable() {
        // Single-host manifests written before the mesh existed must
        // parse unchanged. Default anchor = no node pin, not migratable
        // (the safe stateful default — controller can't move it).
        let p = Profile::from_str(minimal_profile_toml()).unwrap();
        assert_eq!(p.anchor.node, None,
                   "default anchor must not pin to any node");
        assert!(!p.anchor.migratable,
                "default anchor must not be migratable (safe for stateful)");
    }

    #[test]
    fn anchor_round_trips_this_host_magic_value() {
        // "this-host" is the documented magic value the scheduler
        // resolves to the authoring node — exercise the literal so a
        // future refactor doesn't quietly break it.
        let toml = format!(
            "{}\n[anchor]\nnode = \"this-host\"\nmigratable = false\n",
            minimal_profile_toml(),
        );
        let p = Profile::from_str(&toml).unwrap();
        assert_eq!(p.anchor.node.as_deref(), Some("this-host"));
        assert!(!p.anchor.migratable);
    }

    #[test]
    fn anchor_round_trips_explicit_node_id() {
        // Real cross-node manifests will name a peer's node id. Pin the
        // shape that mesh.toml-style configs will produce.
        let toml = format!(
            "{}\n[anchor]\nnode = \"desk-box-7f3a\"\nmigratable = true\n",
            minimal_profile_toml(),
        );
        let p = Profile::from_str(&toml).unwrap();
        assert_eq!(p.anchor.node.as_deref(), Some("desk-box-7f3a"));
        assert!(p.anchor.migratable,
                "explicit migratable=true must round-trip — opt-in for ephemeral compute");
    }

    #[test]
    fn capabilities_required_defaults_are_no_constraint() {
        // Absent section => every field is None/false/empty so the
        // scheduler treats it as "any host is eligible." Existing
        // manifests must parse without growing implicit constraints.
        let p = Profile::from_str(minimal_profile_toml()).unwrap();
        let caps = &p.capabilities_required;
        assert!(!caps.need_iommu);
        assert_eq!(caps.need_gpu_class, None);
        assert_eq!(caps.need_pcpu_min, None);
        assert_eq!(caps.need_memory_mb_min, None);
    }

    #[test]
    fn capabilities_required_full_round_trip() {
        // GPU-passthrough workloads will write all four fields. Pin the
        // exact TOML keys so a future schema rename surfaces here.
        let toml = format!(
            "{}\n[capabilities_required]\n\
             need_iommu = true\n\
             need_gpu_class = \"dGPU\"\n\
             need_pcpu_min = 4\n\
             need_memory_mb_min = 16384\n",
            minimal_profile_toml(),
        );
        let p = Profile::from_str(&toml).unwrap();
        let caps = &p.capabilities_required;
        assert!(caps.need_iommu);
        assert_eq!(caps.need_gpu_class.as_deref(), Some("dGPU"));
        assert_eq!(caps.need_pcpu_min, Some(4));
        assert_eq!(caps.need_memory_mb_min, Some(16384));
    }

    #[test]
    fn lease_policy_defaults_are_permissive_empty() {
        // Empty controllers_allowed = any controller with a valid lease
        // may control. Empty operations_allowed = the agent applies its
        // default safe set (assign/unassign/balloon/start/shutdown).
        // The agent — not the manifest — owns the default safe set, so
        // here we just pin that the manifest fields default to empty.
        let p = Profile::from_str(minimal_profile_toml()).unwrap();
        assert!(p.lease_policy.controllers_allowed.is_empty());
        assert!(p.lease_policy.operations_allowed.is_empty());
    }

    #[test]
    fn lease_policy_round_trips_explicit_controllers_and_ops() {
        // Defense-in-depth use case: this manifest may only be touched
        // by named controllers, and only for the listed ops. Anything
        // else (e.g. `destroy`) is rejected by the agent's refusal layer.
        let toml = format!(
            "{}\n[lease_policy]\n\
             controllers_allowed = [\"laptop-id\", \"desk-box-id\"]\n\
             operations_allowed = [\"assign\", \"balloon\"]\n",
            minimal_profile_toml(),
        );
        let p = Profile::from_str(&toml).unwrap();
        assert_eq!(p.lease_policy.controllers_allowed,
                   vec!["laptop-id".to_string(), "desk-box-id".to_string()]);
        assert_eq!(p.lease_policy.operations_allowed,
                   vec!["assign".to_string(), "balloon".to_string()]);
    }

    #[test]
    fn mesh_sections_compose_with_existing_fields() {
        // Realistic shape: a stateful workload pinned to its anchor
        // node, requiring iGPU passthrough, restricted to a controller
        // allowlist. Pin that all three sections coexist with the
        // existing schema (storage, gpu, autostart, etc).
        let toml = format!(
            "{}\n\
             [gpu]\nmode = \"passthrough\"\ndevice = \"0000:00:02.0\"\n\
             [autostart]\nenabled = true\n\
             [anchor]\nnode = \"this-host\"\n\
             [capabilities_required]\nneed_iommu = true\nneed_gpu_class = \"iGPU\"\n\
             [lease_policy]\ncontrollers_allowed = [\"laptop-id\"]\n",
            minimal_profile_toml(),
        );
        let p = Profile::from_str(&toml).unwrap();
        assert_eq!(p.gpu.mode, "passthrough");
        assert!(p.autostart.enabled);
        assert_eq!(p.anchor.node.as_deref(), Some("this-host"));
        assert!(p.capabilities_required.need_iommu);
        assert_eq!(p.lease_policy.controllers_allowed,
                   vec!["laptop-id".to_string()]);
    }
}
