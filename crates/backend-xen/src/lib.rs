//! Xen backend for rotten-apple.
//!
//! Implements [`rotten_apple_backend::HypervisorBackend`] via libxl FFI.
//!
//! Layering:
//!   - `sys` — raw bindings generated at build time by bindgen
//!     against /usr/include/libxl.h. Unsafe, C-shaped.
//!   - `ctx` — safe wrapper around `libxl_ctx` (the per-process Xen
//!     control context). Manages init/teardown and logger setup.
//!   - `XenBackend` — the public type that implements `HypervisorBackend`.
//!     Owns a `ctx::Ctx` and translates trait calls into libxl calls.
//!
//! libxl is documented as not thread-safe per ctx; `XenBackend: !Sync` is
//! the safe assumption. We expose `Send` so the orchestrator can move it
//! between async tasks but not share it concurrently.

/// Raw libxl bindings. Generated from `wrapper.h` by `build.rs`. Do not
/// touch anything in here directly outside of `ctx`.
///
/// The lint allows here are scoped to *only* the generated code so that
/// the safe wrapper modules (everything outside `sys`) remain held to
/// the project's normal style and unsafe rules.
#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(dead_code)]                 // wide binding surface; most unused at this stage
#[allow(unsafe_op_in_unsafe_fn)]    // bindgen output is Rust-2021-shaped
#[allow(unnecessary_transmutes)]    // bindgen uses transmute for tagged unions
#[allow(clippy::all)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/libxl_sys.rs"));
}

pub mod compat;
pub mod config;
pub mod ctx;
pub mod backend;
pub mod mode;

pub use backend::XenBackend;
pub use ctx::Ctx;
pub use mode::{DiskProbe, LsblkProbe, ModeSelection, XenDomainMode, select_mode};

#[cfg(test)]
mod bindings_sanity {
    use super::*;

    #[test]
    fn libxl_uuid_has_expected_size() {
        // libxl_uuid is a fixed 16-byte structure (RFC 4122).
        assert_eq!(std::mem::size_of::<sys::libxl_uuid>(), 16);
    }

    #[test]
    fn key_libxl_types_resolve() {
        let _: Option<sys::libxl_ctx>            = None;
        let _: Option<sys::libxl_domain_config>  = None;
        let _: Option<sys::libxl_dominfo>        = None;
    }
}
