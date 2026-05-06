//! End-to-end test for create / fork / destroy against a tmpdir-rooted
//! `InstancePaths`. The qemu-img call is stubbed via the
//! `ROTTEN_APPLE_QEMU_IMG` env var so CI doesn't need qemu installed.
//!
//! Note: setting an env var is process-wide. We serialize the tests in
//! this file behind a single mutex so parallel cargo-test runs don't
//! step on each other's stub binary path.

use std::path::PathBuf;
use std::sync::Mutex;

use rotten_apple_instances::{
    InstancePaths, NewInstanceParams,
    QEMU_IMG_OVERRIDE_ENV,
    create_instance_in, destroy_instance_in, fork_instance_in,
};

static SERIAL: Mutex<()> = Mutex::new(());

/// Build a stub script that just `touch`es the dest file qemu-img would
/// have written. The args we hand it are the qemu-img flags plus a
/// trailing destination path; the stub picks the last arg.
fn write_qemu_img_stub(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("qemu-img-stub.sh");
    std::fs::write(&path, r#"#!/bin/sh
# Last positional arg is the dest path qemu-img would create.
for arg in "$@"; do dest="$arg"; done
: > "$dest"
"#).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn make_paths(root: &std::path::Path) -> InstancePaths {
    InstancePaths {
        registry:      root.join("instances/index.toml"),
        instances_dir: root.join("instances"),
        images_index:  root.join("images/index.toml"),
        manifests_dir: root.join("manifests.d"),
    }
}

fn write_image_catalog(paths: &InstancePaths) {
    std::fs::create_dir_all(paths.images_index.parent().unwrap()).unwrap();
    let base_qcow2 = paths.images_index.parent().unwrap()
        .join("ubuntu-24.04.qcow2");
    std::fs::write(&base_qcow2, b"fake-qcow2").unwrap();
    let cat = format!(r#"
[[image]]
id = "ubuntu-24.04"
path = "{}"
"#, base_qcow2.display());
    std::fs::write(&paths.images_index, cat).unwrap();
}

fn tmpdir(label: &str) -> PathBuf {
    let t = std::env::temp_dir()
        .join(format!("rotten-apple-instances-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    t
}

#[test]
fn create_writes_overlay_manifest_and_registry() {
    let _g = SERIAL.lock().unwrap();
    let root = tmpdir("create");
    let paths = make_paths(&root);
    write_image_catalog(&paths);
    let stub = write_qemu_img_stub(&root);
    // SAFETY: env mutation is process-wide; SERIAL serializes the tests
    // in this file. Other test crates link separately, so this is fine.
    unsafe { std::env::set_var(QEMU_IMG_OVERRIDE_ENV, &stub); }

    let entry = create_instance_in(
        &paths,
        NewInstanceParams::new("dev", "ubuntu-24.04"),
        false,
    ).expect("create_instance");

    assert_eq!(entry.id, "dev");
    assert!(std::path::Path::new(&entry.overlay).exists(),
        "overlay should be created at {}", entry.overlay);
    assert!(paths.manifests_dir.join("dev.toml").exists(),
        "manifest should be written");
    assert!(paths.registry.exists(), "registry should be written");

    let reg = rotten_apple_instances::InstanceRegistry::load_or_empty(
        &paths.registry);
    assert_eq!(reg.instances.len(), 1);
    assert_eq!(reg.instances[0].id, "dev");
    unsafe { std::env::remove_var(QEMU_IMG_OVERRIDE_ENV); }
}

#[test]
fn create_dry_run_writes_nothing() {
    let _g = SERIAL.lock().unwrap();
    let root = tmpdir("dry-run");
    let paths = make_paths(&root);
    write_image_catalog(&paths);

    let entry = create_instance_in(
        &paths,
        NewInstanceParams::new("dev", "ubuntu-24.04"),
        true,
    ).expect("create_instance dry-run");

    assert_eq!(entry.id, "dev");
    assert!(!paths.registry.exists(), "registry must not be written");
    assert!(!paths.manifests_dir.join("dev.toml").exists(),
        "manifest must not be written");
}

#[test]
fn create_with_unknown_base_errors() {
    let _g = SERIAL.lock().unwrap();
    let root = tmpdir("unknown-base");
    let paths = make_paths(&root);
    write_image_catalog(&paths);

    let err = create_instance_in(
        &paths,
        NewInstanceParams::new("dev", "does-not-exist"),
        true,
    ).unwrap_err();
    assert!(matches!(err,
        rotten_apple_instances::InstanceError::BaseImageNotFound(_)));
}

#[test]
fn fork_carries_parent_overlay_as_backing() {
    let _g = SERIAL.lock().unwrap();
    let root = tmpdir("fork");
    let paths = make_paths(&root);
    write_image_catalog(&paths);
    let stub = write_qemu_img_stub(&root);
    unsafe { std::env::set_var(QEMU_IMG_OVERRIDE_ENV, &stub); }

    let parent = create_instance_in(
        &paths,
        NewInstanceParams::new("base", "ubuntu-24.04"),
        false,
    ).unwrap();

    let fork = fork_instance_in(&paths, "base", "child", false).unwrap();
    assert_eq!(fork.parent.as_deref(), Some("base"));
    assert_eq!(fork.base_image, parent.overlay);
    assert!(std::path::Path::new(&fork.overlay).exists());
    unsafe { std::env::remove_var(QEMU_IMG_OVERRIDE_ENV); }
}

#[test]
fn destroy_drops_registry_entry() {
    let _g = SERIAL.lock().unwrap();
    let root = tmpdir("destroy");
    let paths = make_paths(&root);
    write_image_catalog(&paths);
    let stub = write_qemu_img_stub(&root);
    unsafe { std::env::set_var(QEMU_IMG_OVERRIDE_ENV, &stub); }

    create_instance_in(
        &paths,
        NewInstanceParams::new("dev", "ubuntu-24.04"),
        false,
    ).unwrap();
    destroy_instance_in(&paths, "dev", false).unwrap();

    let reg = rotten_apple_instances::InstanceRegistry::load_or_empty(
        &paths.registry);
    assert!(reg.find("dev").is_none());
    assert!(!paths.manifests_dir.join("dev.toml").exists());
    unsafe { std::env::remove_var(QEMU_IMG_OVERRIDE_ENV); }
}
