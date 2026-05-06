//! Single-actor libxl owner.
//!
//! libxl_ctx is `!Sync`; exactly one thread can call into it. The actor
//! owns the `XenBackend` (or `None` if libxl couldn't be opened) and
//! serves requests sequentially over an mpsc. Multiple concurrent
//! connections clone an `ActorHandle` and queue requests there — the
//! channel is the serialization point.
//!
//! The actor never panics: every libxl call is fallible, every error
//! is mapped to an `ActorError`. If the actor thread dies anyway, the
//! oneshot sender drops and clients see `ActorError::ActorCrashed`
//! on the next request rather than a hang.
//!
//! Wire types (`DomainInfo`, `HostInfo`) are translated from the
//! backend trait's types at the actor boundary so they remain stable
//! across trait churn.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use serde::Serialize;

use rotten_apple_backend::{
    BackendError, ErrorKind, GuestHandle, GuestState, HypervisorBackend,
};
use rotten_apple_backend_xen::XenBackend;
use rotten_apple_manifest::Profile;

use crate::oneshot;

// ---------------------------------------------------------------------------
// Wire types

#[derive(Debug, Clone, Serialize)]
pub struct DomainInfo {
    pub domid: u32,
    pub name: String,
    pub state: &'static str,
    pub memory_mb: u64,
    pub memory_max_mb: u64,
    pub vcpus: u32,
    pub uptime_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    /// "xen" if libxl opened cleanly at startup, else "unavailable".
    pub backend: &'static str,
    pub libxl_version: &'static str,
    pub running_under_xen: bool,
    pub dom0_uptime_seconds: u64,
}

/// Aggregate host resources for the cockpit's `[H]` view + the engine's
/// feasibility checks. Populated from `backend.physinfo()`. Fields are
/// `None` when the backend isn't available (we still respond — clients
/// can show "—" rather than failing the whole frame).
#[derive(Debug, Clone, Serialize)]
pub struct HostResources {
    pub total_pcpus: Option<u32>,
    pub threads_per_core: Option<u32>,
    pub cores_per_socket: Option<u32>,
    pub total_memory_mb: Option<u64>,
    pub free_memory_mb: Option<u64>,
    pub scrub_memory_mb: Option<u64>,
}

// ---------------------------------------------------------------------------
// Actor errors

/// Failure modes a client can see. Maps 1:1 to JSON-RPC error codes in
/// `dispatch::error_for`.
#[derive(Debug, Clone)]
pub enum ActorError {
    /// `XenBackend::new()` failed at daemon startup. Returned for every
    /// libxl-touching method until the daemon is restarted.
    BackendUnavailable(String),
    /// libxl returned an error we couldn't classify. Detail is the libxl
    /// diagnostic string from `BackendError::detail`.
    BackendInternal(String),
    PermissionDenied(String),
    GuestNotFound(String),
    GuestAlreadyRunning(String),
    InsufficientResources(String),
    HardwareUnavailable(String),
    /// The actor thread died (panicked or exited unexpectedly). Should
    /// never happen — if it does, we surface it cleanly instead of
    /// hanging the client.
    ActorCrashed,
}

impl ActorError {
    fn from_backend(e: BackendError) -> Self {
        match e.kind {
            ErrorKind::GuestNotFound         => ActorError::GuestNotFound(e.detail),
            ErrorKind::GuestAlreadyRunning   => ActorError::GuestAlreadyRunning(e.detail),
            ErrorKind::InsufficientResources => ActorError::InsufficientResources(e.detail),
            ErrorKind::HardwareUnavailable   => ActorError::HardwareUnavailable(e.detail),
            ErrorKind::PermissionDenied      => ActorError::PermissionDenied(e.detail),
            // NotSupported / BackendInternal both fall through here —
            // from a daemon-RPC perspective they're "libxl said no".
            _ => ActorError::BackendInternal(e.detail),
        }
    }
}

// ---------------------------------------------------------------------------
// Request enum

pub enum ActorRequest {
    HostInfo {
        reply: oneshot::Sender<Result<HostInfo, ActorError>>,
    },
    HostResources {
        reply: oneshot::Sender<Result<HostResources, ActorError>>,
    },
    DomainList {
        reply: oneshot::Sender<Result<Vec<DomainInfo>, ActorError>>,
    },
    DomainGet {
        domid: u32,
        reply: oneshot::Sender<Result<DomainInfo, ActorError>>,
    },
    DomainStart {
        domid: u32,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    DomainShutdown {
        domid: u32,
        force: bool,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    DomainBalloon {
        domid: u32,
        target_kb: u64,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    DomainCreate {
        // Boxed because Profile is ~900 bytes; keeps the enum compact.
        profile: Box<Profile>,
        reply: oneshot::Sender<Result<u32, ActorError>>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Handle

/// Cloneable handle to the actor. Each connection thread holds one;
/// the underlying mpsc serializes everything onto the actor thread.
#[derive(Clone)]
pub struct ActorHandle {
    tx: Sender<ActorRequest>,
}

impl ActorHandle {
    fn send_and_recv<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, ActorError>>) -> ActorRequest,
    ) -> Result<T, ActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.tx.send(build(reply_tx)).is_err() {
            return Err(ActorError::ActorCrashed);
        }
        match reply_rx.recv() {
            Ok(r) => r,
            Err(_) => Err(ActorError::ActorCrashed),
        }
    }

    pub fn host_info(&self) -> Result<HostInfo, ActorError> {
        self.send_and_recv(|reply| ActorRequest::HostInfo { reply })
    }

    pub fn host_resources(&self) -> Result<HostResources, ActorError> {
        self.send_and_recv(|reply| ActorRequest::HostResources { reply })
    }

    pub fn domain_list(&self) -> Result<Vec<DomainInfo>, ActorError> {
        self.send_and_recv(|reply| ActorRequest::DomainList { reply })
    }

    pub fn domain_get(&self, domid: u32) -> Result<DomainInfo, ActorError> {
        self.send_and_recv(|reply| ActorRequest::DomainGet { domid, reply })
    }

    pub fn domain_start(&self, domid: u32) -> Result<(), ActorError> {
        self.send_and_recv(|reply| ActorRequest::DomainStart { domid, reply })
    }

    pub fn domain_shutdown(&self, domid: u32, force: bool) -> Result<(), ActorError> {
        self.send_and_recv(|reply| ActorRequest::DomainShutdown { domid, force, reply })
    }

    pub fn domain_balloon(&self, domid: u32, target_kb: u64) -> Result<(), ActorError> {
        self.send_and_recv(|reply| {
            ActorRequest::DomainBalloon { domid, target_kb, reply }
        })
    }

    pub fn domain_create(&self, profile: Profile) -> Result<u32, ActorError> {
        let profile = Box::new(profile);
        self.send_and_recv(|reply| ActorRequest::DomainCreate { profile, reply })
    }

    /// Tell the actor thread to exit. Best-effort — if the channel is
    /// already closed (actor died) this is a no-op.
    pub fn shutdown(&self) {
        let _ = self.tx.send(ActorRequest::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Actor thread

/// Spawn the actor thread. Returns the handle; the caller keeps it alive
/// for the daemon's lifetime and clones it per connection.
///
/// `XenBackend::new()` is attempted on the actor thread (not the caller's)
/// because libxl_ctx is `!Sync` — if we built it here we'd then have to
/// move it onto the worker, which works but is needless ceremony.
/// Failures are stored as `None` and surfaced as `BackendUnavailable`
/// per request rather than killing the daemon.
pub fn spawn() -> ActorHandle {
    let (tx, rx) = mpsc::channel::<ActorRequest>();
    thread::Builder::new()
        .name("orchestratord-actor".into())
        .spawn(move || actor_loop(rx))
        .expect("spawn orchestratord-actor");
    ActorHandle { tx }
}

fn actor_loop(rx: Receiver<ActorRequest>) {
    // BackendUnavailable is a steady state: the daemon keeps running
    // and every libxl-touching method returns the error rather than
    // hanging. host.info still responds with backend="unavailable".
    let backend: Option<XenBackend> = match XenBackend::new() {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("[orchestratord] libxl unavailable: {e}");
            None
        }
    };

    while let Ok(req) = rx.recv() {
        match req {
            ActorRequest::Shutdown => return,
            ActorRequest::HostInfo { reply } => {
                let _ = reply.send(Ok(build_host_info(&backend)));
            }
            ActorRequest::HostResources { reply } => {
                // Always Ok — we return None-fields when the backend
                // isn't available so cockpit can render `—` cleanly.
                let _ = reply.send(Ok(build_host_resources(&backend)));
            }
            ActorRequest::DomainList { reply } => match backend.as_ref() {
                None => {
                    let _ = reply.send(Err(ActorError::BackendUnavailable(
                        "libxl_ctx not opened at daemon startup".into(),
                    )));
                }
                Some(b) => {
                    let summaries = b.list();
                    let mut out = Vec::with_capacity(summaries.len());
                    let mut err: Option<ActorError> = None;
                    for s in summaries {
                        match b.status(&s.handle) {
                            Ok(st) => {
                                let domid = s.handle.0.parse::<u32>().unwrap_or(0);
                                out.push(DomainInfo {
                                    domid,
                                    name: s.name,
                                    state: state_str(&st.state),
                                    memory_mb: st.memory_mb,
                                    memory_max_mb: st.memory_max_mb,
                                    vcpus: st.vcpus,
                                    uptime_seconds: st.uptime.as_secs(),
                                });
                            }
                            Err(e) => {
                                // A domain that vanished between list+status
                                // is not fatal; everything else we surface.
                                if e.kind != ErrorKind::GuestNotFound {
                                    err = Some(ActorError::from_backend(e));
                                    break;
                                }
                            }
                        }
                    }
                    let _ = reply.send(match err {
                        Some(e) => Err(e),
                        None => Ok(out),
                    });
                }
            },
            ActorRequest::DomainGet { domid, reply } => {
                let _ = reply.send(domain_get(&backend, domid));
            }
            ActorRequest::DomainStart { domid, reply } => match backend.as_ref() {
                None => { let _ = reply.send(Err(unavail())); }
                Some(b) => {
                    let r = b.start_guest(&GuestHandle(domid.to_string()))
                        .map_err(ActorError::from_backend);
                    let _ = reply.send(r);
                }
            },
            ActorRequest::DomainShutdown { domid, force, reply } => match backend.as_ref() {
                None => { let _ = reply.send(Err(unavail())); }
                Some(b) => {
                    let r = b.stop_guest(&GuestHandle(domid.to_string()), force)
                        .map_err(ActorError::from_backend);
                    let _ = reply.send(r);
                }
            },
            ActorRequest::DomainBalloon { domid, target_kb, reply } => match backend.as_ref() {
                None => { let _ = reply.send(Err(unavail())); }
                Some(b) => {
                    // The trait wants MB; libxl's underlying call uses kB.
                    // Round to MB so we don't lose precision on the wire
                    // contract that's already in the trait.
                    let target_mb = target_kb / 1024;
                    let r = b.balloon_to(&GuestHandle(domid.to_string()), target_mb)
                        .map_err(ActorError::from_backend);
                    let _ = reply.send(r);
                }
            },
            ActorRequest::DomainCreate { profile, reply } => match backend.as_ref() {
                None => { let _ = reply.send(Err(unavail())); }
                Some(b) => {
                    let r = b.create_guest(profile.as_ref())
                        .map_err(ActorError::from_backend)
                        .and_then(|h| {
                            // GuestHandle is opaque to the daemon, but the
                            // Xen backend stores domid-as-string. Parse it
                            // back so the wire surface stays u32-keyed.
                            h.0.parse::<u32>().map_err(|_| {
                                ActorError::BackendInternal(format!(
                                    "non-numeric guest handle: {:?}", h.0
                                ))
                            })
                        });
                    let _ = reply.send(r);
                }
            },
        }
    }
}

fn unavail() -> ActorError {
    ActorError::BackendUnavailable(
        "libxl_ctx not opened at daemon startup".into(),
    )
}

fn build_host_info(backend: &Option<XenBackend>) -> HostInfo {
    let running_under_xen = backend.is_some();
    HostInfo {
        backend: if running_under_xen { "xen" } else { "unavailable" },
        libxl_version: rotten_apple_backend_xen::compat::LIBXL_BUILD_VERSION,
        running_under_xen,
        dom0_uptime_seconds: read_proc_uptime().map(|d| d.as_secs()).unwrap_or(0),
    }
}

fn build_host_resources(backend: &Option<XenBackend>) -> HostResources {
    match backend.as_ref().and_then(|b| b.physinfo().ok()) {
        Some(p) => HostResources {
            total_pcpus: Some(p.total_pcpus),
            threads_per_core: Some(p.threads_per_core),
            cores_per_socket: Some(p.cores_per_socket),
            total_memory_mb: Some(p.total_memory_mb),
            free_memory_mb: Some(p.free_memory_mb),
            scrub_memory_mb: Some(p.scrub_memory_mb),
        },
        None => HostResources {
            total_pcpus: None, threads_per_core: None, cores_per_socket: None,
            total_memory_mb: None, free_memory_mb: None, scrub_memory_mb: None,
        },
    }
}

fn domain_get(backend: &Option<XenBackend>, domid: u32) -> Result<DomainInfo, ActorError> {
    let b = backend.as_ref().ok_or_else(unavail)?;
    let handle = GuestHandle(domid.to_string());
    let status = b.status(&handle).map_err(ActorError::from_backend)?;
    // Name lookup: walk the list once. This is O(N) per get; fine for
    // the single-host scale we target. If it ever matters we can plumb
    // a `name_for_domid` through the trait.
    let name = b.list().into_iter()
        .find(|s| s.handle == handle)
        .map(|s| s.name)
        .unwrap_or_else(|| format!("dom{domid}"));
    Ok(DomainInfo {
        domid,
        name,
        state: state_str(&status.state),
        memory_mb: status.memory_mb,
        memory_max_mb: status.memory_max_mb,
        vcpus: status.vcpus,
        uptime_seconds: status.uptime.as_secs(),
    })
}

fn state_str(s: &GuestState) -> &'static str {
    match s {
        GuestState::Created   => "Created",
        GuestState::Running   => "Running",
        GuestState::Idle      => "Idle",
        GuestState::Suspended => "Suspended",
        GuestState::Stopped   => "Stopped",
        GuestState::Failed    => "Failed",
    }
}

fn read_proc_uptime() -> Option<Duration> {
    let s = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_secs_f64(secs))
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_str_covers_all_variants() {
        assert_eq!(state_str(&GuestState::Created), "Created");
        assert_eq!(state_str(&GuestState::Running), "Running");
        assert_eq!(state_str(&GuestState::Idle), "Idle");
        assert_eq!(state_str(&GuestState::Suspended), "Suspended");
        assert_eq!(state_str(&GuestState::Stopped), "Stopped");
        assert_eq!(state_str(&GuestState::Failed), "Failed");
    }

    #[test]
    fn from_backend_maps_kinds() {
        let e = BackendError::guest_not_found("x");
        assert!(matches!(ActorError::from_backend(e), ActorError::GuestNotFound(_)));
        let e = BackendError::permission_denied("x");
        assert!(matches!(ActorError::from_backend(e), ActorError::PermissionDenied(_)));
        let e = BackendError::insufficient_resources("x");
        assert!(matches!(ActorError::from_backend(e), ActorError::InsufficientResources(_)));
        let e = BackendError::not_supported("x");
        // NotSupported folds into BackendInternal at the daemon boundary.
        assert!(matches!(ActorError::from_backend(e), ActorError::BackendInternal(_)));
    }

    #[test]
    fn actor_starts_and_responds_to_host_info_without_xen() {
        // Test env has no Xen → backend is None → host.info still succeeds
        // and reports backend="unavailable".
        let h = spawn();
        let info = h.host_info().expect("host_info");
        assert_eq!(info.backend, "unavailable");
        assert!(!info.running_under_xen);
        h.shutdown();
    }

    #[test]
    fn actor_returns_backend_unavailable_for_libxl_methods() {
        let h = spawn();
        let err = h.domain_list().unwrap_err();
        assert!(matches!(err, ActorError::BackendUnavailable(_)));
        let err = h.domain_get(0).unwrap_err();
        assert!(matches!(err, ActorError::BackendUnavailable(_)));
        h.shutdown();
    }
}
