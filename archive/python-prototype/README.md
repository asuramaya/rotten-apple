# archive/python-prototype/

The original Python implementation of rotten-apple, kept for reference
after the project pivoted to all-Rust on 2026-05-01.

This is *prototype* code. It validated the design — manifest schema,
wizard UX, lift/unlift lifecycle, harness pattern — but is not what
gets shipped. The Rust workspace at the project root is the real
product. Each Python file here has a Rust counterpart (or will).

## Contents

| File | What it did | Rust counterpart |
|---|---|---|
| `rotten-apple` | The lifter CLI. Detects host state, installs Xen, modifies GRUB, regenerates initramfs with safety backups, manages an `/etc/default/grub` managed block. ~880 lines, 25 unit tests. Ended at v0.0.4 with `--iommu-force`, `--dom0-pvh`, `--serial-console` cmdline tunability. | `crates/cli` + `crates/bootstrapper` |
| `test_lifter.py` | unittest suite for the lifter. 25 tests covering GRUB parsing, managed-block patching, idempotent re-application, finalize round-trip, wizard phase-router state machine, Xen cmdline builder, firstboot template substitution. | `cargo test` per crate |
| `orchestrator/manifest.py` | Profile / BackendCapabilities dataclasses, TOML loader, capability-based validator. ~360 lines. Loaded both example manifests cleanly; correctly distinguishes Xen vs Hyper-V capability mismatches. | `crates/manifest` |
| `runner.py` (was `harness/runner.py`) | Nested-VM dev harness. `provision`/`boot`/`ssh`/`exec`/`reboot`/`kill`/`snapshot`/`reset`/`list`/`status`/`rm` subcommands. Downloads Ubuntu cloud image, builds cloud-init seed via xorriso, manages QEMU lifecycle with iommu_platform=on virtio devices for Xen-as-guest compatibility. | `crates/harness` (probably) |

## What we proved with this prototype

- Xen 4.20 packages install cleanly on Ubuntu 25.10 / kernel 6.17
- The lifter's GRUB integration works (verified through the harness)
- Three real cmdline-tunability bugs found and fixed via harness iteration:
  - `iommu=force,strict` panics on hosts without working IOMMU
  - `dom0=pvh` panics on hosts without PVH-on-VMX support
  - Default cmdline must be conservative; aggressive features opt-in
- The harness has a structural ceiling for nested-Xen testing — PV grant
  tables collide with emulated IOMMU MGAW (`0x800000000000b000` faults),
  PVH dom0 hangs at SMP bringup. These are nested-virt artifacts, not
  Xen bugs. Real-hardware validation has to happen on real hardware.
- The manifest schema discriminates correctly between Xen and Hyper-V
  capability matrices; the `attestation_required + tpm.mode = hardware-passthrough`
  combination produces 2 errors on Xen, 1 error on Hyper-V (matching
  what the architecture document promises).
- The wizard pattern works: phase-aware single entry point with
  state-machine routing across `fresh`/`pending_finalize`/`stuck`/`done`
  is a clean UX over the underlying primitives.

## What we DIDN'T prove

- Whether Xen actually boots dom0 cleanly on the user's real hardware.
  The harness's nested-KVM environment can't validate this; only real
  hardware can.
- The Phase 2 architecture (minimal dom0 image + Ubuntu-as-domU). The
  Python lifter targeted Phase 1 (existing Ubuntu becomes dom0), which
  is the wrong shape for the product. Phase 2 starts in Rust from
  scratch.

## Why we pivoted

The orchestrator daemon must run inside dom0's initramfs as PID 1 (or
near it). Python's runtime size, startup cost, and dependency footprint
make it a poor fit for that role. Once Rust is mandatory for the
daemon, going Rust for the rest of the project (lifter, harness,
backends) wins on:

- Single binary distribution (one `rotten-apple` executable, not
  Python + scripts)
- Single source of truth for the manifest schema (no two parsers to
  keep in sync)
- One language across the codebase
- Auditability for boot infrastructure
- Cross-platform parity (Hyper-V backend on Windows is first-class
  with Rust; Python on Windows is workable but messy)
