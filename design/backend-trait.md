# Backend Adapter Contract

The single source of truth for how the orchestrator core talks to a
hypervisor. Both `backends/xen` and `backends/hyperv` implement this
trait. The orchestrator core MUST NOT import either backend module;
it interacts only through this trait.

```rust
// crates/backend/src/lib.rs

use crate::manifest::{Profile, ResourceSpec, GpuSpec, /* ... */};
use std::time::Duration;

/// Opaque per-backend handle to a created guest. The core treats this
/// as a black box; only the issuing backend can interpret it.
#[derive(Clone, Debug)]
pub struct GuestHandle(pub String);   // backend-specific identifier

/// Snapshot of a suspended guest's RAM + device state. Held by the
/// core and passed back to `resume`. Format is backend-specific.
#[derive(Clone, Debug)]
pub struct Snapshot(pub Vec<u8>);     // or a path; opaque to core

#[derive(Clone, Debug)]
pub struct PciAddr {
    pub domain: u16, pub bus: u8, pub device: u8, pub function: u8,
}

#[derive(Clone, Debug)]
pub struct UsbDev {
    pub vendor_id: u16, pub product_id: u16,
    pub serial: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CpuMask(pub Vec<u32>);     // logical CPU ids

#[derive(Clone, Debug)]
pub enum GuestState { Created, Running, Idle, Suspended, Stopped, Failed }

#[derive(Clone, Debug)]
pub struct GuestStatus {
    pub state: GuestState,
    pub memory_mb: u64,
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

#[derive(Clone, Debug, Default)]
pub struct BackendCapabilities {
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

/// Errors are the same shape across backends. The `kind` is for the
/// orchestrator to make decisions; `detail` is for logs / users.
#[derive(Debug)]
pub struct BackendError {
    pub kind: ErrorKind,
    pub detail: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ErrorKind {
    NotSupported,        // capability advertised false
    GuestNotFound,
    GuestAlreadyRunning,
    InsufficientResources,
    HardwareUnavailable,
    PermissionDenied,
    BackendInternal,
}

pub type Result<T> = std::result::Result<T, BackendError>;

pub trait HypervisorBackend: Send + Sync {
    /// "xen" | "hyperv". Stable identifier for logs and manifest hints.
    fn name(&self) -> &str;

    /// Self-describe capabilities so the core knows what to skip.
    fn capabilities(&self) -> BackendCapabilities;

    // ---- guest lifecycle ----

    fn create_guest(&self, profile: &Profile) -> Result<GuestHandle>;
    fn destroy_guest(&self, h: &GuestHandle) -> Result<()>;
    fn start_guest(&self, h: &GuestHandle) -> Result<()>;
    fn stop_guest(&self, h: &GuestHandle, force: bool) -> Result<()>;

    // ---- resource arbitration ----

    /// Target memory in MB. Backend handles the balloon driver dance.
    fn balloon_to(&self, h: &GuestHandle, target_mb: u64) -> Result<()>;

    /// Pin vCPUs to a host CPU mask. Used to keep dom0 on E-cores and
    /// active desktop guests on P-cores.
    fn pin_vcpus(&self, h: &GuestHandle, mask: CpuMask) -> Result<()>;

    // ---- suspend / resume ----

    /// Snapshot RAM+device state to a backend-managed location.
    /// Returns an opaque token the core stores in its state DB.
    fn suspend(&self, h: &GuestHandle) -> Result<Snapshot>;

    /// Restore a previously suspended guest. The handle must match the
    /// one returned by `create_guest` for that profile.
    fn resume(&self, h: &GuestHandle, snap: Snapshot) -> Result<()>;

    // ---- passthrough ----

    /// Hot-attach a PCI device to a running guest.
    /// Returns NotSupported if `supports_hot_pci_passthrough` is false;
    /// the core then knows to schedule a stop / passthrough-at-boot /
    /// start cycle instead.
    fn passthrough_pci(&self, h: &GuestHandle, addr: PciAddr) -> Result<()>;

    /// Hot-detach. Same NotSupported semantics.
    fn revoke_pci(&self, h: &GuestHandle, addr: PciAddr) -> Result<()>;

    /// USB attach/detach for follow-focus and per-device-rule routing.
    fn attach_usb(&self, h: &GuestHandle, dev: UsbDev) -> Result<()>;
    fn detach_usb(&self, h: &GuestHandle, dev: UsbDev) -> Result<()>;

    // ---- introspection ----

    fn list(&self) -> Vec<GuestSummary>;
    fn status(&self, h: &GuestHandle) -> Result<GuestStatus>;
}
```

## Behavioural contracts

These are what the orchestrator core relies on; backends MUST honour
them or be considered broken.

### `create_guest`
- Idempotent in name: calling it twice with a profile of the same name
  must error `GuestAlreadyRunning` rather than silently producing a
  second guest. If the backend has a stale entry from a crashed
  previous orchestrator, it must be reused (handle stable across
  daemon restarts).
- MUST NOT start the guest. Use `start_guest` separately. This
  separation lets the core construct a guest, install passthrough
  devices, then start it atomically.

### `start_guest` / `stop_guest`
- `stop_guest(force=false)` SHOULD send a graceful shutdown signal and
  wait up to a backend-defined timeout (recommended 30 s) before
  returning success. If the guest doesn't shut down, return an error
  rather than silently force-killing.
- `stop_guest(force=true)` MUST kill the guest immediately. Used by
  the orchestrator only when graceful failed or the user explicitly
  requested it.

### `balloon_to`
- The target is a hint, not a guarantee. The backend MUST refuse to
  go below the guest's declared `memory_minimum`. If the request is
  below the minimum, return success with the actual achieved value
  reported via `status`, NOT an error — ballooning is a soft-state
  operation and the core handles drift via its policy loop.

### `suspend` / `resume`
- A snapshot taken by backend A MUST NOT be passed to backend B. The
  core enforces this; the backend MAY also validate. There is no
  cross-backend snapshot interchange in v0.x — that's a v2 problem if
  ever.
- Suspended guests MUST consume zero CPU and minimal memory (a small
  metadata footprint is acceptable). The whole point of suspend is to
  reclaim resources.

### `passthrough_pci`
- Devices currently held by dom0/root MUST be unbound from their
  current driver before being passed through. The backend handles
  this; the core just calls passthrough_pci.
- `revoke_pci` MUST rebind the device to its dom0/root driver after
  detach so dom0 has the device again.

### Capabilities
- Capabilities are static per backend instance. The orchestrator core
  reads them at daemon startup and caches them.
- A backend MUST NOT lie about capabilities. If `supports_balloon`
  is true, `balloon_to` must work. If false, the core won't call it.

## Reference implementations

### XenBackend (Track A)
- `name()` → `"xen"`
- Wraps `libxl` via FFI (or shells out to `xl`).
- Capabilities: balloon ✓, hot PCI ✓ (with caveats), suspend/resume ✓
  (`xl save`/`xl restore`), USB via PVUSB ✓, swtpm ✓, hardware TPM
  passthrough ✗, Hyper-V-compatible attestation ✗.
- Initial implementation may shell out to `xl`; final implementation
  should use libxl directly to avoid subprocess overhead in the
  policy loop.

### HyperVBackend (Track B)
- `name()` → `"hyperv"`
- Wraps Microsoft's HCS API via the `hcsshim` Go library or PowerShell
  cmdlets via `pwsh -Command` from the daemon.
- Capabilities: balloon ✓ (Dynamic Memory), hot PCI ✗ (DDA requires
  shutdown), USB via Enhanced Session Mode, swtpm ✓, hardware TPM
  passthrough ✗ (Hyper-V vTPM only), Hyper-V-compatible attestation ✓
  (this is the headline differentiator).
- Daemon runs as a Windows service inside the Windows root partition.
- IPC with the user-side CLI happens over Hyper-V Sockets (vmbus
  AF_HYPERV) using the same JSON-over-length-prefix protocol as Xen.

## Open questions

- **Live focus switching with PCI passthrough.** Hot-detach iGPU from
  one guest and hot-attach to another while both are running. Both
  backends advertise this with caveats; the orchestrator's "switch
  active desktop" command will go through a stop-passthrough-start
  cycle in v0.x and graduate to hot-swap when both backends prove it
  reliable.
- **Cross-backend manifest portability.** A profile that declares
  `tpm.mode = "hardware-passthrough"` and runs on Xen will fail; on
  Hyper-V will also fail (Hyper-V doesn't support hardware TPM
  passthrough either, only vTPM). The core should report this at
  manifest validation time, not at create_guest time.
