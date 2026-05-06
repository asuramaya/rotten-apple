# rotten-apple — Architecture

> Status: design committed, code partially landed (Xen lifter, v0.0.3).
> This document is the contract. Code must match it; if reality forces
> divergence, this document is updated first, then code.

## Product shape

rotten-apple is a **hypervisor-agnostic resource orchestrator** that
non-destructively transforms a stock Ubuntu install into a guest of a
hypervisor, while presenting the user's experience as identical to bare
metal except for the resource debt of the orchestrator itself.

There is exactly **one** orchestrator codebase. It speaks to the
underlying hypervisor through a backend adapter. Two backends are
officially supported:

- **Xen backend** — Track A. Linux dom0 runs the orchestrator daemon.
  Non-destructive lift of an existing Ubuntu install, no licence cost.
  Capability matrix omits anything gated on hardware-rooted Microsoft
  attestation (Windows Hello for Business, hardware-sealed BitLocker,
  PlayReady SL3000, Vanguard-class anti-cheat).
- **Hyper-V backend** — Track B. Windows root partition runs the
  orchestrator daemon. Additive install of a small Windows root
  alongside the existing Ubuntu install. Recovers the OEM Windows key
  from the MSDM ACPI table when available. Adds the full Microsoft
  attestation stack to the capability matrix; Vanguard support depends
  on Riot's heuristics and is documented as best-effort.

Both backends share the same manifest schema, the same daemon API, the
same wizard, the same CLI, and the same orchestration logic. The
hypervisor is a backend implementation detail invisible above the
adapter line.

## Two-target invariant

Bifurcation of the codebase between Xen-flavour and Hyper-V-flavour is
forbidden. If a feature is meaningfully different between backends, it
belongs in the backend trait (capabilities flag, behavioural divergence)
or in the manifest schema (declared per-profile). The orchestrator core
must remain hypervisor-agnostic.

Concretely, the following must be true at all times:

1. A profile manifest written for one backend boots correctly on the
   other backend if its declared capabilities are supported by that
   backend, with no manual editing.
2. The wizard prompts and decision flow are identical between backends
   except for backend-specific install steps (Hyper-V's Windows
   provisioning) and the final capability matrix shown to the user.
3. The orchestrator daemon's wire protocol with the CLI is identical.

## Bootstrapping

Both bootstrappers run **from inside a stock Ubuntu live or installed
session**. Ubuntu is the universal install bed because Linux can read
ACPI tables, manipulate partitions, mount NTFS, apply Windows images
via wimlib, and orchestrate complex multi-step provisioning with full
visibility — none of which is possible from a Windows installer
environment.

### XenBootstrapper

1. Pre-flight detect: distro, kernel, firmware (UEFI required), Secure
   Boot state, IOMMU, GPU, Xen package availability.
2. `apt install xen-system-amd64` (or fallback set).
3. Build minimal dom0 image (kernel + initramfs) containing the
   orchestrator daemon. Phase 1 reuses existing Ubuntu as dom0; Phase 2
   builds a true minimal dom0 from initramfs alone.
4. Add Xen multiboot GRUB entry alongside the bare-metal entry; set as
   default.
5. Generate the `ubuntu-desktop` profile manifest pointing at the
   user's existing root partition.
6. First lifted boot uses `--show-grub-menu` so the user can verify
   both entries are present.
7. After verification, `finalize` re-hides the menu.

### HyperVBootstrapper

1. Pre-flight detect: same as Xen plus MSDM ACPI table presence and
   readable Windows OEM key.
2. **OEM key recovery**:
   - Read `/sys/firmware/acpi/tables/MSDM` from Linux.
   - Verify the table format (signature `MSDM`, length, checksum).
   - Extract the 29-character product key from offset 0x36.
   - If absent or invalid, prompt the user for a manually supplied key.
3. **Partition layout**: shrink the existing Ubuntu partition by
   roughly 30 GB to make room for the Windows root. Non-destructive;
   `gparted`'s online resize for ext4. EFI partition reused.
4. **Windows install via wimlib**, no Microsoft installer:
   - Source: a curated Tiny11 `.wim` shipped or downloaded by
     rotten-apple, or a user-supplied Windows 11 ISO from which we
     extract `install.wim`/`install.esd`.
   - `wimapply install.wim 1 /mnt/windows-target` — applies the image
     to the new partition.
   - Generate `unattend.xml` with: the OEM key, auto-enable Hyper-V
     role, skip OOBE, install rotten-apple-orchestrator as a Windows
     service, configure auto-launch of the Ubuntu child partition.
   - Inject the unattend file into the Windows panther directory.
5. **Boot loader**: add a Windows Boot Manager entry that chains into
   the new Windows install. Add a "Windows + Hyper-V + Ubuntu child"
   GRUB entry (or default Windows Boot Manager entry) that boots the
   orchestrator path. The bare-metal Ubuntu entry remains untouched.
6. **First boot into Windows**: Windows specializes, registers, enables
   Hyper-V, installs rotten-apple-orchestrator service, defines the
   Ubuntu child VM pointing at the existing Ubuntu partition as raw
   block device, configures auto-start. Reboots automatically.
7. **Second boot**: Windows root comes up headlessly, Hyper-V activates,
   Ubuntu child auto-starts, display/input route to Ubuntu. User sees
   Ubuntu desktop login.

The user's interactive obligations during a Hyper-V lift are: confirm
the partition shrink, confirm the OEM key recovered or supply one,
confirm the boot loader change. Everything else is automated.

### Reversibility

Both bootstrappers must implement clean `unlift`:

- **Xen**: restore `/etc/default/grub` from backup, regenerate
  `grub.cfg`, optionally `apt remove --purge` Xen packages.
- **Hyper-V**: remove the Windows partition (after user confirmation),
  remove Windows Boot Manager entries, restore the Ubuntu partition's
  original size, regenerate the boot menu. The bare-metal Ubuntu entry
  was never touched and remains the user's safety net throughout.

## Component layout

```
rotten-apple/
├── rotten-apple                   # CLI entry (current Python implementation)
├── orchestrator/                  # daemon, hypervisor-agnostic core
│   ├── manifest_schema.{py,rs}    # profile dataclass / struct
│   ├── policy.{py,rs}             # active-guest detection, resource arbitration
│   ├── ipc.{py,rs}                # CLI ↔ daemon wire protocol over vsock
│   └── state.{py,rs}              # persistent state, snapshots
├── backends/
│   ├── xen/                       # XenBackend impl: libxl bindings
│   ├── hyperv/                    # HyperVBackend impl: WMI / hcsshim
│   └── trait.{py,rs}              # the abstract base class / trait
├── bootstrappers/
│   ├── xen/                       # current `rotten-apple lift` logic
│   └── hyperv/                    # MSDM extract + wimapply + unattend
├── manifests/
│   ├── ubuntu-desktop.example.toml
│   ├── icloud-keychain.example.toml
│   └── ms-store-host.example.toml  # (later)
├── design/
│   ├── architecture.md            # this file
│   └── backend-trait.md           # the adapter contract
└── test_lifter.py                 # current unit tests
```

The Python lifter we already shipped (`rotten-apple` script, v0.0.3)
becomes the XenBootstrapper implementation in this layout. No code is
thrown away; it gets refactored under the trait.

## Capability matrix (committed)

| Feature                                 | Xen backend | Hyper-V backend |
|-----------------------------------------|:-----------:|:---------------:|
| Non-destructive install                 |     ✓       |       ✓ (additive) |
| Daily-driver Linux experience           |     ✓       |       ✓        |
| Microsoft Store apps (via guest)        |     ✓       |       ✓        |
| iCloud Keychain appliance               |     ✓       |       ✓        |
| Hackintosh guest (OpenCore)             |     ✓       |       ✓        |
| Microsoft 365 / Office sign-in          |     ✓       |       ✓        |
| Windows Hello for Business              |     ✗       |       ✓        |
| Hardware-sealed BitLocker (Windows)     |     ✗       |       ✓        |
| Intune managed-device compliance        |     ✗       |       ✓        |
| PlayReady SL3000 (4K DRM)               |     ✗       |       ✓        |
| Vanguard / EAC / BattlEye anti-cheat    |     ✗       |       ?        |
| Apple Silicon-gated services            |     ✗       |       ✗        |
| Windows licence required                |    none     |     OEM key    |
| Disk overhead                           |   ~200 MB   |   ~30 GB       |

The `?` for Vanguard on Hyper-V reflects that the architecture is
structurally legitimate (Windows IS the root partition) but Riot's
specific detection heuristics for the case "Windows root with active
Linux child partition" are unverified by us.

## Wire protocol (CLI ↔ daemon)

The CLI runs in the user-active guest (typically Ubuntu). The daemon
runs in the substrate (Linux dom0 for Xen, Windows root for Hyper-V).
They communicate over **vsock** (Xen has an emulated vsock; Hyper-V has
native vsock-equivalent via vmbus); identical line protocol, JSON
messages framed by a 4-byte length prefix.

Methods in v0.1:

```
list                         → [{ name, state, mem_mb, vcpus, uptime_s }]
spawn <profile>              → { handle }
suspend <handle>             → { ok }
resume <handle>              → { ok }
stop <handle> [--force]      → { ok }
status <handle>              → { detailed status }
host_info                    → { backend, capabilities, dom0_mem, ... }
```

Future methods: `attach_usb`, `pin_vcpus`, `change_active_desktop`,
`subscribe_events`.

## Versioning

- `manifest_schema_version` is independent of `rotten-apple` version.
- A v0.x daemon must accept any v0.y manifest where y ≤ x.
- Backend trait version is independent of both; backends declare which
  daemon protocol versions they support.

## Non-goals (v0.x — v1.0)

- Cloud-hosted control plane / phone-home anything
- Multi-machine clustering
- Live migration between backends
- An app store / profile registry beyond curated examples in this repo
- Building a clean-room hypervisor

These may become goals in v2+. They are explicitly out of scope today
to keep the surface tractable.
