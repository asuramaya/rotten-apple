#!/usr/bin/env python3
"""
test_lifter — unit tests for the pure functions in rotten-apple.

Exercises GRUB defaults patching, menuentry parsing, and managed-block
strip/restore logic. Runs entirely in memory; touches no real GRUB files,
no apt, no boot anything. Safe to run on the host.

  $ python3 test_lifter.py
"""

from __future__ import annotations

import importlib.util
import importlib.machinery
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "rotten-apple"

# The script has no .py suffix, so importlib needs an explicit SourceFileLoader.
loader = importlib.machinery.SourceFileLoader("rotten_apple", str(SCRIPT))
spec = importlib.util.spec_from_loader("rotten_apple", loader)
ra = importlib.util.module_from_spec(spec)
sys.modules["rotten_apple"] = ra
loader.exec_module(ra)


SAMPLE_GRUB_DEFAULTS = """\
# /etc/default/grub
GRUB_DEFAULT=0
GRUB_TIMEOUT_STYLE=hidden
GRUB_TIMEOUT=0
GRUB_DISTRIBUTOR=`( . /etc/os-release && echo ${NAME} )`
GRUB_CMDLINE_LINUX_DEFAULT="quiet splash iommu=pt intel_iommu=on"
GRUB_CMDLINE_LINUX=""
"""

# A miniature grub.cfg that mimics the structure update-grub produces on
# Ubuntu after Xen is installed.
SAMPLE_GRUB_CFG = """\
# auto-generated
menuentry 'Ubuntu' --class ubuntu --class gnu-linux --class gnu --class os {
    echo loading...
}
submenu 'Advanced options for Ubuntu' {
    menuentry 'Ubuntu, with Linux 6.17.0-22-generic' { echo a }
    menuentry 'Ubuntu, with Linux 6.17.0-22-generic (recovery mode)' { echo b }
}
menuentry 'Ubuntu GNU/Linux, with Xen hypervisor' --class xen { echo xen }
menuentry 'Ubuntu GNU/Linux, with Xen hypervisor (recovery mode)' { echo xen-rec }
"""

# Variant where the Xen entry is nested in a submenu — exercises path tracking.
NESTED_GRUB_CFG = """\
menuentry 'Ubuntu' { echo top }
submenu 'Advanced options for Ubuntu' {
    menuentry 'Ubuntu, with Linux 6.17.0' { echo a }
    submenu 'Xen hypervisor variants' {
        menuentry 'Ubuntu, with Xen hypervisor' { echo nested-xen }
        menuentry 'Ubuntu, with Xen hypervisor (recovery mode)' { echo rec }
    }
}
"""

# Recovery-only Xen — should not match.
RECOVERY_ONLY_CFG = """\
menuentry 'Ubuntu' { echo top }
menuentry 'Ubuntu, with Xen hypervisor (recovery mode)' { echo rec }
"""


class TestGrubCfgParsing(unittest.TestCase):
    def test_lists_all_menuentries(self):
        titles = ra.list_menuentries(SAMPLE_GRUB_CFG)
        self.assertIn("Ubuntu", titles)
        self.assertIn("Ubuntu, with Linux 6.17.0-22-generic", titles)
        self.assertIn("Ubuntu GNU/Linux, with Xen hypervisor", titles)
        self.assertEqual(len(titles), 5)

    def test_finds_xen_path_top_level(self):
        path = ra.find_xen_path(SAMPLE_GRUB_CFG)
        self.assertEqual(path, "Ubuntu GNU/Linux, with Xen hypervisor")

    def test_finds_xen_path_nested_in_submenu(self):
        path = ra.find_xen_path(NESTED_GRUB_CFG)
        self.assertEqual(
            path,
            "Advanced options for Ubuntu>Xen hypervisor variants>"
            "Ubuntu, with Xen hypervisor",
        )

    def test_recovery_only_returns_none(self):
        self.assertIsNone(ra.find_xen_path(RECOVERY_ONLY_CFG))

    def test_returns_none_when_no_xen_entry(self):
        cfg = "menuentry 'Ubuntu' { echo a }\n"
        self.assertIsNone(ra.find_xen_path(cfg))


class TestPatchGrubDefaults(unittest.TestCase):
    def test_appends_managed_block(self):
        out = ra.patch_grub_defaults(
            SAMPLE_GRUB_DEFAULTS,
            xen_cmdline="dom0_mem=8G,max:8G dom0_max_vcpus=8",
            xen_default_path="Ubuntu GNU/Linux, with Xen hypervisor",
        )
        # original text untouched
        self.assertIn('GRUB_CMDLINE_LINUX_DEFAULT="quiet splash iommu=pt intel_iommu=on"', out)
        # managed block present
        self.assertIn(ra.MARKER_BEGIN, out)
        self.assertIn(ra.MARKER_END, out)
        self.assertIn('GRUB_DEFAULT="Ubuntu GNU/Linux, with Xen hypervisor"', out)
        self.assertIn('GRUB_CMDLINE_XEN_DEFAULT="dom0_mem=8G,max:8G dom0_max_vcpus=8"', out)

    def test_idempotent_replace_on_second_apply(self):
        once = ra.patch_grub_defaults(SAMPLE_GRUB_DEFAULTS, "X", "T1")
        twice = ra.patch_grub_defaults(once, "Y", "T2")
        # Only one managed block survives.
        self.assertEqual(twice.count(ra.MARKER_BEGIN), 1)
        self.assertEqual(twice.count(ra.MARKER_END), 1)
        # And it has the new values.
        self.assertIn('GRUB_DEFAULT="T2"', twice)
        self.assertIn('GRUB_CMDLINE_XEN_DEFAULT="Y"', twice)
        self.assertNotIn('"T1"', twice)
        self.assertNotIn('"X"', twice)

    def test_strip_returns_pristine(self):
        patched = ra.patch_grub_defaults(SAMPLE_GRUB_DEFAULTS, "X", "T")
        stripped = ra.strip_managed_block(patched)
        # After stripping, content equals the original (modulo trailing whitespace).
        self.assertEqual(stripped.rstrip("\n"), SAMPLE_GRUB_DEFAULTS.rstrip("\n"))

    def test_strip_when_no_block_is_a_noop(self):
        self.assertEqual(
            ra.strip_managed_block(SAMPLE_GRUB_DEFAULTS),
            SAMPLE_GRUB_DEFAULTS,
        )

    def test_show_menu_adds_timeout_overrides(self):
        out = ra.patch_grub_defaults(
            SAMPLE_GRUB_DEFAULTS, "X", "Title", show_menu=True,
        )
        self.assertIn("GRUB_TIMEOUT_STYLE=menu", out)
        self.assertIn("GRUB_TIMEOUT=5", out)

    def test_no_show_menu_omits_timeout_overrides(self):
        out = ra.patch_grub_defaults(
            SAMPLE_GRUB_DEFAULTS, "X", "Title", show_menu=False,
        )
        self.assertNotIn("GRUB_TIMEOUT_STYLE=menu", out)
        # Original GRUB_TIMEOUT=0 in defaults must remain untouched.
        self.assertIn("GRUB_TIMEOUT=0", out)
        self.assertNotIn("GRUB_TIMEOUT=5", out)

    def test_finalize_round_trip(self):
        """Sequence: lift with show_menu, then finalize → menu hidden again."""
        first = ra.patch_grub_defaults(SAMPLE_GRUB_DEFAULTS, "X", "T", show_menu=True)
        self.assertIn("GRUB_TIMEOUT_STYLE=menu", first)
        finalized = ra.patch_grub_defaults(first, "X", "T", show_menu=False)
        self.assertNotIn("GRUB_TIMEOUT_STYLE=menu", finalized)
        self.assertEqual(finalized.count(ra.MARKER_BEGIN), 1)
        # Final state == directly applying without show_menu.
        direct = ra.patch_grub_defaults(SAMPLE_GRUB_DEFAULTS, "X", "T", show_menu=False)
        self.assertEqual(finalized, direct)


class TestFirstbootTemplate(unittest.TestCase):
    def test_helper_substitutes_paths(self):
        rendered = ra.FIRSTBOOT_HELPER.format(
            rotten_apple_path="/usr/local/bin/rotten-apple",
            desktop_name=ra.FIRSTBOOT_DESKTOP_NAME,
        )
        self.assertIn("/usr/local/bin/rotten-apple verify", rendered)
        self.assertIn(ra.FIRSTBOOT_DESKTOP_NAME, rendered)
        self.assertIn("notify-send", rendered)
        self.assertIn("rm -f", rendered)  # self-cleanup

    def test_desktop_substitutes_helper_path(self):
        rendered = ra.FIRSTBOOT_DESKTOP.format(helper_path="/home/u/x.sh")
        self.assertIn("Exec=/home/u/x.sh", rendered)
        self.assertIn("[Desktop Entry]", rendered)


class TestXenCmdlineBuilder(unittest.TestCase):
    """v0.0.4 introduced the safe-by-default Xen cmdline. These tests pin
    the conservative defaults and confirm the opt-in flags layer cleanly."""

    def test_default_is_safe_for_nested_vms(self):
        cmd = ra.build_xen_cmdline(dom0_mem="2G", dom0_vcpus="2")
        # Conservative defaults: iommu=on (gracefully degrades), no PVH.
        self.assertIn("iommu=on", cmd)
        self.assertNotIn("force", cmd)
        self.assertNotIn("strict", cmd)
        self.assertNotIn("pvh", cmd)
        self.assertIn("dom0_mem=2G,max:2G", cmd)
        self.assertIn("dom0_max_vcpus=2", cmd)

    def test_iommu_force_opt_in(self):
        cmd = ra.build_xen_cmdline(dom0_mem="8G", dom0_vcpus="8",
                                   iommu_force=True)
        self.assertIn("iommu=force,strict", cmd)
        self.assertNotIn("iommu=on ", cmd)   # must not coexist
        self.assertNotIn("pvh", cmd)         # other flags unaffected

    def test_dom0_pvh_opt_in(self):
        cmd = ra.build_xen_cmdline(dom0_mem="8G", dom0_vcpus="8",
                                   dom0_pvh=True)
        self.assertIn("dom0=pvh", cmd)
        self.assertIn("iommu=on", cmd)       # iommu still on default

    def test_both_flags_combine(self):
        cmd = ra.build_xen_cmdline(dom0_mem="8G", dom0_vcpus="8",
                                   iommu_force=True, dom0_pvh=True)
        self.assertIn("iommu=force,strict", cmd)
        self.assertIn("dom0=pvh", cmd)


class TestWizardPhase(unittest.TestCase):
    """The phase router is pure — given (state, running_under_xen, blockers)
    it must return the right phase string. These tests pin the state machine."""

    def _state(self, lifted=False, show_pending=False):
        s = ra.LiftState()
        s.lifted = lifted
        s.show_menu_pending = show_pending
        return s

    def test_blocked_short_circuits(self):
        # blockers always win, regardless of state
        for state in (self._state(), self._state(lifted=True)):
            self.assertEqual(
                ra.wizard_phase(state, running_under_xen=False, blockers=["x86 only"]),
                ra.PHASE_BLOCKED,
            )

    def test_fresh_when_unlifted_and_no_xen(self):
        self.assertEqual(
            ra.wizard_phase(self._state(), False, []),
            ra.PHASE_FRESH,
        )

    def test_pending_finalize_when_xen_and_show_menu(self):
        self.assertEqual(
            ra.wizard_phase(self._state(lifted=True, show_pending=True), True, []),
            ra.PHASE_LIFTED_PENDING_FINAL,
        )

    def test_lifted_done_when_xen_and_finalized(self):
        self.assertEqual(
            ra.wizard_phase(self._state(lifted=True, show_pending=False), True, []),
            ra.PHASE_LIFTED_DONE,
        )

    def test_stuck_when_state_says_lifted_but_not_under_xen(self):
        self.assertEqual(
            ra.wizard_phase(self._state(lifted=True), False, []),
            ra.PHASE_STUCK,
        )

    def test_phase_constants_are_distinct(self):
        all_phases = {
            ra.PHASE_FRESH, ra.PHASE_PENDING_REBOOT,
            ra.PHASE_LIFTED_PENDING_FINAL, ra.PHASE_LIFTED_DONE,
            ra.PHASE_STUCK, ra.PHASE_BLOCKED,
        }
        self.assertEqual(len(all_phases), 6, "phase constants must be distinct")


class TestDetectionShape(unittest.TestCase):
    """Smoke test: detect() must run and return a Detection on the host
    without raising. We only assert structural invariants — values depend
    on the host. This catches regressions in detection wiring."""

    def test_returns_detection_with_required_fields(self):
        d = ra.detect()
        self.assertIsInstance(d, ra.Detection)
        for field in ("distro_id", "distro_version", "is_uefi", "secure_boot",
                      "cpu_count", "mem_total_kb", "kernel", "arch"):
            self.assertTrue(hasattr(d, field), f"missing field {field}")
        self.assertGreater(d.cpu_count, 0)
        self.assertIsInstance(d.warnings, list)
        self.assertIsInstance(d.blockers, list)


if __name__ == "__main__":
    unittest.main(verbosity=2)
