//! End-to-end pull test. Hits the real internet and pulls a multi-MB
//! cloud image. Off by default — operators run with:
//!
//!     cargo test -p rotten-apple-images -- --ignored
//!
//! Cleanup is on the tempdir drop; nothing lands in /var/lib.

use rotten_apple_images::pull;

#[test]
#[ignore = "requires network + bandwidth"]
fn real_pull_ubuntu_24_04() {
    let dir = tempfile::tempdir().unwrap();
    let entry = pull("ubuntu:24.04", dir.path(), false)
        .expect("pull should succeed against a live mirror");
    assert!(entry.size_bytes > 100_000_000,
        "expected a multi-hundred-MB image, got {} bytes", entry.size_bytes);
    assert!(entry.backing.exists());
}

#[test]
fn dry_run_does_not_hit_network() {
    let dir = tempfile::tempdir().unwrap();
    let entry = pull("ubuntu:24.04", dir.path(), true).expect("dry-run is offline-safe");
    assert_eq!(entry.size_bytes, 0);
    assert_eq!(entry.distro, "ubuntu");
}

#[test]
fn unknown_source_errors() {
    let dir = tempfile::tempdir().unwrap();
    let err = pull("not-a-distro", dir.path(), true).unwrap_err();
    assert!(format!("{err}").contains("unknown source"));
}
