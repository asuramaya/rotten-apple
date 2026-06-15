#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
simulate_thindom0_qemu.sh - boot the currently staged ThinDom0 assets in QEMU.

This is a local bring-up harness for the real host-side boot assets:
  /boot/xen-*.gz
  /boot/rotten-apple/vmlinuz
  /boot/rotten-apple/thin-dom0.cpio.gz

It does NOT prove laptop-specific hardware behavior. It is useful for
faster iteration on the early path:
  Xen -> dom0 kernel -> /init

BOOT METHOD (v2): direct multiboot, NO bootloader/firmware.
  Earlier revs built a GRUB rescue ISO and booted it under OVMF/UEFI. That
  stacked four fragile layers (OVMF -> grub-mkrescue -> GRUB -> multiboot2),
  none of which are the product. We now hand QEMU the hypervisor + modules
  directly, exactly like the Xen project's own QEMU CI:
      qemu -kernel <xen>  -initrd "<vmlinuz> <dom0 cmdline>,<cpio>"  -append "<xen cmdline>"
  xen-*.gz is gzip-compressed; QEMU's -kernel multiboot loader reads the
  header from the file start, so we decompress it into the workdir first.

The harness attaches a small ext4 scratch disk. /init mounts it (via the
/rotten-apple/vmlinuz marker file) and writes persistent logs under
/rotten-apple/log/, which this script extracts after QEMU exits.

Run as root so the staged /boot assets are readable:
  sudo scripts/simulate_thindom0_qemu.sh

Options:
  --mode normal|recovery|serial-debug
      normal       real normal ThinDom0 cmdline (autostart enabled, console=vga
                   -> BLIND over serial, faithful to the laptop)
      recovery     real recovery cmdline (no autostart, console=vga -> BLIND)
      serial-debug DEFAULT. Xen COM1 + dom0 hvc0 console -> readable on QEMU
                   serial. This is the dev-iteration mode.
  --break STAGE
      add rotten_apple.break=STAGE to the dom0 kernel cmdline
      useful stages: before-xen
  --dom0-pin N
      add rotten_apple.dom0_pinned_cpu=N to the dom0 cmdline — reproduces the
      laptop-only `xl vcpu-pin` branch in /init (the confirmed boot wedge)
  --init-overlay FILE
      replace /init in the staged cpio with FILE (via a concatenated overlay
      cpio) without rebuilding — to test an /init fix against the real image
  --dom0-mem MB
      override Xen dom0_mem for the simulated entry
      default: read /etc/grub.d/41_rotten_apple_thindom0, fallback 2048
  --accel auto|kvm|tcg
      auto prefers KVM when /dev/kvm is usable, else falls back to TCG
  --timeout SEC
      kill QEMU after SEC seconds (default: 45)
  --memory MB
      guest RAM in MB (default: 4096)
  --smp N
      vCPU count (default: 2)
  --keep
      keep the temporary workdir instead of deleting it on success
  --workdir DIR
      reuse a caller-chosen workdir instead of mktemp
  -h, --help
      show this help
EOF
}

mode="serial-debug"
accel="auto"
# 45s was the OVMF-era default; a nested Xen PV dom0 boot to /init needs far
# longer even with low console verbosity. 180s comfortably reaches the cockpit
# handoff. On a successful boot the cockpit respawn loop keeps dom0 up, so QEMU
# runs to this timeout regardless — keep it tight enough for fast iteration.
timeout_sec=180
memory_mb=4096
smp=2
keep=0
workdir=""
break_stage=""
dom0_mem_mb=""
dom0_pin=""
init_overlay=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --mode)
            mode="${2:?missing value for --mode}"
            shift 2
            ;;
        --accel)
            accel="${2:?missing value for --accel}"
            shift 2
            ;;
        --break)
            break_stage="${2:?missing value for --break}"
            shift 2
            ;;
        --dom0-pin)
            dom0_pin="${2:?missing value for --dom0-pin}"
            shift 2
            ;;
        --init-overlay)
            init_overlay="${2:?missing value for --init-overlay}"
            shift 2
            ;;
        --dom0-mem)
            dom0_mem_mb="${2:?missing value for --dom0-mem}"
            shift 2
            ;;
        --timeout)
            timeout_sec="${2:?missing value for --timeout}"
            shift 2
            ;;
        --memory)
            memory_mb="${2:?missing value for --memory}"
            shift 2
            ;;
        --smp)
            smp="${2:?missing value for --smp}"
            shift 2
            ;;
        --keep)
            keep=1
            shift
            ;;
        --workdir)
            workdir="${2:?missing value for --workdir}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ $EUID -ne 0 ]]; then
    echo "run this script as root (sudo) so /boot/rotten-apple assets are readable" >&2
    exit 2
fi

case "$mode" in
    normal|recovery|serial-debug) ;;
    *)
        echo "invalid --mode: $mode" >&2
        exit 2
        ;;
esac

case "$accel" in
    auto|kvm|tcg) ;;
    *)
        echo "invalid --accel: $accel" >&2
        exit 2
        ;;
esac

require_tool() {
    local name="$1"
    command -v "$name" >/dev/null 2>&1 || {
        echo "required tool missing from PATH: $name" >&2
        exit 2
    }
}

require_tool qemu-system-x86_64
require_tool qemu-img
require_tool guestfish
require_tool timeout
require_tool gzip

if [[ -z "$workdir" ]]; then
    workdir="$(mktemp -d /tmp/ra-thindom-sim.XXXXXX)"
else
    mkdir -p "$workdir"
fi

cleanup() {
    if [[ $keep -eq 0 ]]; then
        rm -rf "$workdir"
    fi
}
trap cleanup EXIT

select_xen_image() {
    find /boot -maxdepth 1 -name 'xen-*.gz' -printf '%f\n' | sort | tail -n 1
}

xen_basename="$(select_xen_image)"
if [[ -z "$xen_basename" ]]; then
    echo "no /boot/xen-*.gz image found" >&2
    exit 2
fi

if [[ ! -r /boot/rotten-apple/vmlinuz || ! -r /boot/rotten-apple/thin-dom0.cpio.gz ]]; then
    echo "staged ThinDom0 assets missing under /boot/rotten-apple/" >&2
    exit 2
fi

detect_dom0_mem_mb() {
    local detected=""
    if [[ -n "$dom0_mem_mb" ]]; then
        printf '%s\n' "$dom0_mem_mb"
        return
    fi
    if [[ -r /etc/grub.d/41_rotten_apple_thindom0 ]]; then
        detected="$(grep -o 'dom0_mem=[0-9]\+M' /etc/grub.d/41_rotten_apple_thindom0 | head -n 1 | sed 's/^dom0_mem=//; s/M$//')"
    fi
    if [[ -n "$detected" ]]; then
        printf '%s\n' "$detected"
    else
        printf '2048\n'
    fi
}

sim_dom0_mem_mb="$(detect_dom0_mem_mb)"
if ! [[ "$sim_dom0_mem_mb" =~ ^[0-9]+$ ]]; then
    echo "invalid dom0 memory value: $sim_dom0_mem_mb" >&2
    exit 2
fi
if (( memory_mb <= sim_dom0_mem_mb )); then
    echo "--memory (${memory_mb}) must exceed dom0_mem (${sim_dom0_mem_mb})" >&2
    exit 2
fi

# Stage the boot inputs flat in the workdir. xen-*.gz must be decompressed:
# QEMU's -kernel multiboot loader looks for the multiboot header at the start
# of the file, which a gzip wrapper hides. zcat -> a plain 32-bit multiboot
# ELF that QEMU loads directly (verified against Xen 4.20 + QEMU 10.1).
gzip -dc "/boot/$xen_basename" > "$workdir/xen"
cp /boot/rotten-apple/vmlinuz          "$workdir/vmlinuz"
cp /boot/rotten-apple/thin-dom0.cpio.gz "$workdir/thin-dom0.cpio.gz"

# --init-overlay FILE: append a tiny cpio.gz carrying just /init (= FILE) AFTER
# the staged rootfs. The kernel's initramfs unpacker processes concatenated
# cpio archives in order and later entries OVERWRITE earlier ones, so this swaps
# /init in place WITHOUT decompressing/repacking the ~250 MB rootfs. Lets us
# test a fixed /init against the real staged image in seconds. This is exactly
# the override mechanism we use to stage the fix onto /boot, so a successful sim
# here also validates the delivery path.
if [[ -n "$init_overlay" ]]; then
    if [[ ! -r "$init_overlay" ]]; then
        echo "--init-overlay: cannot read $init_overlay" >&2
        exit 2
    fi
    ov="$workdir/init-overlay-build"
    mkdir -p "$ov"
    cp "$init_overlay" "$ov/init"
    chmod 0755 "$ov/init"
    ( cd "$ov" && echo init | cpio --quiet -o -H newc | gzip ) > "$workdir/init-overlay.cpio.gz"
    cat "$workdir/init-overlay.cpio.gz" >> "$workdir/thin-dom0.cpio.gz"
    echo "init-overlay: /init replaced via concatenated cpio ($init_overlay)"
fi

# Build the Xen hypervisor cmdline (-append) and the dom0 kernel cmdline
# (the first multiboot module's argument string). These mirror the real GRUB
# entries rendered by thin_dom0_grub.rs.
#
# CRITICAL: the dom0 cmdline goes inside -initrd, where COMMAS SEPARATE
# MODULES. It must not contain a comma. (com1=115200,8n1 / console=com1,vga
# live on the Xen -append line, which is a single string and may contain
# commas.)
break_arg=""
if [[ -n "$break_stage" ]]; then
    break_arg=" rotten_apple.break=$break_stage"
fi

xen_cmdline=""
dom0_cmdline=""
case "$mode" in
    normal)
        xen_cmdline="dom0_mem=${sim_dom0_mem_mb}M,max:${sim_dom0_mem_mb}M dom0_max_vcpus=1 iommu=verbose,no-igfx console=vga loglvl=all guest_loglvl=all"
        dom0_cmdline="console=tty0 loglevel=7 iommu=pt intel_iommu=on amd_iommu=on rotten_apple.user_root_uuid=sim-root${break_arg}"
        ;;
    recovery)
        xen_cmdline="dom0_mem=${sim_dom0_mem_mb}M,max:${sim_dom0_mem_mb}M dom0_max_vcpus=1 iommu=verbose,no-igfx console=vga sync_console=true loglvl=all guest_loglvl=all"
        dom0_cmdline="console=tty0 loglevel=7 iommu=pt intel_iommu=on amd_iommu=on rotten_apple.user_root_uuid=sim-root rotten_apple.no_autostart=1${break_arg}"
        ;;
    serial-debug)
        # NOTE: IOMMU flags (iommu=pt intel_iommu=on amd_iommu=on) are
        # deliberately omitted here. They exist on the real laptop cmdline only
        # for GPU passthrough; this QEMU machine has no IOMMU (Xen logs
        # "I/O virtualisation disabled"), and leaving intel_iommu=on hangs the
        # PV dom0 kernel during PCI enumeration (reproduced 2026-05-21). Add
        # `-device intel-iommu` + restore the flags only if testing passthrough.
        # sync_console deliberately OFF: under nested KVM it forces a vmexit
        # per serial char, and combined with the verbose boot it turns PCI
        # enumeration into a wall-clock crawl (guest clock barely advances
        # while minutes of real time pass — a vmexit storm). Async console is
        # plenty for the sim; /init's persistent log on the scratch disk is the
        # durable forensic channel anyway.
        # Verbosity kept low on purpose: console output is the dominant cost
        # under nested virt (each char is a vmexit). loglevel=4 still shows
        # kernel errors (level <=3) and does NOT gate /init's userspace writes
        # to /dev/console. Dropped Xen loglvl=all/guest_loglvl=all for the same
        # reason. Bump these back up only when chasing an early-boot kernel issue.
        xen_cmdline="dom0_mem=${sim_dom0_mem_mb}M,max:${sim_dom0_mem_mb}M dom0_max_vcpus=1 console=com1,vga com1=115200,8n1"
        dom0_cmdline="console=hvc0 loglevel=4 rotten_apple.user_root_uuid=sim-root rotten_apple.no_autostart=1${break_arg}"
        ;;
esac

# --dom0-pin N: append the pin hint that the REAL laptop GRUB entry carries but
# the sim normally omits. This reproduces the laptop-only `xl vcpu-pin` branch
# in /init — the confirmed wedge — so we can prove a fixed /init survives it.
if [[ -n "$dom0_pin" ]]; then
    dom0_cmdline="$dom0_cmdline rotten_apple.dom0_pinned_cpu=$dom0_pin"
fi

if [[ "$dom0_cmdline" == *,* ]]; then
    echo "internal error: dom0 cmdline contains a comma (would break -initrd module split): $dom0_cmdline" >&2
    exit 3
fi

# The first multiboot module is the dom0 kernel; Xen treats the leading token
# (here the file path) as the module name and passes the rest as the dom0
# command line (same role 'placeholder' plays in the real GRUB module2 line).
# The second module is the cpio rootfs (no cmdline).
initrd_arg="$workdir/vmlinuz $dom0_cmdline,$workdir/thin-dom0.cpio.gz"

qemu-img create -f raw "$workdir/bootlog.img" 128M >/dev/null

guestfish -a "$workdir/bootlog.img" <<'EOF' >/dev/null
run
part-disk /dev/sda mbr
mkfs ext4 /dev/sda1
mount /dev/sda1 /
mkdir /rotten-apple
mkdir /rotten-apple/log
write /rotten-apple/vmlinuz marker
mkdir /boot
mkdir /boot/rotten-apple
write /boot/rotten-apple/vmlinuz marker
EOF

pick_accel() {
    if [[ "$accel" == "kvm" || "$accel" == "tcg" ]]; then
        printf '%s\n' "$accel"
        return
    fi
    if [[ -r /dev/kvm ]] && qemu-system-x86_64 -accel help 2>/dev/null | grep -qx 'kvm'; then
        printf 'kvm\n'
    else
        printf 'tcg\n'
    fi
}

qemu_accel="$(pick_accel)"

# Nested Xen needs the GUEST cpu to expose virtualization extensions, or Xen
# boots but can't run HVM/PVH guests — and on several Xen versions dom0
# bring-up itself stalls. The pre-v2 harness passed NO -cpu, so QEMU used
# the default qemu64 model with no vmx/svm: a prime suspect for the
# "boots then halts" behaviour under nested KVM. Pass the host CPU through
# under KVM (exposes VT-x on this Intel laptop); use -cpu max under TCG so
# the extensions are at least advertised to the emulated guest.
if [[ "$qemu_accel" == "kvm" ]]; then
    qemu_cpu="host"
else
    qemu_cpu="max"
fi
serial_log="$workdir/qemu-serial.log"

echo "workdir: $workdir"
echo "mode:    $mode"
echo "accel:   $qemu_accel"
echo "cpu:     $qemu_cpu"
echo "timeout: ${timeout_sec}s"
echo "dom0:    ${sim_dom0_mem_mb} MB"
echo "memory:  ${memory_mb} MB"
echo "smp:     $smp"
echo "xen:     $xen_basename (decompressed -> $workdir/xen)"
echo
echo "xen cmdline:  $xen_cmdline"
echo "dom0 cmdline: $dom0_cmdline"
echo
echo "booting nested ThinDom0 (direct multiboot, no GRUB/OVMF)..."

set +e
timeout "${timeout_sec}s" qemu-system-x86_64 \
    -machine "q35,accel=$qemu_accel" \
    -cpu "$qemu_cpu" \
    -m "$memory_mb" \
    -smp "$smp" \
    -kernel "$workdir/xen" \
    -initrd "$initrd_arg" \
    -append "$xen_cmdline" \
    -drive "file=$workdir/bootlog.img,format=raw,if=virtio" \
    -nic none \
    -vga none -display none -serial stdio -monitor none -no-reboot \
    >"$serial_log" 2>&1
qemu_rc=$?
set -e

echo
echo "QEMU exit code: $qemu_rc"
echo
echo "--- serial ---"
sed -n '1,400p' "$serial_log"

echo
echo "--- persistent logs ---"
if guestfish -a "$workdir/bootlog.img" -m /dev/sda1 ls /rotten-apple/log >/tmp/ra-thindom-loglist.$$ 2>/dev/null; then
    cat /tmp/ra-thindom-loglist.$$ || true
    while IFS= read -r log_name; do
        [[ -n "$log_name" ]] || continue
        echo "--- /rotten-apple/log/$log_name ---"
        guestfish -a "$workdir/bootlog.img" -m /dev/sda1 cat "/rotten-apple/log/$log_name" | sed -n '1,260p'
    done < /tmp/ra-thindom-loglist.$$
else
    echo "(none)"
fi
rm -f /tmp/ra-thindom-loglist.$$

echo
echo "artifacts:"
echo "  xen (mb):   $workdir/xen"
echo "  scratch fs: $workdir/bootlog.img"
echo "  serial log: $serial_log"

if [[ $qemu_rc -eq 124 ]]; then
    echo
    echo "note: timeout hit before QEMU exited. For serial-debug that usually"
    echo "means /init reached its cockpit respawn loop (dom0 stayed up) — read"
    echo "the serial + persistent logs above to confirm where it got to."
fi
