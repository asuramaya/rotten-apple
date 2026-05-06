#!/usr/bin/env python3
"""
runner — nested-VM development harness for rotten-apple.

Provisions an Ubuntu 25.10 cloud image inside a QEMU VM with UEFI,
nested virt, and SSH access, so we can iterate on the lifter and the
orchestrator without touching the host's boot path.

Subcommands
-----------
  provision   create a new run dir with a fresh disk overlay + ssh key
              + cloud-init seed ISO. Does not boot.
  boot        start QEMU for the most recent run (or --run-id X).
              Daemonizes; writes pidfile, monitor.sock, serial.log.
  ssh         interactive SSH into the running VM.
  exec CMD    run a single command inside the VM via SSH.
  reboot      reboot via SSH; wait for SSH to come back.
  kill        SIGTERM the QEMU process.
  snapshot N  internal qemu snapshot named N (live).
  reset N     restore snapshot N (live).
  list        show all runs and their state.
  rm          remove a run dir (must be killed first).
  status      detailed state of one run.

Layout under harness/
    images/                 cached Ubuntu base image (downloaded once)
    runs/<run-id>/          per-run state
        disk.qcow2          overlay backed by base
        seed.iso            cloud-init NoCloud seed
        ssh_key, .pub       per-run SSH keypair
        ovmf-vars.fd        per-run OVMF NVRAM
        pid                 QEMU pidfile
        monitor.sock        QEMU monitor (UNIX socket)
        serial.log          serial console capture
        state.json          run metadata (ports, paths, status)

Run as a script:
    ./runner.py provision
    ./runner.py boot
    ./runner.py ssh
    ./runner.py exec 'lsb_release -a'
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.request
import uuid
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Paths

HARNESS_DIR = Path(__file__).resolve().parent
IMAGES_DIR  = HARNESS_DIR / "images"
RUNS_DIR    = HARNESS_DIR / "runs"

UBUNTU_RELEASE   = "questing"     # 25.10
UBUNTU_IMAGE_URL = (
    f"https://cloud-images.ubuntu.com/{UBUNTU_RELEASE}/current/"
    f"{UBUNTU_RELEASE}-server-cloudimg-amd64.img"
)
UBUNTU_IMAGE     = IMAGES_DIR / f"ubuntu-{UBUNTU_RELEASE}-cloud.img"

# OVMF firmware on Ubuntu 25.10 lives under /usr/share/OVMF/.
OVMF_CODE = Path("/usr/share/OVMF/OVMF_CODE_4M.fd")
OVMF_VARS = Path("/usr/share/OVMF/OVMF_VARS_4M.fd")

# Defaults
DEFAULT_DISK_GB = 20
DEFAULT_RAM     = "4G"
DEFAULT_VCPUS   = 4
SSH_PORT_BASE   = 2222    # first run uses 2222, second 2223, etc.

# ---------------------------------------------------------------------------
# Run state

@dataclass
class Run:
    run_id: str
    created_at: str
    ssh_port: int
    pid: Optional[int] = None
    booted_at: Optional[str] = None
    status: str = "provisioned"   # provisioned | booted | stopped | broken

    @property
    def dir(self) -> Path:           return RUNS_DIR / self.run_id
    @property
    def disk(self) -> Path:          return self.dir / "disk.qcow2"
    @property
    def seed(self) -> Path:          return self.dir / "seed.iso"
    @property
    def ssh_key(self) -> Path:       return self.dir / "ssh_key"
    @property
    def ovmf_vars(self) -> Path:     return self.dir / "ovmf-vars.fd"
    @property
    def pidfile(self) -> Path:       return self.dir / "pid"
    @property
    def monitor(self) -> Path:       return self.dir / "monitor.sock"
    @property
    def serial_log(self) -> Path:    return self.dir / "serial.log"
    @property
    def state_file(self) -> Path:    return self.dir / "state.json"

    def save(self) -> None:
        self.dir.mkdir(parents=True, exist_ok=True)
        self.state_file.write_text(json.dumps(asdict(self), indent=2,
                                              default=lambda p: str(p)) + "\n")

    @classmethod
    def load(cls, run_id: str) -> "Run":
        p = RUNS_DIR / run_id / "state.json"
        if not p.exists():
            raise SystemExit(f"no such run: {run_id}")
        d = json.loads(p.read_text())
        return cls(**d)


def list_runs() -> list[Run]:
    if not RUNS_DIR.exists():
        return []
    runs = []
    for d in sorted(RUNS_DIR.iterdir()):
        if (d / "state.json").exists():
            runs.append(Run.load(d.name))
    return runs


def latest_run() -> Run:
    runs = list_runs()
    if not runs:
        raise SystemExit("no runs; `provision` first")
    return runs[-1]


def resolve_run(run_id: Optional[str]) -> Run:
    return Run.load(run_id) if run_id else latest_run()


# ---------------------------------------------------------------------------
# Subprocess + alive checks

def run(cmd: list[str], *, check: bool = True, capture: bool = False,
        input_: Optional[str] = None) -> subprocess.CompletedProcess:
    res = subprocess.run(
        cmd, check=False, text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        input=input_,
    )
    if check and res.returncode != 0:
        msg = (res.stderr or "").strip() if capture else f"rc={res.returncode}"
        raise SystemExit(f"{cmd[0]} failed: {msg}")
    return res


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except (ProcessLookupError, PermissionError):
        return False


def port_open(host: str, port: int, timeout: float = 1.0) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def wait_port(host: str, port: int, deadline: float, *, label: str = "port") -> None:
    print(f"  waiting for {label} on {host}:{port}...", end="", flush=True)
    while time.time() < deadline:
        if port_open(host, port):
            print(" up")
            return
        print(".", end="", flush=True)
        time.sleep(2)
    print(" TIMEOUT")
    raise SystemExit(f"{label} on {host}:{port} did not come up")


# ---------------------------------------------------------------------------
# Image management

def download_base_image() -> None:
    if UBUNTU_IMAGE.exists():
        return
    IMAGES_DIR.mkdir(parents=True, exist_ok=True)
    print(f"  downloading {UBUNTU_IMAGE_URL}")
    print(f"  → {UBUNTU_IMAGE}")
    tmp = UBUNTU_IMAGE.with_suffix(".part")
    with urllib.request.urlopen(UBUNTU_IMAGE_URL) as resp, tmp.open("wb") as f:
        total = int(resp.headers.get("Content-Length", 0))
        downloaded = 0
        chunk = 1 << 20
        while True:
            buf = resp.read(chunk)
            if not buf:
                break
            f.write(buf)
            downloaded += len(buf)
            if total:
                pct = downloaded * 100 // total
                print(f"\r  {downloaded >> 20} / {total >> 20} MB ({pct}%)",
                      end="", flush=True)
        print()
    tmp.rename(UBUNTU_IMAGE)
    print(f"  cached: {UBUNTU_IMAGE}")


# ---------------------------------------------------------------------------
# Per-run setup

def make_ssh_key(r: Run) -> None:
    if r.ssh_key.exists():
        return
    run([
        "ssh-keygen", "-t", "ed25519", "-N", "", "-q",
        "-f", str(r.ssh_key),
        "-C", f"rotten-apple-harness:{r.run_id}",
    ])


def make_seed_iso(run_obj: Run) -> None:
    """Build a NoCloud cloud-init seed ISO with user-data + meta-data."""
    pubkey = (run_obj.ssh_key.with_suffix(".pub")).read_text().strip()
    user_data = f"""#cloud-config
hostname: rotten-harness
manage_etc_hosts: true
users:
  - name: ubuntu
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - {pubkey}
ssh_pwauth: false
disable_root: true
package_update: false
runcmd:
  - [systemctl, enable, --now, ssh]
"""
    meta_data = f"""instance-id: {run_obj.run_id}
local-hostname: rotten-harness
"""
    seed_src = run_obj.dir / "seed-src"
    seed_src.mkdir(exist_ok=True)
    (seed_src / "user-data").write_text(user_data)
    (seed_src / "meta-data").write_text(meta_data)
    run([
        "xorriso", "-as", "mkisofs",
        "-output", str(run_obj.seed),
        "-volid", "cidata",
        "-joliet", "-rock",
        str(seed_src / "user-data"),
        str(seed_src / "meta-data"),
    ])


def make_disk_overlay(run_obj: Run) -> None:
    """Create a qcow2 overlay backed by the cached base image."""
    if run_obj.disk.exists():
        return
    run([
        "qemu-img", "create",
        "-f", "qcow2",
        "-b", str(UBUNTU_IMAGE),
        "-F", "qcow2",
        str(run_obj.disk),
        f"{DEFAULT_DISK_GB}G",
    ])


def make_ovmf_vars(run_obj: Run) -> None:
    """Per-run copy of OVMF NVRAM (boot order, EFI vars)."""
    if run_obj.ovmf_vars.exists():
        return
    shutil.copy2(OVMF_VARS, run_obj.ovmf_vars)


def allocate_ssh_port() -> int:
    used = {r.ssh_port for r in list_runs()}
    port = SSH_PORT_BASE
    while port in used or port_open("127.0.0.1", port):
        port += 1
    return port


# ---------------------------------------------------------------------------
# QEMU lifecycle

def qemu_command(run_obj: Run, *, ram: str, vcpus: int) -> list[str]:
    """
    QEMU command for the inner VM.

    Notes on the device shape:
    - Disks and the NIC are wired explicitly (not via `if=virtio`) so we can
      pass `iommu_platform=on` to each virtio device. This makes the device
      advertise VIRTIO_F_ACCESS_PLATFORM, which is required when the guest
      itself is going to run Xen and host PV dom0 — otherwise dom0's virtio
      drivers refuse the device with "device must provide
      VIRTIO_F_ACCESS_PLATFORM" and the system is unbootable.
    - `intel-iommu` is added because `iommu_platform=on` only takes effect
      when the machine actually has an emulated IOMMU. Without it, the
      feature flag is silently dropped and dom0 still rejects the devices.
    - These choices are harmless for non-Xen guests — bare-metal Ubuntu
      boots normally with this same configuration.
    """
    return [
        "qemu-system-x86_64",
        "-name", f"rotten-harness:{run_obj.run_id}",
        # `kernel-irqchip=split` is required by intel-iommu in Q35.
        "-machine", "q35,accel=kvm,kernel-irqchip=split",
        "-cpu", "host",
        "-smp", str(vcpus),
        "-m", ram,
        # Emulated IOMMU; required for iommu_platform=on virtio devices.
        "-device", "intel-iommu,intremap=on,caching-mode=on",
        # UEFI firmware (split CODE/VARS).
        "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}",
        "-drive", f"if=pflash,format=raw,file={run_obj.ovmf_vars}",
        # Primary disk: explicit drive + device so we can set iommu_platform.
        "-drive", f"id=disk0,if=none,format=qcow2,file={run_obj.disk}",
        "-device", "virtio-blk-pci,drive=disk0,iommu_platform=on,disable-legacy=on",
        # Cloud-init seed: same shape, read-only.
        "-drive", f"id=cidata,if=none,format=raw,file={run_obj.seed},readonly=on",
        "-device", "virtio-blk-pci,drive=cidata,iommu_platform=on,disable-legacy=on",
        # User-mode networking with SSH port forward.
        "-netdev", f"user,id=net0,hostfwd=tcp:127.0.0.1:{run_obj.ssh_port}-:22",
        "-device", "virtio-net-pci,netdev=net0,iommu_platform=on,disable-legacy=on",
        # Monitor + serial.
        "-monitor", f"unix:{run_obj.monitor},server,nowait",
        "-serial", f"file:{run_obj.serial_log}",
        # Headless, daemonized.
        "-display", "none",
        "-pidfile", str(run_obj.pidfile),
        "-daemonize",
    ]


def qemu_monitor(run_obj: Run, command: str) -> str:
    """Send a command to the running QEMU's monitor socket; return reply."""
    if not run_obj.monitor.exists():
        raise SystemExit(f"monitor socket missing: {run_obj.monitor}")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(5)
        s.connect(str(run_obj.monitor))
        # Read banner + first prompt
        time.sleep(0.1)
        s.recv(8192)
        s.sendall((command + "\n").encode())
        time.sleep(0.2)
        return s.recv(8192).decode(errors="replace")


def ssh_args(run_obj: Run) -> list[str]:
    return [
        "ssh",
        "-i", str(run_obj.ssh_key),
        "-p", str(run_obj.ssh_port),
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR",
        "-o", "ConnectTimeout=5",
        "ubuntu@127.0.0.1",
    ]


# ---------------------------------------------------------------------------
# Commands

def cmd_provision(args) -> int:
    download_base_image()
    run_id = args.run_id or time.strftime("%Y%m%dT%H%M%S")
    if (RUNS_DIR / run_id).exists():
        raise SystemExit(f"run already exists: {run_id}")

    r = Run(
        run_id=run_id,
        created_at=time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        ssh_port=allocate_ssh_port(),
        status="provisioned",
    )
    r.dir.mkdir(parents=True, exist_ok=True)
    print(f"  run: {r.run_id}")
    print(f"  dir: {r.dir}")
    make_ssh_key(r)
    make_seed_iso(r)
    make_disk_overlay(r)
    make_ovmf_vars(r)
    r.save()
    print(f"  ssh port: {r.ssh_port}")
    print(f"  ready. `boot` to start.")
    return 0


def cmd_boot(args) -> int:
    r = resolve_run(args.run_id)
    if r.pid and pid_alive(r.pid):
        print(f"  already booted (pid {r.pid})")
        return 0
    cmd = qemu_command(r, ram=args.ram, vcpus=args.vcpus)
    run(cmd)
    # qemu daemonized; pidfile written
    pid = int(r.pidfile.read_text().strip())
    r.pid = pid
    r.booted_at = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    r.status = "booted"
    r.save()
    print(f"  qemu pid: {pid}")
    print(f"  serial log: {r.serial_log}")
    print(f"  monitor:    {r.monitor}")
    deadline = time.time() + args.timeout
    wait_port("127.0.0.1", r.ssh_port, deadline, label="ssh")
    # cloud-init may take a bit more after ssh comes up
    print("  waiting for cloud-init to settle...", end="", flush=True)
    for _ in range(30):
        res = subprocess.run(
            ssh_args(r) + ["cloud-init", "status", "--wait"],
            capture_output=True, text=True, timeout=120,
        )
        if res.returncode == 0 and "done" in res.stdout:
            print(" done")
            return 0
        print(".", end="", flush=True)
        time.sleep(2)
    print(" (still settling; you can ssh in anyway)")
    return 0


def cmd_ssh(args) -> int:
    r = resolve_run(args.run_id)
    if not (r.pid and pid_alive(r.pid)):
        raise SystemExit("not booted; `boot` first")
    cmd = ssh_args(r) + (args.cmd or [])
    os.execvp("ssh", cmd)


def cmd_exec(args) -> int:
    r = resolve_run(args.run_id)
    if not (r.pid and pid_alive(r.pid)):
        raise SystemExit("not booted; `boot` first")
    res = subprocess.run(ssh_args(r) + [args.cmd], text=True)
    return res.returncode


def cmd_reboot(args) -> int:
    r = resolve_run(args.run_id)
    if not (r.pid and pid_alive(r.pid)):
        raise SystemExit("not booted; `boot` first")
    print("  reboot via ssh")
    subprocess.run(ssh_args(r) + ["sudo reboot"], timeout=15)
    print("  waiting for ssh to drop...", end="", flush=True)
    for _ in range(30):
        if not port_open("127.0.0.1", r.ssh_port):
            print(" gone")
            break
        print(".", end="", flush=True)
        time.sleep(1)
    deadline = time.time() + args.timeout
    wait_port("127.0.0.1", r.ssh_port, deadline, label="ssh (after reboot)")
    return 0


def cmd_kill(args) -> int:
    r = resolve_run(args.run_id)
    if not (r.pid and pid_alive(r.pid)):
        print("  not running")
    else:
        print(f"  SIGTERM {r.pid}")
        os.kill(r.pid, signal.SIGTERM)
        for _ in range(20):
            if not pid_alive(r.pid):
                break
            time.sleep(0.25)
        if pid_alive(r.pid):
            print(f"  SIGKILL {r.pid}")
            os.kill(r.pid, signal.SIGKILL)
    r.pid = None
    r.status = "stopped"
    r.save()
    return 0


def cmd_snapshot(args) -> int:
    r = resolve_run(args.run_id)
    if not (r.pid and pid_alive(r.pid)):
        raise SystemExit("not booted")
    out = qemu_monitor(r, f"savevm {args.name}")
    print(out)
    return 0


def cmd_reset(args) -> int:
    r = resolve_run(args.run_id)
    if not (r.pid and pid_alive(r.pid)):
        raise SystemExit("not booted")
    out = qemu_monitor(r, f"loadvm {args.name}")
    print(out)
    return 0


def cmd_list(args) -> int:
    runs = list_runs()
    if not runs:
        print("  no runs")
        return 0
    print(f"  {'RUN':24s}  {'PORT':>5s}  {'STATUS':10s}  PID")
    for r in runs:
        alive = "alive" if (r.pid and pid_alive(r.pid)) else "—"
        pid = str(r.pid) if r.pid else "—"
        print(f"  {r.run_id:24s}  {r.ssh_port:>5d}  {r.status:10s}  {pid} ({alive})")
    return 0


def cmd_status(args) -> int:
    r = resolve_run(args.run_id)
    print(f"  run id:     {r.run_id}")
    print(f"  created:    {r.created_at}")
    print(f"  status:     {r.status}")
    print(f"  ssh port:   {r.ssh_port}")
    print(f"  pid:        {r.pid} ({'alive' if r.pid and pid_alive(r.pid) else 'dead'})")
    print(f"  dir:        {r.dir}")
    print(f"  serial log: {r.serial_log}")
    return 0


def cmd_rm(args) -> int:
    r = resolve_run(args.run_id)
    if r.pid and pid_alive(r.pid):
        raise SystemExit("still running; `kill` first")
    shutil.rmtree(r.dir)
    print(f"  removed {r.dir}")
    return 0


# ---------------------------------------------------------------------------
# CLI

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="runner.py", description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="command", required=True)

    sp = sub.add_parser("provision", help="create a new run; do not boot")
    sp.add_argument("--run-id", help="explicit run id (default: timestamp)")

    sp = sub.add_parser("boot", help="start QEMU for the run")
    sp.add_argument("--run-id", help="explicit run id (default: latest)")
    sp.add_argument("--ram", default=DEFAULT_RAM)
    sp.add_argument("--vcpus", type=int, default=DEFAULT_VCPUS)
    sp.add_argument("--timeout", type=int, default=180,
                    help="seconds to wait for ssh after boot")

    sp = sub.add_parser("ssh", help="interactive SSH into the run")
    sp.add_argument("--run-id")
    sp.add_argument("cmd", nargs=argparse.REMAINDER, help="optional command")

    sp = sub.add_parser("exec", help="run a single command via SSH")
    sp.add_argument("--run-id")
    sp.add_argument("cmd", help="command (single arg, will be shell-evaluated)")

    sp = sub.add_parser("reboot", help="reboot via SSH; wait for SSH back")
    sp.add_argument("--run-id")
    sp.add_argument("--timeout", type=int, default=180)

    sp = sub.add_parser("kill", help="SIGTERM the QEMU process")
    sp.add_argument("--run-id")

    sp = sub.add_parser("snapshot", help="qemu live snapshot")
    sp.add_argument("name")
    sp.add_argument("--run-id")

    sp = sub.add_parser("reset", help="restore qemu live snapshot")
    sp.add_argument("name")
    sp.add_argument("--run-id")

    sub.add_parser("list", help="show all runs")

    sp = sub.add_parser("status", help="show one run's state")
    sp.add_argument("--run-id")

    sp = sub.add_parser("rm", help="delete a run dir (must be killed)")
    sp.add_argument("--run-id")

    return p


def main(argv: Optional[list[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    match args.command:
        case "provision": return cmd_provision(args)
        case "boot":      return cmd_boot(args)
        case "ssh":       return cmd_ssh(args)
        case "exec":      return cmd_exec(args)
        case "reboot":    return cmd_reboot(args)
        case "kill":      return cmd_kill(args)
        case "snapshot":  return cmd_snapshot(args)
        case "reset":     return cmd_reset(args)
        case "list":      return cmd_list(args)
        case "status":    return cmd_status(args)
        case "rm":        return cmd_rm(args)
    return 2


if __name__ == "__main__":
    sys.exit(main())
