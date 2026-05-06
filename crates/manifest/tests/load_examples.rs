//! Round-trip the example manifests through the schema. These are the
//! canonical "does the parser still match the published schema" checks.
//!
//! Both example manifests must:
//!   1. Load without error against the v0.0.1 schema
//!   2. Validate against both the Xen reference capabilities and the
//!      Hyper-V reference capabilities (zero issues on either)
//!
//! If either of these fails, either the schema or the example drifted —
//! both must be updated together.

use rotten_apple_manifest::{BackendCapabilities, Profile, ProfileKind, TpmMode};

const UBUNTU_DESKTOP_TOML: &str = "../../manifests/ubuntu-desktop.example.toml";
const ICLOUD_KEYCHAIN_TOML: &str = "../../manifests/icloud-keychain.example.toml";

#[test]
fn ubuntu_desktop_loads_and_validates_on_both_backends() {
    let p = Profile::load(UBUNTU_DESKTOP_TOML).expect("load ubuntu-desktop");
    assert_eq!(p.name(), "ubuntu-desktop");
    assert_eq!(*p.kind(), ProfileKind::Desktop);
    assert!(matches!(p.tpm.mode, TpmMode::Swtpm));
    assert_eq!(p.gpu.mode, "passthrough");

    let xen = p.validate_against(&BackendCapabilities::xen_reference());
    let hv  = p.validate_against(&BackendCapabilities::hyperv_reference());
    assert!(xen.is_empty(), "ubuntu-desktop on xen had issues: {xen:?}");
    assert!(hv.is_empty(),  "ubuntu-desktop on hyperv had issues: {hv:?}");
}

#[test]
fn icloud_keychain_loads_and_validates_on_both_backends() {
    let p = Profile::load(ICLOUD_KEYCHAIN_TOML).expect("load icloud-keychain");
    assert_eq!(p.name(), "icloud-keychain");
    assert_eq!(*p.kind(), ProfileKind::Appliance);
    assert_eq!(p.gpu.mode, "none");
    assert!(p.trigger.is_some(), "icloud-keychain must declare a trigger");
    assert_eq!(
        p.trigger.as_ref().unwrap().kind,
        "browser-native-messaging",
    );

    let xen = p.validate_against(&BackendCapabilities::xen_reference());
    let hv  = p.validate_against(&BackendCapabilities::hyperv_reference());
    assert!(xen.is_empty(), "icloud-keychain on xen had issues: {xen:?}");
    assert!(hv.is_empty(),  "icloud-keychain on hyperv had issues: {hv:?}");
}

#[test]
fn ubuntu_desktop_resource_sizes_decoded_to_bytes() {
    let p = Profile::load(UBUNTU_DESKTOP_TOML).unwrap();
    // memory_active = "56G"
    assert_eq!(p.resources.memory_active_bytes, 56u64 << 30);
    // memory_idle = "8G"
    assert_eq!(p.resources.memory_idle_bytes, 8u64 << 30);
    // memory_minimum = "2G"
    assert_eq!(p.resources.memory_minimum_bytes, 2u64 << 30);
}

#[test]
fn icloud_keychain_appliance_sizing_is_small() {
    let p = Profile::load(ICLOUD_KEYCHAIN_TOML).unwrap();
    assert_eq!(p.resources.memory_active_bytes,  1u64 << 30);          // 1G
    assert_eq!(p.resources.memory_idle_bytes,    256u64 << 20);        // 256M
    assert_eq!(p.resources.memory_minimum_bytes, 128u64 << 20);        // 128M
    assert_eq!(p.autostart.suspend_after_idle, Some(30));              // 30s
}
