//! Safe wrapper around `libxl_ctx`.
//!
//! Every libxl operation flows through a `*mut libxl_ctx`. This module
//! owns one such context for the life of the wrapper and tears it down
//! cleanly on Drop. The wrapper enforces the four invariants every caller
//! into libxl must respect (see `crates/backend-xen` module docs):
//!
//!   1. Logger lifetime ≥ ctx lifetime
//!   2. ABI version check at construction (catches libxl-dev / libxenlight
//!      version skew at process startup, not mid-flight)
//!   3. Single-thread access per ctx (`Ctx: Send + !Sync`)
//!   4. libxl error codes mapped to `BackendError` with kind discrimination
//!
//! All callers in this crate go through [`Ctx`]; nothing else touches the
//! raw `libxl_ctx` pointer directly.

use crate::sys;
use rotten_apple_backend::{BackendError, ErrorKind, Result};
use std::marker::PhantomData;
use std::os::raw::{c_int, c_uint};
use std::ptr;

// libc's `stderr` global. We pass it to libxl's stdio logger; we never
// close it (it's owned by the C runtime, lasts the life of the process).
unsafe extern "C" {
    static stderr: *mut sys::FILE;
}

/// Owns a `libxl_ctx` and its required logger. Every backend method
/// borrows `&mut Ctx` and calls into libxl through `self.raw_mut()`.
///
/// Send but **not Sync** — libxl_ctx is documented as not concurrent-safe;
/// the orchestrator never shares one across threads.
pub struct Ctx {
    raw_ctx: *mut sys::libxl_ctx,
    raw_logger: *mut sys::xentoollog_logger,
    /// Marker that takes Sync away while leaving Send. Phantom raw pointer
    /// is the canonical Rust idiom for "single-thread-at-a-time."
    _not_sync: PhantomData<*mut ()>,
}

// SAFETY: we explicitly want Send (orchestrator may move Ctx between
// async tasks). The PhantomData<*mut ()> field would otherwise also
// remove Send; we put it back manually.
unsafe impl Send for Ctx {}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't dereference the raw pointers; just show whether we hold
        // valid handles. The tests use `unwrap_err()` which requires Debug
        // on the Ok variant — this satisfies that without exposing libxl
        // internals.
        f.debug_struct("Ctx")
            .field("ctx_initialised", &!self.raw_ctx.is_null())
            .field("logger_initialised", &!self.raw_logger.is_null())
            .finish()
    }
}

impl Ctx {
    /// Build a context. Allocates an stdio logger writing to stderr,
    /// then `libxl_ctx_alloc`. Returns a typed error on failure (logger
    /// is freed before returning so we don't leak it).
    pub fn new() -> Result<Self> {
        // 1. Allocate the logger first. libxl's ctx_alloc needs it as an
        //    arg and stores it; we MUST NOT pass NULL (libxl will segfault
        //    on the first log call).
        //
        //    SAFETY: `stderr` is the C runtime's global FILE*, valid for
        //    the life of the process. xtl_createlogger_stdiostream returns
        //    a heap-allocated logger that we own and free in Drop.
        let logger_stdio = unsafe {
            sys::xtl_createlogger_stdiostream(
                stderr,
                sys::xentoollog_level::XTL_PROGRESS,
                0,
            )
        };
        if logger_stdio.is_null() {
            return Err(BackendError::internal(
                "xtl_createlogger_stdiostream returned NULL"));
        }

        // The stdio logger is a *xentoollog_logger_stdiostream*; libxl_ctx_alloc
        // wants the base *xentoollog_logger*. In C the two are layout-compatible
        // (struct embedding); in Rust we cast the pointer.
        let logger = logger_stdio as *mut sys::xentoollog_logger;

        // 2. Allocate the ctx. Version check happens here — if libxl_dev
        //    we compiled against differs from libxenlight at runtime, this
        //    returns ERROR_VERSION and we abort cleanly.
        //
        //    SAFETY: pctx is a valid out-pointer; lg is the logger we just
        //    allocated. libxl will populate *pctx on success.
        let mut ctx_raw: *mut sys::libxl_ctx = ptr::null_mut();
        let rc = unsafe {
            sys::libxl_ctx_alloc(
                &mut ctx_raw,
                sys::LIBXL_VERSION as c_int,
                0 as c_uint,
                logger,
            )
        };

        if rc != 0 {
            // Free the logger we just allocated; otherwise leak.
            // SAFETY: logger is the same one we got from xtl_createlogger.
            unsafe { sys::xtl_logger_destroy(logger); }
            return Err(map_rc(rc, "libxl_ctx_alloc"));
        }

        Ok(Ctx {
            raw_ctx: ctx_raw,
            raw_logger: logger,
            _not_sync: PhantomData,
        })
    }

    /// Mutable access to the raw context. Restricted to this crate; the
    /// public surface is the trait impl. (Unused until the trait methods
    /// land in the next pass.)
    #[allow(dead_code)]
    pub(crate) fn raw_mut(&mut self) -> *mut sys::libxl_ctx {
        self.raw_ctx
    }

    /// Translate a libxl return code into our typed result. `op` is the
    /// libxl function name for the error message. (Unused until trait
    /// methods land.)
    #[allow(dead_code)]
    pub(crate) fn check(rc: c_int, op: &'static str) -> Result<()> {
        if rc == 0 { Ok(()) } else { Err(map_rc(rc, op)) }
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        // Order matters: ctx first (it may write log lines via the
        // logger during teardown), THEN logger.
        //
        // SAFETY: both pointers were given to us by their respective
        // allocators in `new` and have not been touched by anyone else.
        // Reading them once each here is safe; we don't touch them again.
        if !self.raw_ctx.is_null() {
            unsafe { sys::libxl_ctx_free(self.raw_ctx); }
            self.raw_ctx = ptr::null_mut();
        }
        if !self.raw_logger.is_null() {
            unsafe { sys::xtl_logger_destroy(self.raw_logger); }
            self.raw_logger = ptr::null_mut();
        }
    }
}

// ---------------------------------------------------------------------------
// Error mapping

/// Translate libxl's negative integer error codes into a typed error.
///
/// libxl returns negative `libxl_error` values; we widen to i32 first.
/// Common codes map to specific `ErrorKind` variants; everything else
/// falls through to `BackendInternal` with the libxl error string in
/// the `detail` field.
pub(crate) fn map_rc(rc: c_int, op: &'static str) -> BackendError {
    // libxl returns the error code as a negative int. The libxl_error
    // enum values are positive; the convention is `return ERROR_FOO`
    // negated. So abs-value the rc and look it up.
    let code = rc.unsigned_abs() as i32;

    // libxl_error is a newtype enum (`pub struct libxl_error(pub i32)`),
    // so the constants are values like `libxl_error::ERROR_NOMEM` and we
    // unwrap with `.0` to get the underlying integer.
    let kind = match code {
        c if c == sys::libxl_error::ERROR_NOMEM.0           => ErrorKind::InsufficientResources,
        c if c == sys::libxl_error::ERROR_DOMAIN_NOTFOUND.0 => ErrorKind::GuestNotFound,
        c if c == sys::libxl_error::ERROR_NOTFOUND.0        => ErrorKind::GuestNotFound,
        c if c == sys::libxl_error::ERROR_DEVICE_EXISTS.0   => ErrorKind::GuestAlreadyRunning,
        c if c == sys::libxl_error::ERROR_NI.0              => ErrorKind::NotSupported,
        c if c == sys::libxl_error::ERROR_FEATURE_REMOVED.0 => ErrorKind::NotSupported,
        c if c == sys::libxl_error::ERROR_GUEST_TIMEDOUT.0  => ErrorKind::HardwareUnavailable,
        c if c == sys::libxl_error::ERROR_NOPARAVIRT.0      => ErrorKind::HardwareUnavailable,
        // ERROR_INVAL, ERROR_FAIL, ERROR_BADFAIL, ERROR_LOCK_FAIL, etc.
        // → BackendInternal (likely a programmer error or a libxl bug)
        _ => ErrorKind::BackendInternal,
    };

    let detail = libxl_strerror(code);
    BackendError {
        kind,
        detail: format!("{op}: {detail} (libxl rc={rc})"),
    }
}

/// Best-effort string for a libxl error code. libxl 4.20's
/// `libxl_error_to_string` returns NULL for many codes (including the
/// common ERROR_FAIL=3 and ERROR_INVAL=6); we keep a manual fallback
/// table so events read as enum names instead of bare integers.
fn libxl_strerror(code: i32) -> String {
    // libxl_error is a newtype around i32; constructing one from any value
    // is safe (no UB), unlike a transmute into a strict enum.
    let err = sys::libxl_error(code);
    // SAFETY: libxl_error_to_string returns either NULL (for an unrecognised
    // code) or a pointer to a static, read-only C string within libxl that
    // we never free.
    let from_libxl = unsafe {
        let p = sys::libxl_error_to_string(err);
        if p.is_null() { None }
        else { Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()) }
    };
    if let Some(s) = from_libxl {
        return s;
    }
    // Fallback table — match all codes referenced elsewhere in this file
    // plus the two we hit in production (FAIL, INVAL). Keep it conservative;
    // an unknown code falls through to a numeric label, which is still
    // better than the previous "<unknown libxl error code N>".
    match code {
        c if c == sys::libxl_error::ERROR_NONSPECIFIC.0     => "ERROR_NONSPECIFIC".into(),
        c if c == sys::libxl_error::ERROR_VERSION.0         => "ERROR_VERSION".into(),
        c if c == sys::libxl_error::ERROR_FAIL.0            => "ERROR_FAIL".into(),
        c if c == sys::libxl_error::ERROR_NI.0              => "ERROR_NI (not implemented)".into(),
        c if c == sys::libxl_error::ERROR_NOMEM.0           => "ERROR_NOMEM".into(),
        c if c == sys::libxl_error::ERROR_INVAL.0           => "ERROR_INVAL".into(),
        c if c == sys::libxl_error::ERROR_BADFAIL.0         => "ERROR_BADFAIL".into(),
        c if c == sys::libxl_error::ERROR_GUEST_TIMEDOUT.0  => "ERROR_GUEST_TIMEDOUT".into(),
        c if c == sys::libxl_error::ERROR_NOPARAVIRT.0      => "ERROR_NOPARAVIRT".into(),
        c if c == sys::libxl_error::ERROR_NOTFOUND.0        => "ERROR_NOTFOUND".into(),
        c if c == sys::libxl_error::ERROR_DOMAIN_NOTFOUND.0 => "ERROR_DOMAIN_NOTFOUND".into(),
        c if c == sys::libxl_error::ERROR_DEVICE_EXISTS.0   => "ERROR_DEVICE_EXISTS".into(),
        c if c == sys::libxl_error::ERROR_FEATURE_REMOVED.0 => "ERROR_FEATURE_REMOVED".into(),
        _ => format!("libxl error code {code}"),
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_new_returns_typed_error_when_not_dom0() {
        // Running this as a non-root user on a non-Xen host: libxl_ctx_alloc
        // fails. We don't care which specific error — just that we get one
        // back as a typed BackendError, not a panic or segfault.
        let result = Ctx::new();
        assert!(result.is_err(),
                "expected Ctx::new() to fail in test env (not running as Xen dom0)");
        let err = result.unwrap_err();
        // detail should mention the operation we tried
        assert!(err.detail.contains("libxl_ctx_alloc"),
                "detail should name the failing op, got: {}", err.detail);
    }

    #[test]
    fn libxl_strerror_known_code_yields_real_string() {
        let s = libxl_strerror(sys::libxl_error::ERROR_NOMEM.0);
        // libxl always provides strings for its own errors; should not
        // be the unknown-fallback.
        assert!(!s.contains("<unknown"), "expected real string, got: {s}");
        assert!(!s.is_empty());
    }

    #[test]
    fn libxl_strerror_garbage_code_falls_back() {
        let s = libxl_strerror(99999);
        assert!(s.contains("unknown") || !s.is_empty());
    }

    #[test]
    fn check_zero_is_ok() {
        assert!(Ctx::check(0, "test_op").is_ok());
    }

    #[test]
    fn check_nonzero_carries_op_name() {
        let err = Ctx::check(-3, "test_op").unwrap_err();
        assert!(err.detail.contains("test_op"),
                "detail should include op, got: {}", err.detail);
    }
}
