//! `XenBackend` — concrete implementation of [`HypervisorBackend`] for Xen.
//!
//! Owns a single [`Ctx`] for the life of the backend. The trait's
//! `&self` signatures meet libxl's `*mut libxl_ctx` requirement via
//! `RefCell` interior mutability — runtime borrow check, no contention
//! expected because `XenBackend: !Sync` (Ctx carries the marker).
//!
//! v0.1 scope: the read-only methods (`name`, `capabilities`, `list`)
//! are real; mutating methods stub to `BackendError::NotSupported` and
//! land in subsequent passes.

use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::time::Duration;

use rotten_apple_backend::{
    BackendError, CpuMask, GuestHandle, GuestState, GuestStatus, GuestSummary,
    HostPhysinfo, HypervisorBackend, PciAddr, Result, Snapshot, UsbDev,
};
use rotten_apple_manifest::{BackendCapabilities, Profile};

use crate::config::{DomainConfigPlan, OwnedDomainConfig};
use crate::ctx::{Ctx, map_rc};
use crate::mode::{LsblkProbe, select_mode};
use crate::sys;

pub struct XenBackend {
    ctx: RefCell<Ctx>,
}

impl XenBackend {
    /// Open a libxl context. Fails on non-Xen hosts (no `/dev/xen/privcmd`),
    /// non-root callers, or libxl version skew. Errors carry libxl's
    /// diagnostic in the `detail` field.
    pub fn new() -> Result<Self> {
        Ok(XenBackend { ctx: RefCell::new(Ctx::new()?) })
    }
}

impl HypervisorBackend for XenBackend {
    fn name(&self) -> &str { "xen" }

    fn capabilities(&self) -> BackendCapabilities {
        // The reference values from the manifest crate are the source of
        // truth: validators read them, manifests are checked against them.
        // The trait returns this directly so there is exactly one place
        // those values live.
        BackendCapabilities::xen_reference()
    }

    fn list(&self) -> Vec<GuestSummary> {
        let mut ctx = self.ctx.borrow_mut();
        let raw_ctx = ctx.raw_mut();

        // libxl_list_domain returns a heap-allocated array of dominfo
        // structs; nb_domain_out is the element count. NULL or count<=0
        // means "no domains" (or an error we silently treat as none —
        // list() is best-effort).
        let mut nb_domain: c_int = 0;
        // SAFETY: raw_ctx came from libxl_ctx_alloc and is valid; out-ptr
        // is a stack u32 we own; libxl will populate nb_domain even on
        // empty list (sets it to 0).
        let list_ptr = unsafe { sys::libxl_list_domain(raw_ctx, &mut nb_domain) };
        if list_ptr.is_null() || nb_domain <= 0 {
            return vec![];
        }

        // SAFETY: libxl_list_domain guarantees the returned pointer is
        // valid for `nb_domain` `libxl_dominfo` elements when the count
        // is positive. The slice does not outlive the array (we free
        // below before returning).
        let domains = unsafe {
            std::slice::from_raw_parts(list_ptr, nb_domain as usize)
        };

        let summaries: Vec<GuestSummary> = domains.iter()
            .map(|info| build_summary(raw_ctx, info))
            .collect();

        // SAFETY: libxl_dominfo_list_free is the matching deallocator
        // for libxl_list_domain. Frees both the array and each element's
        // owned strings (ssid_label, etc.).
        unsafe { sys::libxl_dominfo_list_free(list_ptr, nb_domain); }
        summaries
    }

    // ---- mutating methods ----

    /// Build a domain from a profile and return its handle.
    ///
    /// Flow:
    ///   1. Pick a Xen mode (PV/PVH/HVM) from profile + host signals.
    ///   2. Translate Profile → `DomainConfigPlan` (pure Rust, testable).
    ///   3. Materialize plan → `OwnedDomainConfig` (libxl-owned strings
    ///      and arrays; dispose runs in Drop on every exit path).
    ///   4. Call `libxl_domain_create_new`. The new domain is created in
    ///      *paused* state by default; the trait contract says the caller
    ///      must call `start_guest` to actually run it.
    ///   5. Return the domid as a GuestHandle.
    fn create_guest(&self, profile: &Profile) -> Result<GuestHandle> {
        let mut ctx = self.ctx.borrow_mut();
        let raw_ctx = ctx.raw_mut();

        let mode_sel = select_mode(profile, &LsblkProbe);
        let plan = DomainConfigPlan::from_profile(profile, mode_sel.mode)?;

        // SAFETY: raw_ctx is a valid libxl_ctx; OwnedDomainConfig handles
        // its own dispose lifecycle on every error path inside ::new and
        // via Drop here.
        let mut owned = unsafe { OwnedDomainConfig::new(raw_ctx, &plan)? };

        let mut domid: u32 = 0;
        // SAFETY: raw_ctx is live; owned.raw_mut() is a fully-populated
        // libxl_domain_config we just built; ao_how/aop_console_how NULL
        // means blocking semantics with no console attach. libxl reads
        // d_config and writes domid on success.
        let rc = unsafe {
            sys::libxl_domain_create_new(
                raw_ctx,
                owned.raw_mut(),
                &mut domid,
                ptr::null(),
                ptr::null(),
            )
        };
        // owned drops here regardless — dispose is required.
        if rc != 0 {
            return Err(map_rc(rc, "libxl_domain_create_new"));
        }
        Ok(GuestHandle(domid.to_string()))
    }

    fn start_guest(&self, h: &GuestHandle) -> Result<()> {
        let domid = parse_domid(h)?;
        let mut ctx = self.ctx.borrow_mut();
        // SAFETY: raw_ctx is a live libxl_ctx; ao_how=NULL gives blocking
        // semantics; `libxl_domain_unpause` only reads through the ctx.
        let rc = unsafe {
            sys::libxl_domain_unpause(ctx.raw_mut(), domid, ptr::null())
        };
        Ctx::check(rc, "libxl_domain_unpause")
    }

    fn stop_guest(&self, h: &GuestHandle, force: bool) -> Result<()> {
        let domid = parse_domid(h)?;
        let mut ctx = self.ctx.borrow_mut();
        let raw_ctx = ctx.raw_mut();
        // SAFETY: ctx is live, ao_how=NULL is blocking, the domain may
        // not exist (libxl returns ERROR_DOMAIN_NOTFOUND, mapped to
        // GuestNotFound by Ctx::check).
        let rc = if force {
            // libxl_domain_destroy is the immediate kill — equivalent to
            // pulling the power cord. Returns when the domain is gone.
            unsafe { sys::libxl_domain_destroy(raw_ctx, domid, ptr::null()) }
        } else {
            // libxl_domain_shutdown sends an ACPI shutdown signal. The
            // guest decides what to do; we don't wait here. The trait
            // contract says we SHOULD wait up to 30s for graceful, but
            // wiring that needs an event loop — TODO when we add async.
            unsafe { sys::libxl_domain_shutdown(raw_ctx, domid, ptr::null()) }
        };
        Ctx::check(rc, if force { "libxl_domain_destroy" } else { "libxl_domain_shutdown" })
    }

    fn destroy_guest(&self, h: &GuestHandle) -> Result<()> {
        // Same as stop_guest(force=true) but without the ACPI option.
        // Both call libxl_domain_destroy; this method exists in the
        // trait separately so callers can express intent ("I want it
        // gone NOW, not gracefully").
        let domid = parse_domid(h)?;
        let mut ctx = self.ctx.borrow_mut();
        // SAFETY: same as start_guest.
        let rc = unsafe {
            sys::libxl_domain_destroy(ctx.raw_mut(), domid, ptr::null())
        };
        Ctx::check(rc, "libxl_domain_destroy")
    }

    fn balloon_to(&self, h: &GuestHandle, target_mb: u64) -> Result<()> {
        let domid = parse_domid(h)?;
        // libxl wants kB. Cast to i64 (libxl supports negative values
        // for "shrink by N" when relative=1; we pass relative=0 → absolute).
        let target_kb = (target_mb * 1024) as i64;
        let mut ctx = self.ctx.borrow_mut();
        // SAFETY: target_kb is in range (well under i64::MAX); relative=0,
        // enforce=1 means "set absolute target, fail if guest's max prevents it".
        let rc = unsafe {
            sys::libxl_set_memory_target(
                ctx.raw_mut(), domid, target_kb, /* relative */ 0, /* enforce */ 1,
            )
        };
        Ctx::check(rc, "libxl_set_memory_target")
    }

    fn pin_vcpus(&self, _h: &GuestHandle, _mask: CpuMask) -> Result<()> {
        // Needs libxl_bitmap construction (variable-length, not trivial).
        // Pending — same uncertainty bucket as create_guest.
        Err(BackendError::not_supported(
            "pin_vcpus: pending — libxl_bitmap construction"))
    }

    fn suspend(&self, _h: &GuestHandle) -> Result<Snapshot> {
        // libxl_domain_suspend_only writes the snapshot to a fd. v0.1 will
        // open a temp file; not yet implemented.
        Err(BackendError::not_supported(
            "suspend: pending — needs temp-file fd plumbing"))
    }
    fn resume(&self, _h: &GuestHandle, _snap: Snapshot) -> Result<()> {
        Err(BackendError::not_supported("resume: pending"))
    }
    fn passthrough_pci(&self, _h: &GuestHandle, _addr: PciAddr) -> Result<()> {
        Err(BackendError::not_supported(
            "passthrough_pci: pending — libxl_device_pci construction"))
    }
    fn revoke_pci(&self, _h: &GuestHandle, _addr: PciAddr) -> Result<()> {
        Err(BackendError::not_supported("revoke_pci: pending"))
    }
    fn attach_usb(&self, _h: &GuestHandle, _dev: UsbDev) -> Result<()> {
        Err(BackendError::not_supported(
            "attach_usb: pending — libxl_device_usbdev construction"))
    }
    fn detach_usb(&self, _h: &GuestHandle, _dev: UsbDev) -> Result<()> {
        Err(BackendError::not_supported("detach_usb: pending"))
    }

    fn status(&self, h: &GuestHandle) -> Result<GuestStatus> {
        let domid = parse_domid(h)?;
        let mut ctx = self.ctx.borrow_mut();
        let raw_ctx = ctx.raw_mut();

        // libxl_dominfo has owned strings (ssid_label) etc.; we must
        // init-then-dispose to avoid leaking on partial fill.
        // SAFETY: zeroed() then init is the libxl-blessed pattern; dispose
        // is the matching deallocator on every exit path below.
        let mut info: sys::libxl_dominfo = unsafe { std::mem::zeroed() };
        unsafe { sys::libxl_dominfo_init(&mut info); }

        // SAFETY: raw_ctx live, info_r is our valid stack pointer, domid
        // in range. libxl populates *info on success; sets error code on failure.
        let rc = unsafe { sys::libxl_domain_info(raw_ctx, &mut info, domid) };
        if let Err(e) = Ctx::check(rc, "libxl_domain_info") {
            // SAFETY: info was init'd above; dispose is required by libxl
            // on every exit path (success OR failure).
            unsafe { sys::libxl_dominfo_dispose(&mut info); }
            return Err(e);
        }

        let result = GuestStatus {
            state: state_from_dominfo(&info),
            memory_mb: info.current_memkb / 1024,
            memory_max_mb: info.max_memkb / 1024,
            // vcpu_max_id is the highest valid id; +1 gives the count.
            vcpus: info.vcpu_max_id + 1,
            // dom0 we can read directly from /proc/uptime; for domU libxl
            // reports cumulative cpu_time but not wall-clock uptime —
            // proper tracking needs xenstore /local/domain/N/start-time
            // which we'll wire after the daemon lands. Until then domU
            // uptime is zero and the cockpit renders "—".
            uptime: if domid == 0 {
                read_proc_uptime().unwrap_or(Duration::ZERO)
            } else {
                Duration::ZERO
            },
            last_event: None,
        };

        // SAFETY: same dispose as above; second free is required.
        unsafe { sys::libxl_dominfo_dispose(&mut info); }
        Ok(result)
    }

    fn physinfo(&self) -> Result<HostPhysinfo> {
        let mut ctx = self.ctx.borrow_mut();
        let raw_ctx = ctx.raw_mut();

        // SAFETY: zeroed-then-init is the libxl pattern. dispose is
        // required on every exit path (success OR failure).
        let mut info: sys::libxl_physinfo = unsafe { std::mem::zeroed() };
        unsafe { sys::libxl_physinfo_init(&mut info); }

        // SAFETY: raw_ctx is a live ctx; info was init'd above; libxl
        // populates *info on success or sets an error code.
        let rc = unsafe { sys::libxl_get_physinfo(raw_ctx, &mut info) };
        if let Err(e) = Ctx::check(rc, "libxl_get_physinfo") {
            unsafe { sys::libxl_physinfo_dispose(&mut info); }
            return Err(e);
        }

        // libxl reports memory in 4-KB pages. Convert to MB rather than
        // KB so callers (cockpit, engine) don't need to do it everywhere.
        let pages_to_mb = |pages: u64| -> u64 { pages * 4 / 1024 };

        let result = HostPhysinfo {
            total_pcpus: info.nr_cpus,
            threads_per_core: info.threads_per_core,
            cores_per_socket: info.cores_per_socket,
            total_memory_mb: pages_to_mb(info.total_pages),
            free_memory_mb:  pages_to_mb(info.free_pages),
            scrub_memory_mb: pages_to_mb(info.scrub_pages),
        };

        // SAFETY: dispose is the matching deallocator; called on every
        // exit path. libxl_physinfo holds a heap-allocated hw_cap array.
        unsafe { sys::libxl_physinfo_dispose(&mut info); }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Helpers

/// Parse a domid out of a `GuestHandle`. Handles produced by `list()` are
/// always `domid.to_string()`; callers that synthesize handles from
/// other sources (state restore, hand-built tests) get a clean error
/// rather than a panic.
fn parse_domid(h: &GuestHandle) -> Result<u32> {
    h.0.parse::<u32>().map_err(|_| {
        BackendError::guest_not_found(format!("invalid Xen domid handle: {:?}", h.0))
    })
}

/// Translate a single libxl_dominfo entry into our GuestSummary.
fn build_summary(raw_ctx: *mut sys::libxl_ctx, info: &sys::libxl_dominfo) -> GuestSummary {
    let state = state_from_dominfo(info);
    let name = name_for_domid(raw_ctx, info.domid);
    GuestSummary {
        handle: GuestHandle(info.domid.to_string()),
        name,
        state,
    }
}

/// Map libxl's per-domain boolean flags to our typed `GuestState`. Order
/// matters — multiple flags can be true simultaneously, and we take the
/// most-specific one.
fn state_from_dominfo(info: &sys::libxl_dominfo) -> GuestState {
    if info.dying    { GuestState::Failed }
    else if info.shutdown { GuestState::Stopped }
    else if info.paused   { GuestState::Suspended }
    else if info.running  { GuestState::Running }
    else if info.blocked  { GuestState::Idle }
    else                  { GuestState::Created }
}

/// Read /proc/uptime and return the first field (seconds since boot) as
/// a Duration. dom0's uptime IS the host's uptime — Xen handed control
/// to dom0's kernel at boot, and dom0 has been counting since.
fn read_proc_uptime() -> Option<Duration> {
    let s = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_secs_f64(secs))
}

/// Look up a domain's name. libxl_domid_to_name returns a malloc'd C
/// string we MUST free with libc::free. Returns "dom{N}" if libxl can't
/// find a name (e.g. transient domains during creation).
fn name_for_domid(raw_ctx: *mut sys::libxl_ctx, domid: u32) -> String {
    // SAFETY: raw_ctx is a live libxl_ctx; libxl_domid_to_name is a pure
    // lookup that either returns NULL or a malloc'd C string we own.
    let p = unsafe { sys::libxl_domid_to_name(raw_ctx, domid) };
    if p.is_null() {
        return format!("dom{domid}");
    }
    // SAFETY: p is a valid NUL-terminated C string returned by libxl;
    // we copy out the contents before freeing.
    let name = unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() };
    // SAFETY: libxl uses libc's malloc; libc::free is the matching
    // deallocator.
    unsafe { libc::free(p as *mut c_void); }
    name
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xen_backend_new_fails_on_non_xen_host() {
        // Same failure mode as Ctx::new(). The test environment isn't
        // running under Xen, so libxl_ctx_alloc returns an error.
        let result = XenBackend::new();
        assert!(result.is_err(), "XenBackend::new() should fail in test env");
    }

    /// A backend that didn't open libxl successfully shouldn't be usable
    /// at all. We test the unimplemented stubs by constructing a backend
    /// from a Ctx we'd otherwise have built — but since we can't, we
    /// verify capability/name through a different path: those methods
    /// that don't need a live ctx.
    #[test]
    fn parse_domid_round_trips_well_formed() {
        let h = GuestHandle("42".into());
        assert_eq!(parse_domid(&h).unwrap(), 42);
    }

    #[test]
    fn parse_domid_rejects_non_numeric() {
        let h = GuestHandle("ubuntu-desktop".into());
        let err = parse_domid(&h).unwrap_err();
        assert_eq!(err.kind, rotten_apple_backend::ErrorKind::GuestNotFound);
    }

    #[test]
    fn capabilities_match_manifest_xen_reference() {
        // We can't construct XenBackend without a live ctx, so we test
        // the reference values against the manifest crate directly. If
        // these drift, the orchestrator's pre-flight validation against
        // BackendCapabilities will get out of sync with the actual
        // backend behaviour.
        let mf = BackendCapabilities::xen_reference();
        assert_eq!(mf.backend_name, "xen");
        assert!(mf.supports_balloon);
        assert!(mf.supports_pci_passthrough_at_boot);
        assert!(mf.supports_swtpm);
        assert!(!mf.supports_hyperv_compatible_attestation);
        assert!(!mf.supports_hardware_tpm_passthrough);
    }
}
