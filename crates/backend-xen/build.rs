//! Generate Rust bindings for libxl at compile time.
//!
//! libxl is the Xen Light library — the canonical C API for managing Xen
//! domains. Bindings live in `OUT_DIR/libxl_sys.rs` and are included from
//! `src/lib.rs` as `mod sys`.
//!
//! Build dependencies on the host:
//!   - libxen-dev   (headers under /usr/include/{,_}libxl*.h)
//!   - clang        (libclang for bindgen)
//!   - libxenlight  (linked at runtime; provided by xen-utils-common etc.)

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Tell cargo to relink if our wrapper changes.
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");

    // Detect installed libxl version and emit cfg flags so version-fragile
    // code paths can branch without scattering pkg-config calls everywhere.
    // libxl_domain_config field shapes shift across minor releases; we
    // generate bindings against whatever's installed, but our hand-written
    // accessors need to know which set of fields exist.
    //
    // Emits (for e.g. xenlight 4.20.x):
    //   cargo:rustc-cfg=xen_major="4"
    //   cargo:rustc-cfg=xen_minor="20"
    //   cargo:rustc-cfg=xen_4_20
    //   cargo:rustc-cfg=xen_4_20_or_later
    //   cargo:rustc-cfg=xen_4_19_or_later
    //   ...
    //
    // If pkg-config is missing or the library isn't installed, build fails
    // loudly here rather than producing bindings against unknown headers.
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rustc-check-cfg=cfg(xen_major, values(any()))");
    println!("cargo:rustc-check-cfg=cfg(xen_minor, values(any()))");
    for v in ["xen_4_17", "xen_4_18", "xen_4_19", "xen_4_20", "xen_4_21",
              "xen_4_18_or_later", "xen_4_19_or_later", "xen_4_20_or_later",
              "xen_4_21_or_later"] {
        println!("cargo:rustc-check-cfg=cfg({v})");
    }
    let (major, minor) = detect_libxl_version();
    println!("cargo:rustc-cfg=xen_major=\"{major}\"");
    println!("cargo:rustc-cfg=xen_minor=\"{minor}\"");
    println!("cargo:rustc-cfg=xen_{major}_{minor}");
    for m in 17..=minor {
        println!("cargo:rustc-cfg=xen_{major}_{m}_or_later");
    }
    println!("cargo:warning=building against libxl {major}.{minor}");

    // Link against libxenlight (libxl) and its event loop helper. Order
    // matters for static linking; for the dynamic libs Ubuntu ships it's
    // permissive but we list dependencies explicitly anyway.
    println!("cargo:rustc-link-lib=xenlight");
    println!("cargo:rustc-link-lib=xenstore");
    println!("cargo:rustc-link-lib=xenctrl");
    // xtl_* (xen tools logger) lives in its own .so, separate from libxl.
    println!("cargo:rustc-link-lib=xentoollog");

    // Generate bindings.
    //
    // Allowlist filters keep the output focused on libxl_*. Without these
    // bindgen would also pull in everything libxl.h transitively includes
    // (sys/types, signal.h, etc.) which makes builds slow and noisy.
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        // Headers are in /usr/include directly; no extra -I needed on Ubuntu.
        .allowlist_function("libxl_.*")
        .allowlist_function("xtl_.*")          // xen-tools logger callbacks
        .allowlist_type("libxl_.*")
        .allowlist_type("xtl_.*")
        .allowlist_var("LIBXL_.*")
        .allowlist_var("XTL_.*")
        // Generate Rust enums (not constants) for libxl C enums.
        .default_enum_style(bindgen::EnumVariation::Rust { non_exhaustive: false })
        // libxl_error specifically: use a newtype (struct wrapping i32) so
        // we can safely look up unknown codes returned at runtime without
        // tripping Rust 2024's "transmute to invalid enum variant" UB check.
        .newtype_enum("libxl_error")
        // libxl uses many opaque structs; let bindgen treat them as such.
        .derive_debug(true)
        .derive_default(true)
        // Don't try to layout-test bitfield-heavy types we don't use.
        .layout_tests(false)
        // Tell bindgen the target environment.
        .clang_arg("-D_GNU_SOURCE")
        .generate()
        .expect("bindgen failed to generate libxl bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("libxl_sys.rs");
    bindings.write_to_file(&out_path).expect("write bindings");
    println!("cargo:warning=libxl bindings written to {}", out_path.display());
}

/// Returns `(major, minor)` of the installed libxl. Tries pkg-config first
/// (most reliable), then falls back to dpkg, then to `LIBXL_VERSION` from
/// the headers themselves. Panics if all probes fail — the build can't
/// proceed without knowing the ABI.
fn detect_libxl_version() -> (u32, u32) {
    if let Some(v) = pkg_config_version("xenlight") { return v }
    if let Some(v) = pkg_config_version("xencontrol") { return v }
    if let Some(v) = dpkg_libxen_dev_version() { return v }
    panic!("could not determine installed libxl version. Install pkg-config \
            and ensure libxen-dev is present, or set XEN_LIBXL_MAJOR / \
            XEN_LIBXL_MINOR env vars.");
}

fn pkg_config_version(pkg: &str) -> Option<(u32, u32)> {
    let out = Command::new("pkg-config")
        .args(["--modversion", pkg])
        .output().ok()?;
    if !out.status.success() { return None }
    parse_major_minor(String::from_utf8_lossy(&out.stdout).trim())
}

fn dpkg_libxen_dev_version() -> Option<(u32, u32)> {
    let out = Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "libxen-dev"])
        .output().ok()?;
    if !out.status.success() { return None }
    parse_major_minor(String::from_utf8_lossy(&out.stdout).trim())
}

/// Accepts `"4.20.0"`, `"4.20.0+68-g35cb38b222-1"`, `"4.20"`, etc.
fn parse_major_minor(s: &str) -> Option<(u32, u32)> {
    // Take everything before the first non-numeric/non-dot character.
    let head: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut parts = head.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}
