//! User-desktop guest manifest generator.
//!
//! When ThinDom0 boots, its cockpit auto-starts a guest from the
//! manifest at `/etc/rotten-apple/user-desktop.toml`. That guest IS
//! the user's daily-driver Ubuntu desktop, now living as a domU rather
//! than as dom0 itself.
//!
//! The manifest mirrors `manifests/this-machine-ubuntu-domu.toml`
//! (which the v0.0.1 lift was the conceptual ancestor of) but with
//! values derived from the host at install time, not hand-edited.
//!
//! Disk passthrough strategy:
//!   `kind = "block"` with source = the parent physical disk
//!   (`plan.user_root_disk`, e.g. `/dev/nvme1n1`). The guest sees the
//!   whole disk; pygrub reads /boot from its own partition; the
//!   guest's existing kernel + initramfs + LUKS + GRUB config all
//!   work unmodified. Same shape as `manifests/this-machine-ubuntu-domu.toml`
//!   from the v0.0.1 lift.
//!
//! This means dom0 cannot mount any partition on that disk at runtime
//! (concurrent RW would corrupt the guest's filesystems). dom0 keeps
//! ephemeral state in tmpfs and reconstructs structural state at boot
//! from /sys/kernel/iommu_groups + xenstore. Persistent dom0 state is
//! a Phase 6 concern (likely a small dedicated rotten-apple partition
//! carved at install time, or PV-USB to a host-side state file).

use std::path::Path;

use rotten_apple_manifest::TpmMode;

use crate::thin_dom0::ThinDom0Plan;

/// User-desktop guest sizing — hard-capped, not host-dependent.
///
/// 4 vcpus = 2 P-threads + 2 E-threads (a hyperthreaded P-core plus
/// 2 E-cores). 4 GB per thread = 16 GB total. iGPU passthrough as the
/// default visual experience. Leaves the rest of the host (more
/// P/E-cores, dGPU, remaining RAM) free for other compute guests
/// (SteamOS, LLM containers, etc.).
///
/// User can edit /etc/rotten-apple/user-desktop.toml to override
/// after install, but defaults are tuned for laptop battery life
/// and "the daily driver shouldn't eat all the resources."
pub const THINDOM0_USER_DESKTOP_VCPUS: u32 = 4;
pub const THINDOM0_USER_DESKTOP_MEM_MB: u64 = 16 * 1024;
pub const USER_DESKTOP_MANIFEST_FILENAME: &str = "user-desktop.toml";
pub const USER_DESKTOP_VISIBLE_MANIFEST_FILENAME: &str = "user-desktop-visible.toml";
pub const USER_DESKTOP_MANIFEST_PATH: &str = "/etc/rotten-apple/user-desktop.toml";
pub const USER_DESKTOP_VISIBLE_MANIFEST_PATH: &str =
    "/etc/rotten-apple/user-desktop-visible.toml";

/// Inputs needed beyond the plan itself — chiefly which GPU (if any)
/// to lease to the user-desktop guest. On a laptop with an iGPU + dGPU,
/// the iGPU is the sacred framebuffer and the right default for the
/// daily desktop. Pass `None` to fall back to PV framebuffer (no 3D,
/// laggy — for bring-up only).
#[derive(Debug, Clone)]
pub struct UserDesktopInputs {
    /// PCI BDF of the GPU to pass through, e.g. "0000:00:02.0" for
    /// the Intel iGPU. None = generate a PV framebuffer entry instead.
    pub gpu_bdf: Option<String>,
    /// Whether this manifest should prefer native framebuffer ownership
    /// (daily-driver path) or force a visible paravirtual debug surface
    /// (passphrase / LUKS bring-up path).
    pub display_mode: UserDesktopDisplayMode,
    /// TPM exposure policy for the guest.
    pub tpm_mode: TpmMode,
    /// Whether cockpit should autostart this manifest after boot.
    pub autostart_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDesktopDisplayMode {
    PassthroughIfAvailable,
    ParavirtOnly,
}

/// Produce the manifest TOML string. Pure: takes the plan + inputs
/// and renders text. No I/O.
pub fn render_user_desktop_manifest(plan: &ThinDom0Plan, inputs: &UserDesktopInputs) -> String {
    // Sizing is hard-capped, not host-derived. See module-level
    // constants for the reasoning. The user-desktop is the *primary*
    // workload but not the *only* one — leaving headroom on a
    // 64 GB / 20-thread host means SteamOS, LLM containers, etc. can
    // co-exist without the daily driver hogging everything.
    let mem_active   = THINDOM0_USER_DESKTOP_MEM_MB;
    let mem_idle     = mem_active / 2;
    let mem_min      = mem_active / 4;
    let vcpus_active = THINDOM0_USER_DESKTOP_VCPUS;

    let storage_source = plan.user_root_disk.display();
    let display_gpu = match inputs.display_mode {
        UserDesktopDisplayMode::PassthroughIfAvailable => inputs.gpu_bdf.as_deref(),
        UserDesktopDisplayMode::ParavirtOnly => None,
    };
    let (gpu, audio) = render_gpu_and_audio(display_gpu);
    let description = match inputs.display_mode {
        UserDesktopDisplayMode::PassthroughIfAvailable =>
            "User's existing Ubuntu install, lifted onto ThinDom0 as the primary domU.",
        UserDesktopDisplayMode::ParavirtOnly =>
            "Visible-debug view of the user's Ubuntu install for ThinDom0 passphrase bring-up.",
    };
    let tpm = render_tpm_section(&inputs.tpm_mode);
    let autostart = render_autostart_section(inputs.autostart_enabled);

    format!(
r#"# Generated by rotten-apple bootstrapper for ThinDom0 first-boot.
# This is the user-desktop guest manifest — the user's existing Ubuntu
# install, presented as a domU on top of ThinDom0.
#
# Whole-disk passthrough: the guest sees the entire underlying disk
# via xen-blkback. pygrub reads /boot from its own partition; the
# guest's kernel + initramfs + LUKS + GRUB all work unmodified.
# dom0 must NOT mount any partition on this disk at runtime — that
# would race the guest's mounts and corrupt the filesystem.

[profile]
name = "user-desktop"
schema_version = "1"
description = "{description}"
type = "desktop"

[meta]
license = "personal"
attestation_required = false

# Sizing pulled from rotten_apple_detect::plan() — same numbers
# `rotten-apple plan-lift` prints. dom0 reserve already excluded.
[resources]
memory_active   = "{mem_active}M"
memory_idle     = "{mem_idle}M"
memory_minimum  = "{mem_min}M"
vcpus_active    = {vcpus_active}
vcpus_idle      = {vcpus_active}
vcpus_minimum   = 1
prefer_p_cores  = true
idle_on_e_cores = false

# Whole-disk passthrough — guest's pygrub finds /boot, LUKS unlocks
# inside the guest, the existing Ubuntu kernel boots unchanged.
[storage]
root = {{ kind = "block", source = "{storage_source}", mode = "rw-exclusive" }}

[network]
mode = "bridge"

[[network.interfaces]]
name   = "primary"
mac    = "auto"
egress = "any"

{gpu}
[input]
keyboard = "follow_focus"
mouse    = "follow_focus"

{audio}
[usb]
mode          = "policy"
default_route = "follow_focus"

{tpm}

{autostart}

[orchestration]
priority = "primary"
"#)
}

/// Audio always rides with the GPU mode — passthrough HDA when the
/// GPU is being passed (HDA typically shares the iGPU's IOMMU group),
/// PV audio when the GPU is PV. Returning both together prevents the
/// two sections from drifting (e.g. PV gpu + passthrough audio, which
/// the manifest validator wouldn't catch but the guest would fail at).
fn render_gpu_and_audio(bdf: Option<&str>) -> (String, String) {
    match bdf {
        Some(bdf) => (
            format!(
"# iGPU passthrough — drives the laptop framebuffer directly. dom0 stays
# headless (cockpit on tty1 via the host's serial-over-VGA console).
[gpu]
mode   = \"passthrough\"
device = \"{bdf}\"
"
            ),
            String::from(
"# Audio rides with the GPU on Intel iGPU passthrough (HDA controller
# typically shares the IOMMU group with 0000:00:02.0).
[audio]
mode = \"passthrough\"
"
            ),
        ),
        None => (
            String::from(
"# No GPU lease yet — Looking Glass + dGPU passthrough is Phase 5.
# Until then the user-desktop guest gets a PV framebuffer (laggy, no 3D).
[gpu]
mode = \"paravirt\"
"
            ),
            String::from(
"# Audio follows the GPU mode — PV audio while the guest runs on a PV
# framebuffer. Passthrough HDA only works when the GPU it shares an
# IOMMU group with is also passed through.
[audio]
mode = \"paravirt\"
"
            ),
        ),
    }
}

fn render_tpm_section(mode: &TpmMode) -> String {
    match mode {
        TpmMode::None => String::from(
r#"# Ubuntu doesn't require a TPM at boot; LUKS unlocks via passphrase
# inside the guest. PVH-friendly.
[tpm]
mode = "none"
"#),
        TpmMode::Swtpm => String::from(
r#"# Software TPM exposure for guests that want a TPM-visible boot path.
# Existing host-side LUKS TPM enrollment does NOT automatically carry
# over to a guest; re-enroll inside the guest if you want auto-unlock.
[tpm]
mode = "swtpm"
"#),
        TpmMode::HardwarePassthrough => String::from(
r#"# Hardware TPM passthrough is a high-trust / special-purpose path.
[tpm]
mode = "hardware-passthrough"
"#),
    }
}

fn render_autostart_section(enabled: bool) -> String {
    if enabled {
        String::from(
r#"[autostart]
enabled            = true
delay_after_boot   = "5s"
"#)
    } else {
        String::from(
r#"[autostart]
enabled = false
"#)
    }
}

/// Convenience: write the manifest to disk. Used by install-thindom0
/// (not by plan-thindom0 which only renders to stdout).
#[allow(dead_code)]
pub fn install_user_desktop_manifest(plan: &ThinDom0Plan, inputs: &UserDesktopInputs)
    -> std::io::Result<std::path::PathBuf>
{
    let dst = Path::new(USER_DESKTOP_MANIFEST_PATH);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = render_user_desktop_manifest(plan, inputs);
    std::fs::write(dst, body)?;
    Ok(dst.to_path_buf())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_plan() -> ThinDom0Plan {
        ThinDom0Plan {
            kernel_path: PathBuf::from("/boot/rotten-apple/vmlinuz"),
            initrd_path: PathBuf::from("/boot/rotten-apple/thin-dom0.cpio.gz"),
            kernel_source: PathBuf::from("/boot/vmlinuz-6.17.0-23-generic"),
            dom0_mem_mb: crate::thin_dom0::THINDOM0_DOM0_MEM_MB,
            dom0_mem_max_mb: crate::thin_dom0::THINDOM0_DOM0_MEM_MAX_MB,
            dom0_vcpus: 1,
            dom0_pinned_cpu: Some(12),
            user_root_device: PathBuf::from("/dev/mapper/ubuntu--vg-ubuntu--lv"),
            user_root_uuid: "9881a7d1-66a6-463d-9433-a7117badbc35".to_string(),
            user_root_fstype: "ext4".to_string(),
            user_root_disk: PathBuf::from("/dev/nvme1n1"),
            persistent_state_dir: PathBuf::from("/var/lib/rotten-apple"),
            grub_entry_name: "rotten-apple ThinDom0".to_string(),
            has_separate_boot: true,
            boot_fs_uuid: "2048f8be-403d-4cb7-b79a-c821a0daa609".to_string(),
            xen_image_basename: PathBuf::from("xen-4.20-amd64.gz"),
            framebuffer_gpu_bdf: Some("0000:00:02.0".to_string()),
            esp_device: Some(PathBuf::from("/dev/nvme0n1p1")),
            esp_fs_uuid: Some("B6D5-2FF2".to_string()),
            dry_run: true,
        }
    }

    #[test]
    fn manifest_parses_via_profile_load() {
        // Pin the cross-crate contract: the rendered TOML must validate
        // through the manifest crate's Profile::from_str. If we add a
        // required field to Profile and forget to render it here, this
        // test fails immediately rather than the install hitting it.
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: Some("0000:00:02.0".into()),
            display_mode: UserDesktopDisplayMode::PassthroughIfAvailable,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        let parsed = rotten_apple_manifest::Profile::from_str(&m)
            .expect("rendered manifest must parse via Profile::from_str");
        assert_eq!(parsed.name(), "user-desktop");
        assert_eq!(*parsed.kind(), rotten_apple_manifest::ProfileKind::Desktop);
    }

    #[test]
    fn manifest_uses_parent_disk_as_storage_source() {
        // Whole-disk passthrough: the guest receives the parent block
        // device, not the LV. pygrub reads /boot from the boot partition
        // on that disk — passing only the LV would leave pygrub blind.
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: None,
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        assert!(m.contains("source = \"/dev/nvme1n1\""),
                "storage source must point at the parent disk, not the LV: {m}");
        assert!(!m.contains("source = \"/dev/mapper/"),
                "manifest must NOT pass the LV directly (pygrub can't \
                 read /boot from inside the LV alone): {m}");
        assert!(m.contains("kind = \"block\""),
                "storage kind should be block for whole-device passthrough");
    }

    #[test]
    fn manifest_with_gpu_bdf_emits_passthrough() {
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: Some("0000:00:02.0".into()),
            display_mode: UserDesktopDisplayMode::PassthroughIfAvailable,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        assert!(m.contains("mode   = \"passthrough\""),
                "gpu mode must be passthrough when BDF supplied: {m}");
        assert!(m.contains("device = \"0000:00:02.0\""),
                "gpu device must be the BDF supplied: {m}");
    }

    #[test]
    fn manifest_without_gpu_bdf_emits_paravirt() {
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: None,
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        assert!(m.contains("mode = \"paravirt\""),
                "gpu mode must be paravirt when no BDF: {m}");
        assert!(!m.contains("mode   = \"passthrough\""),
                "passthrough section must be absent when no BDF: {m}");
    }

    #[test]
    fn audio_follows_gpu_mode() {
        // Pin the contract that audio and gpu mode never drift apart.
        // Earlier rev had audio hardcoded to passthrough, which produced
        // an incoherent manifest when gpu was paravirt — guest would
        // try to claim the HDA controller dom0 still owned.
        let plan = sample_plan();

        let pv = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: None,
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        let pv_audio = pv.split("[audio]").nth(1).expect("audio section");
        assert!(pv_audio.contains("mode = \"paravirt\""),
                "audio must be paravirt when GPU is paravirt: {pv}");

        let pt = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: Some("0000:00:02.0".into()),
            display_mode: UserDesktopDisplayMode::PassthroughIfAvailable,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        let pt_audio = pt.split("[audio]").nth(1).expect("audio section");
        assert!(pt_audio.contains("mode = \"passthrough\""),
                "audio must be passthrough when GPU is passthrough: {pt}");
    }

    #[test]
    fn manifest_warns_dom0_must_not_mount_user_disk() {
        // The whole-disk passthrough model only works if dom0 keeps its
        // hands off the disk after handing it to the guest. Pin that
        // the manifest carries this warning so a future contributor
        // adding a "convenience" mount in the bootstrapper sees it.
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: None,
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        assert!(m.contains("dom0 must NOT mount any partition on this disk"),
                "manifest must warn against concurrent mounts: {m}");
    }

    #[test]
    fn manifest_resources_match_hard_caps() {
        // The user-desktop guest is hard-capped, not host-dependent.
        // Pin the literals so a future change to the planner doesn't
        // silently re-grow the daily driver to "all of host RAM."
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: None,
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: TpmMode::None,
            autostart_enabled: true,
        });
        let parsed = rotten_apple_manifest::Profile::from_str(&m).unwrap();

        let active_mb = parsed.resources.memory_active_bytes / (1024 * 1024);
        assert_eq!(active_mb, THINDOM0_USER_DESKTOP_MEM_MB,
                   "memory_active must equal the hard-cap constant");
        assert_eq!(parsed.resources.vcpus_active, THINDOM0_USER_DESKTOP_VCPUS,
                   "vcpus_active must equal the hard-cap constant");
        // Memory floor: idle = half active, minimum = quarter active.
        let idle_mb = parsed.resources.memory_idle_bytes / (1024 * 1024);
        let min_mb  = parsed.resources.memory_minimum_bytes / (1024 * 1024);
        assert_eq!(idle_mb, THINDOM0_USER_DESKTOP_MEM_MB / 2);
        assert_eq!(min_mb,  THINDOM0_USER_DESKTOP_MEM_MB / 4);
    }

    #[test]
    fn visible_manifest_disables_autostart_and_forces_paravirt_gpu() {
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: Some("0000:00:02.0".into()),
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: TpmMode::None,
            autostart_enabled: false,
        });
        assert!(m.contains("enabled = false"),
                "visible-debug manifest must not autostart: {m}");
        assert!(m.contains("mode = \"paravirt\""),
                "visible-debug manifest must force paravirt display: {m}");
        assert!(!m.contains("mode   = \"passthrough\""),
                "visible-debug manifest must not lease the framebuffer GPU: {m}");
    }

    #[test]
    fn manifest_can_request_swtpm() {
        let plan = sample_plan();
        let m = render_user_desktop_manifest(&plan, &UserDesktopInputs {
            gpu_bdf: None,
            display_mode: UserDesktopDisplayMode::ParavirtOnly,
            tpm_mode: TpmMode::Swtpm,
            autostart_enabled: false,
        });
        assert!(m.contains("mode = \"swtpm\""),
                "TPM-enabled manifest must render swtpm mode: {m}");
    }
}
