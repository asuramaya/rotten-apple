//! Version compatibility shim for libxl struct accessors.
//!
//! libxl's documented C API (functions, init/dispose pairs, error codes)
//! is stable across Xen minor releases. The internals of structs like
//! `libxl_domain_config` are NOT — fields appear, get renamed, move
//! between unions across 4.17 → 4.18 → 4.19 → 4.20 → 4.21.
//!
//! Strategy: any code in this crate that touches a libxl struct field
//! goes through a helper here. Helpers carry `#[cfg(xen_4_xx)]` gates
//! when a field's name or location differs by version. If 4.20 and 4.21
//! agree on a field, the helper has no cfg; if they diverge, two helpers
//! with mutually-exclusive cfgs produce the same logical effect.
//!
//! `build.rs` emits these cfg flags by reading `pkg-config --modversion
//! xenlight`:
//!
//!   - `xen_major` / `xen_minor` — string values (rarely used directly)
//!   - `xen_4_20` etc. — exact-match flag for one specific minor
//!   - `xen_4_19_or_later` etc. — cumulative flag for "version ≥ X.Y"
//!
//! Tests run against the build host's libxl, so a missing field in a
//! newer libxl that we never updated for is caught at compile time.

#![allow(dead_code)]

use crate::sys;

/// Compile-time string of the libxl this crate is built against, e.g.
/// `"4.20"`. Useful in error messages and the `name()` trait method.
#[cfg(xen_4_21)]
pub const LIBXL_BUILD_VERSION: &str = "4.21";
#[cfg(all(xen_4_20, not(xen_4_21)))]
pub const LIBXL_BUILD_VERSION: &str = "4.20";
#[cfg(all(xen_4_19, not(xen_4_20), not(xen_4_21)))]
pub const LIBXL_BUILD_VERSION: &str = "4.19";
#[cfg(all(xen_4_18, not(xen_4_19), not(xen_4_20), not(xen_4_21)))]
pub const LIBXL_BUILD_VERSION: &str = "4.18";
#[cfg(all(xen_4_17, not(xen_4_18), not(xen_4_19), not(xen_4_20), not(xen_4_21)))]
pub const LIBXL_BUILD_VERSION: &str = "4.17";
#[cfg(not(any(xen_4_17, xen_4_18, xen_4_19, xen_4_20, xen_4_21)))]
pub const LIBXL_BUILD_VERSION: &str = "unknown";

// ---------------------------------------------------------------------------
// libxl_domain_create_info accessors
//
// `c_info.type_` is the domain type discriminant (PV / PVH / HVM). The
// field name has been `type_` since at least 4.17 (the trailing
// underscore is bindgen's escaping of the C keyword `type`); shape may
// change later. We hide that here so callers say `set_type(c_info, …)`.

/// # Safety
///
/// `c_info` must be a properly initialized (or freshly zeroed) libxl
/// `libxl_domain_create_info` whose layout matches the libxl this crate
/// was built against; writing through the reference mutates a C struct
/// that libxl itself will later read.
pub unsafe fn set_create_info_type(
    c_info: &mut sys::libxl_domain_create_info,
    ty: sys::libxl_domain_type,
) {
    c_info.type_ = ty;
}

/// # Safety
///
/// `c_info` must be a properly initialized libxl `libxl_domain_create_info`
/// whose layout matches the libxl this crate was built against.
pub unsafe fn get_create_info_type(
    c_info: &sys::libxl_domain_create_info,
) -> sys::libxl_domain_type {
    c_info.type_
}

// ---------------------------------------------------------------------------
// libxl_domain_build_info accessors
//
// `b_info.type_` mirrors `c_info.type_`; `b_info.u` is the per-type union
// (`hvm`, `pv`, `pvh`). Field NAMES inside the union are the most likely
// thing to drift between minor versions — when that happens, helpers like
// `set_pvh_bootloader` get a cfg fork.

/// # Safety
///
/// `b_info` must be a properly initialized (or freshly zeroed) libxl
/// `libxl_domain_build_info` whose layout matches the libxl this crate
/// was built against.
pub unsafe fn set_build_info_type(
    b_info: &mut sys::libxl_domain_build_info,
    ty: sys::libxl_domain_type,
) {
    b_info.type_ = ty;
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_is_concrete() {
        // Either we built against a known minor (4.17–4.21) or we abort.
        // "unknown" should never appear on a configured CI machine; if it
        // does, the cfg gate is broken.
        assert!(LIBXL_BUILD_VERSION != "unknown",
                "build.rs should have set xen_4_xx cfg; got {LIBXL_BUILD_VERSION:?}");
    }

    #[test]
    fn create_info_type_round_trip() {
        // SAFETY: zeroed struct of POD layout; libxl's *_init normally
        // does field-specific init but for an enum field that only takes
        // valid discriminants, zeroing is sufficient for round-trip.
        unsafe {
            let mut c: sys::libxl_domain_create_info = std::mem::zeroed();
            set_create_info_type(&mut c, sys::libxl_domain_type::LIBXL_DOMAIN_TYPE_PVH);
            assert_eq!(get_create_info_type(&c),
                       sys::libxl_domain_type::LIBXL_DOMAIN_TYPE_PVH);
        }
    }
}
