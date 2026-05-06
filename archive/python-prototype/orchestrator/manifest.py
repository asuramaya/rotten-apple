"""
manifest — rotten-apple profile manifest schema.

Mirrors the TOML schema in `manifests/*.example.toml` and the contract
in `design/architecture.md`. Hypervisor-agnostic: one Profile instance
is consumed by every backend.

Usage
-----
    from orchestrator.manifest import Profile, BackendCapabilities

    p = Profile.load("manifests/ubuntu-desktop.example.toml")
    caps = BackendCapabilities(backend_name="xen", supports_balloon=True, ...)
    problems = p.validate_against(caps)
    if problems:
        for line in problems: print(line)
    else:
        # Profile is satisfiable on this backend; hand it to the daemon.
        ...

Run as a script to validate a manifest against a fake-good backend:
    python3 orchestrator/manifest.py manifests/ubuntu-desktop.example.toml
"""

from __future__ import annotations

import re
import sys
import tomllib
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Optional

SCHEMA_VERSION = "1"


# ---------------------------------------------------------------------------
# Primitive parsers
# ---------------------------------------------------------------------------

_SIZE_UNITS = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30, "T": 1 << 40}
_SIZE_RE = re.compile(r"^\s*(\d+(?:\.\d+)?)\s*([KMGT])i?B?\s*$", re.IGNORECASE)


def parse_size_bytes(spec: str | int) -> int:
    """'56G' / '256M' / '1.5G' → bytes (binary multipliers).

    Plain ints pass through. Bytes are returned, never KB/MB/GB — the
    caller does any pretty-printing it needs.
    """
    if isinstance(spec, int):
        return spec
    if isinstance(spec, str):
        m = _SIZE_RE.match(spec)
        if m:
            return int(float(m.group(1)) * _SIZE_UNITS[m.group(2).upper()])
    raise ValueError(f"unparseable size: {spec!r}")


_DUR_UNITS = {"s": 1, "m": 60, "h": 3600, "d": 86400}
_DUR_RE = re.compile(r"^\s*(\d+)\s*([smhd])\s*$", re.IGNORECASE)


def parse_duration_seconds(spec: str | int | None) -> Optional[int]:
    """'30s' / '5m' / '1h' / 'never' / None → seconds (None = never)."""
    if spec is None or spec == "never":
        return None
    if isinstance(spec, int):
        return spec
    if isinstance(spec, str):
        m = _DUR_RE.match(spec)
        if m:
            return int(m.group(1)) * _DUR_UNITS[m.group(2).lower()]
    raise ValueError(f"unparseable duration: {spec!r}")


# ---------------------------------------------------------------------------
# Enums (only where the small fixed set actually helps dispatch)
# ---------------------------------------------------------------------------

class ProfileKind(Enum):
    DESKTOP = "desktop"
    APPLIANCE = "appliance"
    SERVICE = "service"


class TpmMode(Enum):
    SWTPM = "swtpm"
    HARDWARE_PASSTHROUGH = "hardware-passthrough"
    NONE = "none"


# ---------------------------------------------------------------------------
# Section dataclasses
# ---------------------------------------------------------------------------

@dataclass
class StorageSpec:
    kind: str                       # "passthrough" | "qcow2" | "raw"
    source: Optional[str] = None    # for passthrough: "/dev/nvme0n1p2"
    path: Optional[str] = None      # for qcow2: "/var/lib/.../foo.qcow2"
    mode: str = "rw-exclusive"


@dataclass
class StorageProfile:
    root: StorageSpec
    extra_disks: list[StorageSpec] = field(default_factory=list)


@dataclass
class NetworkInterface:
    name: str = "primary"
    mac: str = "auto"
    egress: list[str] | str = "any"


@dataclass
class NetworkProfile:
    mode: str = "bridge"            # "bridge" | "nat" | "isolated" | "tor"
    interfaces: list[NetworkInterface] = field(default_factory=list)


@dataclass
class GpuProfile:
    mode: str = "none"              # "passthrough" | "paravirt" | "none"
    device: Optional[str] = None    # PCI BDF string, e.g. "0000:00:02.0"
    fallback: Optional[str] = None  # "paravirt" | None


@dataclass
class AudioProfile:
    mode: str = "none"              # "virtio-snd" | "passthrough" | "none"
    default_sink: Optional[str] = None


@dataclass
class InputProfile:
    keyboard: str = "follow_focus"
    mouse: str = "follow_focus"


@dataclass
class UsbRoute:
    vendor: str
    product: str
    route: str


@dataclass
class UsbProfile:
    mode: str = "policy"            # "policy" | "denied" | "passthrough"
    default_route: str = "follow_focus"
    explicit_routes: list[UsbRoute] = field(default_factory=list)


@dataclass
class TpmProfile:
    mode: TpmMode = TpmMode.SWTPM


@dataclass
class ResourceProfile:
    memory_active_bytes: int
    memory_idle_bytes: int
    memory_minimum_bytes: int
    vcpus_active: int
    vcpus_idle: int
    vcpus_minimum: int
    prefer_p_cores: bool = True
    idle_on_e_cores: bool = True


@dataclass
class AutostartProfile:
    enabled: bool = False
    delay_after_boot_seconds: Optional[int] = None
    suspend_after_idle_seconds: Optional[int] = None  # None = never


@dataclass
class TriggerProfile:
    type: str
    manifest_name: Optional[str] = None
    browsers: list[str] = field(default_factory=list)
    extra: dict[str, Any] = field(default_factory=dict)  # type-specific extras


@dataclass
class IntegrationSocket:
    kind: str                       # "vsock"
    port: int
    role: str


@dataclass
class IntegrationProfile:
    sockets: list[IntegrationSocket] = field(default_factory=list)
    files: list[str] = field(default_factory=list)
    clipboard: bool = False


@dataclass
class OrchestrationProfile:
    priority: str = "background"    # "primary" | "secondary" | "background"
    exclusive_resources: list[str] = field(default_factory=list)


@dataclass
class TrustProfile:
    documented_capabilities: list[str] = field(default_factory=list)
    documented_limitations: list[str] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Profile (top-level)
# ---------------------------------------------------------------------------

@dataclass
class Profile:
    name: str
    kind: ProfileKind
    description: str
    schema_version: str
    license_tier: str
    attestation_required: bool

    resources: ResourceProfile
    storage: StorageProfile
    network: NetworkProfile
    gpu: GpuProfile
    audio: AudioProfile
    input: InputProfile
    usb: UsbProfile
    tpm: TpmProfile
    autostart: AutostartProfile
    trigger: Optional[TriggerProfile]
    integration: IntegrationProfile
    orchestration: OrchestrationProfile
    trust: TrustProfile

    # -- loaders -----------------------------------------------------------

    @classmethod
    def load(cls, path: Path | str) -> "Profile":
        with Path(path).open("rb") as f:
            data = tomllib.load(f)
        return cls.from_dict(data)

    @classmethod
    def from_dict(cls, data: dict) -> "Profile":
        p = data["profile"]
        meta = data.get("meta", {})
        r = data["resources"]
        s = data["storage"]
        net = data.get("network", {})
        g = data.get("gpu", {})
        a = data.get("audio", {})
        inp = data.get("input", {})
        u = data.get("usb", {})
        t = data.get("tpm", {})
        au = data.get("autostart", {})
        ig = data.get("integration", {})
        o = data.get("orchestration", {})
        tr = data.get("trust", {})

        resources = ResourceProfile(
            memory_active_bytes=parse_size_bytes(r["memory_active"]),
            memory_idle_bytes=parse_size_bytes(r["memory_idle"]),
            memory_minimum_bytes=parse_size_bytes(r["memory_minimum"]),
            vcpus_active=int(r["vcpus_active"]),
            vcpus_idle=int(r["vcpus_idle"]),
            vcpus_minimum=int(r["vcpus_minimum"]),
            prefer_p_cores=bool(r.get("prefer_p_cores", True)),
            idle_on_e_cores=bool(r.get("idle_on_e_cores", True)),
        )

        storage = StorageProfile(
            root=StorageSpec(**s["root"]),
            extra_disks=[StorageSpec(**d) for d in s.get("extra_disks", [])],
        )

        network = NetworkProfile(
            mode=net.get("mode", "bridge"),
            interfaces=[NetworkInterface(**i) for i in net.get("interfaces", [])],
        )

        gpu = GpuProfile(
            mode=g.get("mode", "none"),
            device=g.get("device"),
            fallback=g.get("fallback"),
        )

        audio = AudioProfile(
            mode=a.get("mode", "none"),
            default_sink=a.get("default_sink"),
        )

        input_p = InputProfile(
            keyboard=inp.get("keyboard", "follow_focus"),
            mouse=inp.get("mouse", "follow_focus"),
        )

        usb = UsbProfile(
            mode=u.get("mode", "policy"),
            default_route=u.get("default_route", "follow_focus"),
            explicit_routes=[UsbRoute(**rr) for rr in u.get("explicit_routes", [])],
        )

        tpm = TpmProfile(mode=TpmMode(t.get("mode", "swtpm")))

        autostart = AutostartProfile(
            enabled=bool(au.get("enabled", False)),
            delay_after_boot_seconds=parse_duration_seconds(au.get("delay_after_boot")),
            suspend_after_idle_seconds=parse_duration_seconds(au.get("suspend_after_idle")),
        )

        trigger = None
        if "trigger" in data:
            td = dict(data["trigger"])
            ttype = td.pop("type")
            mname = td.pop("manifest_name", None)
            browsers = td.pop("browsers", [])
            trigger = TriggerProfile(
                type=ttype, manifest_name=mname, browsers=browsers, extra=td,
            )

        integration = IntegrationProfile(
            sockets=[IntegrationSocket(**ss) for ss in ig.get("sockets", [])],
            files=ig.get("files", []),
            clipboard=bool(ig.get("clipboard", False)),
        )

        orchestration = OrchestrationProfile(
            priority=o.get("priority", "background"),
            exclusive_resources=o.get("exclusive_resources", []),
        )

        trust = TrustProfile(
            documented_capabilities=tr.get("documented_capabilities", []),
            documented_limitations=tr.get("documented_limitations", []),
        )

        return cls(
            name=p["name"],
            kind=ProfileKind(p["type"]),
            description=p.get("description", ""),
            schema_version=p.get("schema_version", SCHEMA_VERSION),
            license_tier=meta.get("license", "personal"),
            attestation_required=bool(meta.get("attestation_required", False)),
            resources=resources,
            storage=storage,
            network=network,
            gpu=gpu,
            audio=audio,
            input=input_p,
            usb=usb,
            tpm=tpm,
            autostart=autostart,
            trigger=trigger,
            integration=integration,
            orchestration=orchestration,
            trust=trust,
        )

    # -- validation --------------------------------------------------------

    def validate_against(self, caps: "BackendCapabilities") -> list[str]:
        """Human-readable mismatches between this profile and a backend.

        Empty list = profile is satisfiable on this backend. Non-empty
        list of strings = the backend cannot honour this profile; show
        them to the user / refuse to spawn.
        """
        problems: list[str] = []

        # ---- internal consistency (independent of backend) ----
        rp = self.resources
        if rp.memory_minimum_bytes > rp.memory_idle_bytes:
            problems.append(
                f"resources: memory_minimum ({rp.memory_minimum_bytes} B) "
                f"exceeds memory_idle ({rp.memory_idle_bytes} B)"
            )
        if rp.memory_idle_bytes > rp.memory_active_bytes:
            problems.append(
                f"resources: memory_idle ({rp.memory_idle_bytes} B) "
                f"exceeds memory_active ({rp.memory_active_bytes} B)"
            )
        if rp.vcpus_minimum > rp.vcpus_idle:
            problems.append("resources: vcpus_minimum exceeds vcpus_idle")
        if rp.vcpus_idle > rp.vcpus_active:
            problems.append("resources: vcpus_idle exceeds vcpus_active")

        # ---- backend capability checks ----
        if (self.tpm.mode == TpmMode.HARDWARE_PASSTHROUGH
                and not caps.supports_hardware_tpm_passthrough):
            problems.append(
                f"tpm.mode=hardware-passthrough requires backend with "
                f"hardware-TPM passthrough; {caps.backend_name} does not."
            )

        if self.gpu.mode == "passthrough" and not caps.supports_pci_passthrough_at_boot:
            problems.append(
                f"gpu.mode=passthrough requires PCI passthrough; "
                f"{caps.backend_name} does not advertise it."
            )

        if (self.attestation_required
                and not caps.supports_hyperv_compatible_attestation):
            problems.append(
                f"attestation_required=true: {caps.backend_name} does not "
                f"provide hyperv-compatible attestation chain."
            )

        if (self.autostart.suspend_after_idle_seconds is not None
                and not caps.supports_suspend_resume):
            problems.append(
                f"autostart.suspend_after_idle requires suspend/resume; "
                f"{caps.backend_name} does not support it."
            )

        if (self.usb.mode == "policy"
                and self.usb.explicit_routes
                and not caps.supports_usb_passthrough):
            problems.append(
                f"usb explicit_routes require USB passthrough; "
                f"{caps.backend_name} does not support it."
            )

        if self.tpm.mode == TpmMode.SWTPM and not caps.supports_swtpm:
            problems.append(
                f"tpm.mode=swtpm requires swtpm support; "
                f"{caps.backend_name} does not."
            )

        return problems


# ---------------------------------------------------------------------------
# BackendCapabilities — mirror of design/backend-trait.md
# ---------------------------------------------------------------------------

@dataclass
class BackendCapabilities:
    backend_name: str
    supports_balloon: bool = False
    supports_hot_pci_passthrough: bool = False
    supports_pci_passthrough_at_boot: bool = False
    supports_usb_passthrough: bool = False
    supports_swtpm: bool = False
    supports_hardware_tpm_passthrough: bool = False
    supports_hyperv_compatible_attestation: bool = False
    supports_live_migration: bool = False
    supports_suspend_resume: bool = False
    max_guests: Optional[int] = None


# ---------------------------------------------------------------------------
# CLI: `python3 -m orchestrator.manifest <path>` validates a manifest
# ---------------------------------------------------------------------------

def _fake_xen_caps() -> BackendCapabilities:
    return BackendCapabilities(
        backend_name="xen",
        supports_balloon=True,
        supports_hot_pci_passthrough=True,
        supports_pci_passthrough_at_boot=True,
        supports_usb_passthrough=True,
        supports_swtpm=True,
        supports_hardware_tpm_passthrough=False,
        supports_hyperv_compatible_attestation=False,
        supports_suspend_resume=True,
    )


def _fake_hyperv_caps() -> BackendCapabilities:
    return BackendCapabilities(
        backend_name="hyperv",
        supports_balloon=True,
        supports_hot_pci_passthrough=False,
        supports_pci_passthrough_at_boot=True,
        supports_usb_passthrough=True,
        supports_swtpm=True,
        supports_hardware_tpm_passthrough=False,
        supports_hyperv_compatible_attestation=True,
        supports_suspend_resume=True,
    )


def _main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(f"usage: {argv[0]} <manifest.toml>", file=sys.stderr)
        return 2
    path = argv[1]
    try:
        p = Profile.load(path)
    except Exception as e:
        print(f"load error: {e}", file=sys.stderr)
        return 1

    def _fmt(b: int) -> str:
        if b >= 1 << 30: return f"{b / (1 << 30):.1f}G"
        if b >= 1 << 20: return f"{b / (1 << 20):.0f}M"
        return f"{b}B"

    print(f"loaded: {p.name}  ({p.kind.value})")
    print(f"  description:  {p.description}")
    print(f"  memory:       active={_fmt(p.resources.memory_active_bytes)} "
          f"idle={_fmt(p.resources.memory_idle_bytes)} "
          f"min={_fmt(p.resources.memory_minimum_bytes)}")
    print(f"  vcpus:        active={p.resources.vcpus_active} "
          f"idle={p.resources.vcpus_idle} min={p.resources.vcpus_minimum}")
    print(f"  storage.root: {p.storage.root.kind} -> "
          f"{p.storage.root.source or p.storage.root.path}")
    print(f"  gpu:          {p.gpu.mode}"
          + (f" ({p.gpu.device})" if p.gpu.device else ""))
    print(f"  tpm:          {p.tpm.mode.value}")
    print(f"  attestation:  required={p.attestation_required}")
    print()

    for caps in (_fake_xen_caps(), _fake_hyperv_caps()):
        problems = p.validate_against(caps)
        verdict = "OK" if not problems else f"{len(problems)} issue(s)"
        print(f"  {caps.backend_name:8s} → {verdict}")
        for line in problems:
            print(f"      - {line}")
    return 0


if __name__ == "__main__":
    sys.exit(_main(sys.argv))
