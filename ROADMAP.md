# rotten-apple roadmap

> **Working memory.** Update this file as we land work, hit decisions, or
> discover constraints. The version on disk is the source of truth — when
> a session ends, this should reflect what's done, what's next, and why.
>
> Format conventions:
> - `[x]` = done and verified
> - `[~]` = in progress (current focus)
> - `[ ]` = not started
> - Phases are gated by dependency, not preference. Don't skip ahead.

---

## Where we are right now

- **Current phase:** 2.2 — First backend (lifecycle landed); 2.4 prep underway (lift-readiness probes)
- **Migration mode:** user is willing to lift this dev machine onto real Xen and iterate the rest of the project from inside the resulting Ubuntu domU. This collapses the "discuss → code → ship → test in 6 months" loop into "code → lift → iterate on hardware." Several questions become decided rather than open.
- **Last landed:** `rotten-apple install` subcommand. Standalone install (binary → `/usr/local/bin/`, desktop launcher → `/usr/share/applications/`) without committing to a Xen lift. Same code path the lift uses internally, so `install` and `lift`'s install-bits are byte-identical. Cockpit prints a one-line install hint when launched from outside `/usr/local/bin/rotten-apple` so the discoverability path is: `cargo build` → `sudo ./target/release/rotten-apple install` → `sudo rotten-apple cockpit` → `e` to lift from inside the TUI. Plus the auto-config from the previous pass (cockpit `[P]romote Xen to GRUB default` action with brace-aware `grub.cfg` parser, `submenu_id>menuentry_id` resolution, parser unit tests for Advanced-Options-submenu and top-level cases). **77 workspace tests, all green.**
- **Live result on this machine (2026-05-04):** clean lift target. `/` on LVM-on-LUKS (`/dev/mapper/ubuntu--vg-ubuntu--lv`, ext4); `/boot` plaintext ext4 on `/dev/nvme0n1p2` (pygrub-readable); iGPU `8086:46a6` (Alder Lake) **alone in IOMMU group 0** — clean GPU passthrough, zero collateral; `grub-efi-amd64-signed` installed (Secure Boot path supported); 34 IOMMU groups (fine-grained); no hibernation; no blockers; one warning (LUKS on /, operationally fine — domU prompts for passphrase like bare-metal does).
- **Next concrete action (3 commands, end-to-end):** `cargo build --release && sudo ./target/release/rotten-apple install && sudo rotten-apple cockpit --manifest manifests/this-machine-ubuntu-domu.toml`. The install puts the binary on PATH; the cockpit auto-detects state and either shows AwaitingReboot (current state — reboot, pick the Xen entry) or PreLift (`e` to lift) or Active (post-Xen, manage domains). Single binary, single entrypoint, three lines.
- **Blockers:** none.

---

## Phase 2 — All-Rust product (current)

### 2.1 Workspace foundation ✅

- [x] Cargo workspace at project root (`Cargo.toml`, edition 2024, resolver 3)
- [x] `crates/manifest` — Profile schema, TOML loader, capability validator
  - 8 tests (3 unit, 4 integration against `manifests/*.toml`, 1 doctest)
- [x] `crates/backend` — trait + types from `design/backend-trait.md` (4 tests, no impls yet)
- [x] `crates/detect` — host inspection (5), `lift_readiness` (partition / LUKS / IOMMU groups / GRUB flavor, 8), `topology` (CPU hybrid P/E detection, 6), `planner` (pure-function host-facts → dom0/domU resource split, 6). 25 tests in this crate. Footprint is **derived from host**, not hardcoded — required because rotten-apple is OSS and runs on arbitrary x86_64 hardware. Rules: dom0 RAM = `clamp(round64(total * 3%), 768, 4096)`; dom0 CPUs = E-cores capped at 4 (hybrid) or 1–4 lowest-indexed cores (uniform); domU gets the rest. All numbers are recommendations the user can override.
- [x] `crates/cli` — single `rotten-apple` binary, clap-based subcommand routing
  - `rotten-apple detect` works against the live host
  - `rotten-apple manifest validate <path>` validates manifests against both reference backend caps
  - `rotten-apple lift-readiness` runs Phase-2-lift-specific probes
  - `rotten-apple plan-lift` computes the recommended dom0/domU split for this host
  - Release binary builds in ~12 s

### 2.2 First backend

- [x] `crates/backend-xen` skeleton + bindgen FFI to libxl (782 fns, 142 types)
  - Build deps: `libxen-dev` (headers), `clang` (libclang for bindgen), runtime `libxenlight.so.4.20`
  - Links: `libxenlight`, `libxenstore`, `libxenctrl`, `libxentoollog`
  - Sanity tests pass; `sys::libxl_*` types resolve at compile time
- [x] **Decision:** libxl FFI from the start, NOT `xl` shell-out. Minimal-overhead is the design rule. (User explicit, 2026-05-01.)
- [x] Safe `Ctx` wrapper around `libxl_ctx` — init, teardown, logger callback, drop-ordering, error mapping (5 tests; failing path verified end-to-end via libxl's own diagnostics)
- [x] **Decision:** `libxl_error` uses bindgen `newtype_enum` (not strict Rust enum) so we can construct `libxl_error(code)` from any i32 returned by libxl at runtime without UB.
- [x] **Decision:** `HypervisorBackend` trait is `Send` only, not `Send + Sync`. libxl is single-thread per ctx; orchestrator runs single event loop. Backend can move between async tasks but can't be shared concurrently.
- [x] `XenBackend` struct holds `RefCell<Ctx>` for interior-mutable libxl access from `&self` trait methods
- [x] Implement `name()`, `capabilities()`, `list()` (calls `libxl_list_domain` + per-domain `libxl_domid_to_name`, properly frees both)
- [x] Implement `start_guest()` (libxl_domain_unpause), `stop_guest()` (shutdown / destroy), `destroy_guest()` (libxl_domain_destroy)
- [x] Implement `balloon_to()` (libxl_set_memory_target, absolute, enforce=1)
- [x] Implement `status()` (libxl_domain_info → typed GuestStatus; uptime is currently ZERO — TODO add proper tracking)
- [x] `parse_domid()` helper; rejects non-numeric handles with GuestNotFound
- [x] `create_guest()` — translates `Profile` → `libxl_domain_config` via the two-layer split in `crates/backend-xen::config`: (1) `DomainConfigPlan` (pure-Rust, 7 unit tests covering pvh/hvm/disk-prefix-stripping/extra-disks/missing-source/etc.) and (2) `OwnedDomainConfig` (libxl FFI materialization with `libc::strdup` for strings, `libc::calloc` for device arrays, `Drop` calls `libxl_domain_config_dispose` on every exit path). Selects mode via `mode::select_mode`; pygrub for PV/PVH, no bootloader for HVM; PHY backend for disks; VIF (paravirt) or VIF_IOEMU (HVM) NICs on `xenbr0`. Untested against real Xen; first real validation runs at lift time.
- [ ] Implement `pin_vcpus()` (libxl_set_vcpuaffinity + libxl_bitmap construction)
- [ ] Implement `suspend()` (libxl_domain_suspend_only — needs temp-file fd plumbing) and `resume()`
- [ ] Implement `passthrough_pci()` (libxl_device_pci_add + struct construction), `revoke_pci()`, `attach_usb()`, `detach_usb()`
- [ ] Integration test: spawn a paused stub guest, list it, destroy it (requires real Xen host)

### 2.3 / 2.4 First lift — collapsed into "host becomes dom0" (v0.0.1)

**Architectural pivot (2026-05-04, user-explicit):** "the bottom layer has to own the hardware, that's an orchestration problem."

The original Phase 2 plan (minimal dom0 in a separate LV; existing Ubuntu becomes a domU) requires a full PV presentation layer in dom0: PV input, PV framebuffer, Wi-Fi NAT, viewer. That's months of work — and gating the first reboot on it means no validation of the orchestrator path against real Xen for that whole time.

Probed this laptop's hardware: no ethernet (Wi-Fi only — can't bridge in Linux), trackpad on i2c, keyboard on i8042/PS-2, lid/power on ACPI, two GPUs (Iris Xe + RTX A3000). PCI passthrough of any of this to a domU is non-trivial. Wi-Fi cannot be bridged at all.

**v0.0.1 collapse:** for the first lift, the bare-metal Ubuntu *becomes* dom0. apt installs Xen, our orchestrator runs as a systemd service inside that dom0. Hardware "just works" because the same drivers run in the same kernel — only the Xen layer is new. The orchestrator runs in `--check` mode (open libxl, list domains, exit) on each boot to prove the path. Real guest creation comes after we have a tractable test guest disk image (v0.0.2+).

The Phase 2 minimal-dom0 + Ubuntu-as-domU shape is preserved as the long-term goal; it gets unlocked once we've built PV input + viewer + Wi-Fi NAT in dom0. Migration path: build those PV pipelines as `crates/dom0-presenter` (or similar); when ready, lvcreate a small dom0 root, debootstrap into it, switch the Ubuntu rootfs to be a domU pointing at the original disk.

- [x] `crates/orchestrator` — daemon (lib+bin). PID-1-aware (reboots via libc::reboot on shutdown if init). Two modes: `--check` (one-shot connect/list/exit, used by systemd service) and full (create_guest + start_guest + block on signal). Signal handlers via libc::sigaction. 3 tests.
- [x] `crates/bootstrapper` — v0.0.1 procedural lift, "host becomes dom0":
  1. pre-flight (refuses on lift_readiness blockers)
  2. apt install xen-system-amd64 (DEBIAN_FRONTEND=noninteractive, --no-install-recommends)
  3. install orchestrator binary to /usr/local/bin (chmod 755)
  4. install /etc/rotten-apple/active.toml
  5. install systemd unit (After=xen-init-dom0.service xenstored.service network-online.target; Restart=on-failure)
  6. systemctl enable
  7. write /etc/default/grub.d/40-rotten-apple.cfg with planner-derived `GRUB_CMDLINE_XEN_DEFAULT="dom0_mem=NM,max:NM dom0_max_vcpus=N dom0_vcpus_pin"`
  8. update-grub
  9. verify both 'Ubuntu' and 'with Xen hypervisor' menuentries present in /boot/grub/grub.cfg
  - Dry-run by default; `--execute` to run.
- [x] CLI `lift` subcommand wired; dry-run smoke tested on this host.
- [ ] Run with `--execute` on the laptop (requires root + ~3-5 minutes for apt install).
- [ ] Reboot, pick 'Ubuntu GNU/Linux, with Xen hypervisor' at GRUB menu.
- [ ] Validate: `xl list` shows Domain-0; `systemctl status rotten-apple-orchestrator` shows clean exit (--check mode); existing Ubuntu desktop fully working under Xen.

### 2.4b The cockpit (TUI control surface)

- [x] `crates/cockpit` — interactive ratatui TUI. Architecture: main thread runs UI loop (~20 fps draw, key event poll); worker thread owns the `XenBackend` (libxl_ctx is `!Sync`, so single-actor ownership enforces correctness); std mpsc channels for commands and snapshot/event messages. 1 Hz background polling; `r` triggers immediate refresh. No tokio.
- Layout: header bar (libxl version + domain count) | left domain list | right detail (selected guest stats + per-event log) | bottom keybinding hints. Status colors (green=running, yellow=idle, red=failed). State glyphs.
- Keybindings: `s` start, `x` graceful stop, `X` destroy (asks for confirmation), `b` balloon (inline number prompt), `c` create from manifest, `r` refresh, `m` (manifest editor — deferred), `?`/`F1` help overlay, `q`/`Ctrl-C` quit, `Esc` cancels overlays/prompts.
- Toast overlay for action results (info/success/error, 4 s TTL). Help overlay + balloon prompt + destroy-confirm modals.
- Backend-failure path: when libxl can't be opened (non-Xen host, non-root), the UI still draws — backend error shown in the detail pane, keybindings stay visible, `q` quits cleanly. Failing actions toast the error.
- Location-agnostic: same binary works locally on dom0 (sudo) or over SSH (ssh into dom0, then run cockpit there — terminal is the rendering target either way).
- CLI: `rotten-apple cockpit [--manifest <path>]`. Default manifest path matches bootstrapper output (`/etc/rotten-apple/active.toml`).
- 5 unit tests (truncate, state glyphs, event kinds, initial state).

### 2.5 PV presentation layer in dom0 *(unlocks Phase 2 proper)*

- [ ] PV input pipeline: dom0 owns kbd/trackpad/mouse drivers; xen-kbdback exposes events to domU's xen-input-front
- [ ] PV framebuffer + viewer: dom0 runs a minimal Wayland session with a viewer that displays the active guest's framebuffer fullscreen
- [ ] Wi-Fi NAT: xenbr0 as internal-only bridge; dom0 NATs domU traffic via NetworkManager-managed Wi-Fi
- [ ] GPU hot-swap: orchestrator can move iGPU between dom0 (no guest active) and the active desktop guest
- [ ] Audio PV: xen-pcm or PipeWire passthrough
- [ ] Once these work: lvcreate dom0root, debootstrap, switch existing Ubuntu rootfs to be a domU. This is the original Phase 2 architecture.

### 2.5 Real-hardware gating *(v0.1 ship gate)*

- [ ] `crates/harness` — Rust port of Python harness for nested-VM dev testing
- [ ] Run on real hardware (the Dell 5770: 12900H, 64 GB, Iris Xe + RTX A3000)
- [ ] Iterate any real-hardware-specific issues that come up
- [ ] If the real boot works: Phase 2 is real

### 2.6 First desktop guest validated

- [ ] Ubuntu domU boots identically to bare-metal (same files, network, desktop)
- [ ] dom0 footprint bounded: ~1.5 GB RAM, 2 E-cores
- [ ] Document quirks (suspend behavior, Optimus, etc.) in `design/known-quirks.md`
- [ ] **v0.1 declared shipped**

---

## Phase 3 — Hyper-V backend

- [ ] `crates/backend-hyperv` — implements the trait, runs as a Windows service inside the Windows root partition
- [ ] MSDM ACPI key extraction (read `/sys/firmware/acpi/tables/MSDM` offset 0x36 from Linux)
- [ ] wimlib-based unattended Windows install — no Microsoft installer dialog
- [ ] unattend.xml automation: OEM key, auto-enable Hyper-V role, install rotten-apple-orchestrator service, configure Ubuntu child VM auto-start
- [ ] CLI `--backend hyperv` wires this up; same `lift` command, additive ~30 GB Windows partition install, Ubuntu untouched

---

## Phase 4 — First real appliance: iCloud Keychain

- [ ] Build `tiny11-icloud-passwords.qcow2` image (wimlib + the `.appx` we have)
- [ ] Vsock-tunneled native-messaging bridge:
  - Browser-side: au2001-based Firefox extension adapter
  - dom0 side: socket bridge between native-messaging stdin/stdout and vsock
  - Guest side: Windows service forwarding to iCloud Passwords helper
- [ ] Wake-on-trigger orchestrator extension: browser opens native-messaging port → orchestrator wakes appliance from suspend
- [ ] **Original "iCloud Keychain on Linux" use case working end-to-end** — the demo that started the project

---

## Phase 5 — Composition *(open-ended)*

- [ ] Microsoft Store host appliance — generic UWP app runtime ("Microsoft Store on Ubuntu" demo)
- [ ] macOS Hackintosh desktop guest (OpenCore-based, Apple x86 attestation is lenient enough)
- [ ] Display routing / GPU hot-swap when adding a second desktop guest
- [ ] Resource arbitration daemon — active-guest detection, balloon orchestration, vCPU pinning
- [ ] Hardened browser appliance (dom0-side Tor wireguard, virtio-net only)
- [ ] Profile registry — signing, updates, install/uninstall flows

---

## Decisions made *(context for future sessions, do not delete)*

### Architecture
- **Two-target invariant:** Xen and Hyper-V backends share the same daemon, manifest schema, CLI, wizard. Bifurcation is a contract violation. (`design/architecture.md`)
- **Phase 2 over Phase 1:** the lifter does NOT put Ubuntu in dom0. It builds a minimal dom0 image; Ubuntu becomes a domU pointing at the existing root partition. (User explicit, 2026-04-30.)
- **All-Rust:** prototype done in Python (now in `archive/`); product is Rust. One binary, multiple subcommands. (User explicit, 2026-05-01.)
- **One binary, plug-in backends:** `crates/backend-xen` and `crates/backend-hyperv` are Cargo crates that implement the same trait. The orchestrator binary loads the appropriate backend at runtime based on substrate.
- **dom0 is a resource broker, not a desktop:** dom0 owns hardware (Wi-Fi, NIC, audio mixer) but exposes virtualized handles to guests. ~1.5 GB / 2 E-cores. The user lives in Ubuntu domU.

### Backend implementation
- **libxl FFI from the start, not xl shell-out.** Minimal overhead is the design rule. Shell-out would add ~5 ms per backend call (negligible) but +5 MB to dom0 initramfs and adds subprocess crash modes the orchestrator must handle. FFI is ~2-3 sessions of plumbing work before behavior shows up; user accepted the slow start. (2026-05-01.)
- **bindgen at build time, not vendored bindings.** Re-runs whenever `wrapper.h` or build.rs changes; tracks libxl-dev version at build site. Allowlist filters keep output focused on `libxl_*` / `xtl_*` / `LIBXL_*`.
- **Lint allows scoped to `mod sys`.** Bindgen's Rust-2021-shaped output trips Rust 2024's stricter `unsafe_op_in_unsafe_fn` lint; allow is on the module, not the crate, so safe wrappers stay held to project style.
- **`libxl_error` is a newtype enum (not strict Rust enum).** libxl returns arbitrary integers at runtime; constructing a strict enum from an unrecognised value is UB in Rust 2024 (panics in debug). Newtype lets us safely accept any i32 and pass it back to `libxl_error_to_string` for the message.
- **Logger lives ≥ ctx.** `Drop for Ctx` is `libxl_ctx_free` then `xtl_logger_destroy`. Reverse order would segfault libxl on a final log line during ctx teardown.
- **Ctx is `Send + !Sync`.** libxl_ctx is documented as not concurrent-safe per ctx; multiple ctxs per process are allowed but each must be used single-threaded. PhantomData<*mut ()> field removes Sync; manual `unsafe impl Send` puts Send back.
- **Async mode: blocking only for v0.1.** Every libxl call passes NULL for `libxl_asyncop_how`. Callbacks-with-lifetimes-in-unsafe-Rust is a footgun we don't pay for until the orchestrator actually needs concurrent guest operations.
- **`HypervisorBackend: Send` not `Send + Sync`.** libxl_ctx is documented as not concurrent-safe per ctx; HCS handles are similar. Orchestrator runs a single event loop. Backend can move between async tasks but can't be shared concurrently across threads. Saved us from `Mutex<Ctx>` (runtime cost) and lets backends use `RefCell` for interior mutability under `&self` trait methods.
- **`XenBackend` uses `RefCell<Ctx>`.** All trait methods are `&self`; libxl always wants `*mut libxl_ctx`. RefCell gives interior mutability with runtime borrow check — never expected to fail because the type isn't Sync.
- **Xen domain mode (PV / PVH / HVM) is NOT a manifest field.** Selected at `create_guest` time by the Xen backend from Profile + host signals. Why: the manifest is the hypervisor-agnostic contract (what the user wants); the mode is a Xen-specific implementation detail (how the Xen backend expresses it). Putting it in the manifest would leak Xen vocabulary into Hyper-V territory and break the two-target invariant. (User explicit, 2026-05-04: "dynamic honestly, its part of orchestration".) Selector lives in `crates/backend-xen::mode::select_mode`. Rules: TPM requested → HVM (PVH has no vTPM); root disk carries Linux fs → PVH; otherwise HVM as the safe default. Probe is a trait seam (`DiskProbe`) so tests don't need real disks. PV is never auto-selected; deprecated for new Linux guests in favor of PVH.
- **Storage controllers stay with dom0; never PCI-passthrough'd.** Disks reach guests via the libxl `phy:` backend (xen-blkfront in the guest, xen-blkback in dom0), not via passthrough of the NVMe/SATA controller itself. Why: future guests share the same physical disk (Ubuntu domU on /dev/nvme0n1, Windows appliance on a qcow2 file on /dev/nvme0n1, etc.). PCI-passthrough'ing the controller would lock all storage into a single guest. (User explicit, 2026-05-04: "we may mount other vms on the same disk so the pci must belong to xen".) GPU is the opposite case — there's only one and it goes to the active desktop, so PCI passthrough is correct there.
- **Bootloader: pygrub.** Ubuntu domU boots via `bootloader = "pygrub"`; libxl reads the GRUB config out of the guest's `/boot` at start time. Alternative was bootstrapper-copies-vmlinuz-into-dom0; rejected because it desynchronises from the guest's apt-managed kernel updates. (User explicit, 2026-05-04: "explicit kernel is counter productive would rather sync".)
- **libxl version detection at build time.** `build.rs` runs `pkg-config --modversion xenlight` (falling back to `dpkg-query`) and emits cfg flags: `xen_4_20`, `xen_4_19_or_later`, etc. All hand-written code that touches a `libxl_*` struct field goes through `crates/backend-xen::compat`. When a field name shifts between minor versions, the helper there carries `#[cfg(xen_4_xx)]` gates; callers don't see the diff. Why: writing for one specific Xen version is fragile; libxl struct internals are NOT ABI-stable across minor releases. (User explicit, 2026-05-04: "writing for one specific shape is fragile".) Currently building against 4.20.0 from Ubuntu 25.10's libxen-dev.

### Lifter behavior
- Default Xen cmdline conservative: `iommu=on`, no `dom0=pvh`. Aggressive features opt-in via `--iommu-force` / `--dom0-pvh`. (Discovered via Python prototype harness experiments.)
- `--serial-console` adds `console=com1,vga` to Xen and `console=hvc0 earlyprintk=xen` to dom0 for diagnostic visibility.
- Initramfs backed up before regen; auto-rollback if it shrinks suspiciously.
- `grub.cfg` post-write verification asserts BOTH Xen entry AND bare-metal entry present; otherwise roll back.
- Bare-metal Ubuntu entry preserved at all times. Worst-case recovery: hold Shift at boot, pick "Ubuntu", run `unlift`.

### Hyper-V variant
- Uses MSDM ACPI table for OEM Windows key recovery (`/sys/firmware/acpi/tables/MSDM` at offset 0x36, 29-char key)
- Uses `wimlib` to apply Windows install image from Linux — no Microsoft installer dialog
- Architecture: Hyper-V bottom + Windows root partition (Tiny11) + Ubuntu child partition. Additive ~30 GB Windows partition; existing Ubuntu untouched.
- Vanguard's tolerance of "Linux child partition under Windows root" is unverified; documented as best-effort.
- Trick to know: spoofing Hyper-V at the CPUID/hypercall level is already done by Xen's `viridian=1`. The TPM attestation layer is the actual wall — and the Hyper-V variant clears it because Hyper-V owns the bare-metal TPM.

### Capability matrix (committed)
- See `design/architecture.md` for the table.
- Xen: ~95% of Microsoft Store catalog, no WHfB, no anti-cheat games, no 4K DRM.
- Hyper-V: adds WHfB, BitLocker hw-sealed, 4K DRM. Vanguard maybe.
- Apple Silicon-gated services (some iCloud features): no path either way.

---

## Open questions / unknowns *(to attack via real-hardware testing)*

- [x] ~~Manifest needs an explicit Xen domain type field~~ — **decided: no.** Mode selection is orchestration, not configuration. Lives in `crates/backend-xen::mode`. (2026-05-04)
- [x] ~~Disk + bootloader strategy for Phase 2 Ubuntu domU~~ — **decided.** Whole-disk via libxl `phy:` backend (`phy:/dev/nvme0n1,xvda,w`); storage controller stays with dom0 (multiple guests share the disk). Bootloader = `pygrub`; syncs with apt kernel updates. (2026-05-04)
- [ ] **`create_guest` validation strategy.** Without real Xen, tests can only check "build the config, call libxl, get NoPerm". That validates memory safety, not semantics. Real validation requires Phase 2.5 (real-hardware run).
- [ ] **Will Xen 4.20 boot cleanly on Intel Alder Lake hybrid P/E cores?** Some older Xen versions had quirks; 4.20 should be fine but unverified for 12900H specifically.
- [ ] **Does PV vs PVH dom0 differ meaningfully on real hardware?** Bootstrapper currently defaults to PV (more mature); PVH is recommended but could expose nested-virt-shaped issues. Default needs validation.
- [ ] **Will dom0 driving the iGPU (i915) under PV/PVH have any compositor quirks?** Well-tested in general, unknown for this exact stack.
- [ ] **Suspend/resume on the laptop.** Historically iffy under Xen. Real-world impact: lid-close might not resume cleanly. Workaround: lock-on-lid instead of suspend-on-lid.
- [ ] **Vanguard heuristics for Hyper-V root + actively-used Linux child partition.** Architecturally legitimate (Windows IS root) but Riot's specific detection is opaque.

---

## Reference material in the repo

- `design/architecture.md` — the contract
- `design/backend-trait.md` — adapter spec, pseudo-Rust
- `manifests/*.example.toml` — wire format
- `archive/python-prototype/` — Python prototype with its own README explaining what each file did and which Rust crate replaces it
- `icp-firefox/` — au2001's iCloud Passwords Firefox extension (protocol code for Phase 4 appliance)
- `AppleInc.iCloud_15.7.56.0_x64.appx` — Apple's iCloud for Windows .appx (binary source for Phase 4 appliance image)

---

## Session log *(append-only, brief)*

- **2026-04-29** — Project began as "iCloud Keychain on Linux." Architecture explored end-to-end (lifter, orchestrator, Hyper-V backend, attestation, OpenCore). Python prototype built: `rotten-apple` lifter + harness + manifest schema + wizard.
- **2026-04-30** — Harness experiments surfaced four real bugs (`iommu=force,strict` panics, `dom0=pvh` panics in nested KVM, missing initramfs xen modules, `VIRTIO_F_ACCESS_PLATFORM` requirement) that became `--iommu-force` / `--dom0-pvh` / `--serial-console` opt-in flags + `intel-iommu` + `iommu_platform=on` harness fix. Architecture pivot recognized: Phase 1 (Ubuntu-as-dom0) is the wrong shape — Phase 2 (minimal dom0 + Ubuntu-as-domU + orchestrator) is the product. Safety hardening landed (initramfs backup, grub.cfg dual-entry verification, /boot space pre-flight check).
- **2026-05-04** — User declared willingness to lift this dev machine onto real Xen and iterate from inside the resulting Ubuntu domU. Path-forward reordering: lift dependencies (manifest schema for Xen, `create_guest`, dom0-image, bootstrapper) become the critical path; harness becomes secondary. `crates/detect::lift_readiness` landed: partition discovery via `findmnt`, LUKS detection via `lsblk -s`-walk-up, full IOMMU group enumeration with PCI class translation, hibernation config, GRUB flavor. CLI `rotten-apple lift-readiness`. Live run on this machine returned a clean target: LVM-on-LUKS / on ext4, plaintext ext4 /boot pygrub-readable, iGPU `8086:46a6` alone in IOMMU group 0, signed-grub installed, no hibernation, zero blockers. User immediately pushed back on the "1.5 GB / 2 E-cores hardcoded dom0" plan — this is OSS, footprint must come from host facts. Built `crates/detect::topology` (Intel hybrid P/E split via `/sys/devices/cpu_core` + `cpu_atom`) and `crates/detect::planner` (pure-function policy: dom0 RAM = clamp(round64(total*3%), 768, 4096); dom0 CPUs = E-cores capped at 4 for hybrid OR 1-4 lowest-indexed cores for uniform; domU gets the rest with sensible idle/min ballooning bounds). CLI `rotten-apple plan-lift` prints the proposed split with rationale. Tested for 4-core 8 GB mini-PC, 12-core 64 GB hybrid laptop, 64-core 128 GB server, 16-core 256 GB workstation — all sensible. Live on this machine: 1856 MB dom0 + 4 E-cores; 60.5 GB / 12 P-vCPU domU. Then user closed the manifest-Xen-mode question: "dynamic honestly, its part of orchestration." Built `crates/backend-xen::mode` — `XenDomainMode { Pv, Pvh, Hvm }` + `select_mode(profile, probe)` with rationale. Heuristic: TPM requested → HVM (no vTPM in PVH); root disk has ext/btrfs/xfs → PVH; else HVM. `DiskProbe` is a trait seam so unit tests don't need real disks. 4 new tests; 52 workspace total, all green. Then user closed three more questions in one go: (1) **whole-disk passthrough via `phy:` backend, NOT PCI passthrough of the storage controller** — controller stays with dom0 because future guests share the disk; (2) **pygrub** for the bootloader (sync with apt kernel updates over copy); (3) **libxl version detection at build time** — `build.rs` reads `pkg-config --modversion xenlight`, emits cfg flags `xen_4_20` / `xen_4_19_or_later` / etc., and a new `crates/backend-xen::compat` module wraps every struct-field access so version diffs land in one place. 54 workspace tests, all green. Building against libxl 4.20.0 from Ubuntu 25.10's libxen-dev. Then `create_guest` landed end-to-end: `crates/backend-xen::config` with two-layer split — pure-Rust `DomainConfigPlan` (7 unit tests) + unsafe `OwnedDomainConfig` (libc::strdup'd strings, libc::calloc'd device arrays, Drop calls libxl_domain_config_dispose on every exit path including partial-init failures). `XenBackend::create_guest` now: select_mode → plan → materialize → libxl_domain_create_new. Plus `manifests/this-machine-ubuntu-domu.toml` — concrete lift target for THIS laptop (60591M / 12 P-vCPU domU; /dev/nvme0n1 whole-disk passthrough; iGPU 0000:00:02.0 passthrough; pygrub; tpm=none → PVH). Validates clean against both Xen and Hyper-V backend capabilities (the abstraction held). 9 of 16 trait methods now real. 61 workspace tests, all green. Then the smoke-and-keep-going pass: added `xen list` / `xen try-create` CLI subcommands (proves error mapping works on non-Xen — libxl emits "Could not obtain handle on privileged command interface", we wrap in typed BackendError, exit 2). Built `crates/orchestrator` (lib+bin, PID-1-aware via getpid()==1 check — reboots via libc::reboot(RB_AUTOBOOT) instead of exiting if init). User pushed back on assumed custom-initramfs design ("why custom initramfs"); pivoted: dom0 footprint is enforced by Xen cmdline, userspace can be stock Debian. Built `crates/bootstrapper` — procedural lift: lvcreate dom0root on existing VG (auto-detect via findmnt + double-dash unescape), mkfs.ext4, mount, debootstrap stable, bind /proc /sys /dev /dev/pts, chroot apt install (linux-image-amd64 + xen-system-amd64 + cryptsetup-initramfs + lvm2 + bridge-utils + ifupdown + openssh-server), install orchestrator binary + systemd unit + manifest + xenbr0 network + crypttab passthrough + fstab + update-initramfs, unmount, write /etc/grub.d/40_rotten_apple with Xen menuentry, update-grub, verify dual-entry. All steps idempotent. `--dry-run` (default) prints choreography without acting. CLI `lift` subcommand wired. Dry-run on this host: 16 steps print clean. 69 workspace tests, all green.
- **2026-05-01** — Pivoted to all-Rust. Python prototype archived to `archive/python-prototype/` with README. Cargo workspace + `crates/manifest` landed (8/8 tests). This roadmap committed. Phase 2.1 completed: `crates/backend` (trait + types, 4 tests), `crates/detect` (host inspection ported from Python with regex tightened, 5 tests), `crates/cli` (clap-based binary `rotten-apple` with `detect` and `manifest validate` subcommands working end-to-end against the live host). 17 tests across the workspace, all green. Release binary builds ~12 s. Note: detect now reports NVIDIA proprietary loaded on the host — that wasn't true earlier in our Python detect runs; user must have switched drivers. Confirmed warning fires correctly. Phase 2.2 begun: `crates/backend-xen` skeleton with bindgen FFI to libxl. User installed `libxen-dev` + `clang`. Bindgen produces 782 libxl functions, 142 types, ~209 KB of generated bindings. Crate compiles, links, sanity tests pass. Safe `Ctx` wrapper landed: owns `libxl_ctx` + xtl logger, enforces drop ordering, maps libxl error codes to typed `BackendError`, runs without UB. Required swapping `libxl_error` to `bindgen.newtype_enum` (strict enum + transmute panics on unknown codes in Rust 2024) and adding `libxentoollog` to the link line (xtl_* lives in its own .so). Failing-path test produces real libxl diagnostics on stderr ("xencall: error: Could not obtain handle on privileged command interface"), proving the logger is wired up. `XenBackend` struct landed with non-mutating trait methods (`name`, `capabilities`, `list`) calling `libxl_list_domain` + `libxl_domid_to_name` properly with allocator/deallocator pairing. Mutating methods stub to `NotSupported`. Trait dropped `Sync` requirement (libxl is single-thread per ctx). Lifecycle methods landed: `start_guest` (libxl_domain_unpause), `stop_guest` (shutdown/destroy by force flag), `destroy_guest`, `balloon_to` (libxl_set_memory_target), `status` (libxl_domain_info → typed GuestStatus with proper init/dispose pairing on every exit path). 8 of 16 trait methods now real; 8 stubbed with specific reasons. `parse_domid` helper rejects non-numeric handles cleanly. 28 tests across workspace, all green.
