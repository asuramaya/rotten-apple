//! Backend adapter contract.
//!
//! Materialization of [`design/backend-trait.md`]. Every concrete backend
//! (Xen, Hyper-V, future) implements [`HypervisorBackend`]. The orchestrator
//! core knows nothing about specific hypervisors — it interacts only
//! through this trait.
//!
//! See `design/backend-trait.md` at the project root for the behavioural
//! contract each method MUST honour.

use rotten_apple_manifest::{BackendCapabilities, Profile};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Opaque handles & values

/// Per-backend handle to a created guest. The orchestrator core treats
/// this as a black box; only the issuing backend can interpret it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GuestHandle(pub String);

impl std::fmt::Display for GuestHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Snapshot of a suspended guest's RAM + device state. Held by the core,
/// passed back to [`HypervisorBackend::resume`]. Format is backend-private.
#[derive(Clone, Debug)]
pub struct Snapshot(pub Vec<u8>);

/// PCI address (BDF). Used for passthrough operations.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PciAddr {
    pub domain: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl std::fmt::Display for PciAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04x}:{:02x}:{:02x}.{}", self.domain, self.bus, self.device, self.function)
    }
}

impl std::str::FromStr for PciAddr {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // accept "0000:00:02.0" or "00:02.0"
        let parts: Vec<&str> = s.split([':', '.']).collect();
        let (domain, bus, device, function) = match parts.as_slice() {
            [d, b, dev, fun] => (
                u16::from_str_radix(d, 16).map_err(|e| e.to_string())?,
                u8::from_str_radix(b, 16).map_err(|e| e.to_string())?,
                u8::from_str_radix(dev, 16).map_err(|e| e.to_string())?,
                u8::from_str_radix(fun, 16).map_err(|e| e.to_string())?,
            ),
            [b, dev, fun] => (
                0,
                u8::from_str_radix(b, 16).map_err(|e| e.to_string())?,
                u8::from_str_radix(dev, 16).map_err(|e| e.to_string())?,
                u8::from_str_radix(fun, 16).map_err(|e| e.to_string())?,
            ),
            _ => return Err(format!("not a PCI BDF: {s:?}")),
        };
        Ok(PciAddr { domain, bus, device, function })
    }
}

/// USB device identity. Optional serial for unique disambiguation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsbDev {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: Option<String>,
}

/// Mask of host CPU IDs for vCPU pinning.
#[derive(Clone, Debug, Default)]
pub struct CpuMask(pub Vec<u32>);

// ---------------------------------------------------------------------------
// Runtime state

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestState {
    Created,
    Running,
    Idle,
    Suspended,
    Stopped,
    Failed,
}

#[derive(Clone, Debug)]
pub struct GuestStatus {
    pub state: GuestState,
    pub memory_mb: u64,
    /// Backend-reported max memory the guest could grow to (libxl's
    /// `max_memkb`). Cockpit uses this as the balloon ceiling; engine
    /// uses it as the policy upper bound. Backends that can't report a
    /// max should set this equal to `memory_mb`.
    pub memory_max_mb: u64,
    pub vcpus: u32,
    pub uptime: Duration,
    pub last_event: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GuestSummary {
    pub handle: GuestHandle,
    pub name: String,
    pub state: GuestState,
}

/// Host-wide resource snapshot. Cockpit + engine consult this to display
/// "what's available, what's allocated, what's free." Backends derive
/// this from their hypervisor's bookkeeping (libxl: libxl_get_physinfo).
///
/// Memory totals reflect host hardware (after firmware reservations);
/// free_memory_mb is what Xen has unallocated. Domain memory sums plus
/// free should approximately equal total — the residual is hypervisor
/// + dom0 kernel + scrubbing pages.
#[derive(Clone, Debug)]
pub struct HostPhysinfo {
    pub total_pcpus: u32,
    pub threads_per_core: u32,
    pub cores_per_socket: u32,
    pub total_memory_mb: u64,
    pub free_memory_mb: u64,
    pub scrub_memory_mb: u64,
}

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    NotSupported,           // capability advertised false
    GuestNotFound,
    GuestAlreadyRunning,
    InsufficientResources,
    HardwareUnavailable,
    PermissionDenied,
    BackendInternal,
}

#[derive(Debug, Clone)]
pub struct BackendError {
    pub kind: ErrorKind,
    pub detail: String,
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for BackendError {}

impl BackendError {
    pub fn not_supported(detail: impl Into<String>) -> Self {
        Self { kind: ErrorKind::NotSupported, detail: detail.into() }
    }
    pub fn guest_not_found(detail: impl Into<String>) -> Self {
        Self { kind: ErrorKind::GuestNotFound, detail: detail.into() }
    }
    pub fn already_running(detail: impl Into<String>) -> Self {
        Self { kind: ErrorKind::GuestAlreadyRunning, detail: detail.into() }
    }
    pub fn insufficient_resources(detail: impl Into<String>) -> Self {
        Self { kind: ErrorKind::InsufficientResources, detail: detail.into() }
    }
    pub fn hardware_unavailable(detail: impl Into<String>) -> Self {
        Self { kind: ErrorKind::HardwareUnavailable, detail: detail.into() }
    }
    pub fn permission_denied(detail: impl Into<String>) -> Self {
        Self { kind: ErrorKind::PermissionDenied, detail: detail.into() }
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Self { kind: ErrorKind::BackendInternal, detail: detail.into() }
    }
}

pub type Result<T> = std::result::Result<T, BackendError>;

// ---------------------------------------------------------------------------
// The trait

/// Every concrete backend implements this. The orchestrator core depends
/// only on `dyn HypervisorBackend`, never on a specific backend type.
///
/// **Send but not Sync.** libxl_ctx is documented as not thread-safe per
/// ctx; Hyper-V's HCS handles are similar. The orchestrator runs a single
/// event loop and moves the backend between async tasks if needed (Send),
/// but never shares it concurrently across threads (!Sync).
pub trait HypervisorBackend: Send {
    /// Stable identifier — `"xen"` | `"hyperv"` | etc. Used for logs and
    /// for discriminating which backend produced a given handle.
    fn name(&self) -> &str;

    /// Self-describe capabilities. Cached by the orchestrator at startup.
    /// Backends MUST NOT lie: if `supports_balloon` is true,
    /// [`balloon_to`] must work; if false, the orchestrator won't call it.
    fn capabilities(&self) -> BackendCapabilities;

    // ---- guest lifecycle ----

    /// Create a guest from a profile. MUST NOT start it. Returns a handle
    /// stable across daemon restarts (the backend may reuse an existing
    /// stale entry from a prior orchestrator if name matches).
    fn create_guest(&self, profile: &Profile) -> Result<GuestHandle>;

    fn destroy_guest(&self, h: &GuestHandle) -> Result<()>;
    fn start_guest(&self, h: &GuestHandle) -> Result<()>;

    /// `force=false` SHOULD send a graceful shutdown signal and wait up to
    /// a backend-defined timeout (recommended 30 s) before returning an
    /// error. `force=true` MUST kill the guest immediately.
    fn stop_guest(&self, h: &GuestHandle, force: bool) -> Result<()>;

    // ---- resource arbitration ----

    /// Target memory in MB. Backend handles the balloon driver dance.
    /// If `target_mb` is below the guest's declared `memory_minimum`, the
    /// backend MUST clamp to the minimum and return success — ballooning
    /// is a soft-state operation; the orchestrator handles drift via its
    /// policy loop.
    fn balloon_to(&self, h: &GuestHandle, target_mb: u64) -> Result<()>;

    /// Pin vCPUs to a host CPU mask. Used to keep dom0 on E-cores and
    /// active desktop guests on P-cores.
    fn pin_vcpus(&self, h: &GuestHandle, mask: CpuMask) -> Result<()>;

    // ---- suspend / resume ----

    /// Snapshot RAM + device state. Returns an opaque token the
    /// orchestrator stores in its state DB.
    fn suspend(&self, h: &GuestHandle) -> Result<Snapshot>;

    fn resume(&self, h: &GuestHandle, snap: Snapshot) -> Result<()>;

    // ---- passthrough ----

    /// Hot-attach a PCI device. Returns [`ErrorKind::NotSupported`] if
    /// `supports_hot_pci_passthrough` is false; the orchestrator then
    /// schedules a stop / passthrough-at-boot / start cycle instead.
    fn passthrough_pci(&self, h: &GuestHandle, addr: PciAddr) -> Result<()>;

    fn revoke_pci(&self, h: &GuestHandle, addr: PciAddr) -> Result<()>;

    fn attach_usb(&self, h: &GuestHandle, dev: UsbDev) -> Result<()>;
    fn detach_usb(&self, h: &GuestHandle, dev: UsbDev) -> Result<()>;

    // ---- introspection ----

    fn list(&self) -> Vec<GuestSummary>;
    fn status(&self, h: &GuestHandle) -> Result<GuestStatus>;

    /// Aggregate host resources (total/free memory + physical CPU
    /// topology). Used by the cockpit's resources view and by the
    /// engine when deciding whether a balloon-up is feasible.
    fn physinfo(&self) -> Result<HostPhysinfo>;
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn pci_addr_round_trips_full_form() {
        let a = PciAddr::from_str("0000:00:02.0").unwrap();
        assert_eq!(a, PciAddr { domain: 0, bus: 0, device: 2, function: 0 });
        assert_eq!(a.to_string(), "0000:00:02.0");
    }

    #[test]
    fn pci_addr_accepts_short_form() {
        let a = PciAddr::from_str("01:00.0").unwrap();
        assert_eq!(a, PciAddr { domain: 0, bus: 1, device: 0, function: 0 });
    }

    #[test]
    fn pci_addr_rejects_garbage() {
        assert!(PciAddr::from_str("not-a-pci-bdf").is_err());
        assert!(PciAddr::from_str("0000:zz:02.0").is_err());
    }

    #[test]
    fn backend_error_constructors_are_terse() {
        let e = BackendError::not_supported("hot pci passthrough");
        assert_eq!(e.kind, ErrorKind::NotSupported);
        assert!(e.detail.contains("hot pci"));
    }
}
