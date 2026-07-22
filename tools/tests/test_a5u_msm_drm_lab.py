from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "a5u_msm_drm_lab.py"
SPEC = importlib.util.spec_from_file_location("a5u_msm_drm_lab", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
lab = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = lab
SPEC.loader.exec_module(lab)


def good_inventory(driver: str = "msm-kms") -> str:
    return "\n".join(
        (
            lab.INVENTORY_HEADER,
            "cmdline\t/proc/cmdline\tmsm.skip_gpu=1 pocketboot.drm_page_flips=16",
            f"dri-name\t/sys/kernel/debug/dri/0/name\t{driver} dev=1a01000.display-controller unique=1a01000.display-controller",
            "card-driver\t/sys/class/drm/card0\tmsm_mdp",
            "connector-status\t/sys/class/drm/card0-DSI-1\tconnected",
            "connector-enabled\t/sys/class/drm/card0-DSI-1\tenabled",
            "connector-mode\t/sys/class/drm/card0-DSI-1\t720x1280",
            "",
        )
    )


def good_dmesg() -> str:
    lines = [
        "INFO pocketboot::ui: POCKETBOOT_DRM_READY path=/dev/dri/card0 connector=DSI-1 width=720 height=1280",
        "INFO pocketboot::ui: POCKETBOOT_DRM_PAGE_FLIP_TEST_START requested=16",
    ]
    lines.extend(
        f"INFO pocketboot::ui: POCKETBOOT_DRM_PAGE_FLIP sequence={sequence} remaining={16 - sequence}"
        for sequence in range(1, 17)
    )
    lines.append(
        "INFO pocketboot::ui: POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested=16 completed=16"
    )
    return "\n".join(lines) + "\n"


class A5uMsmDrmLabTests(unittest.TestCase):
    def test_getvar_timeout_can_be_retried_by_boot_waiter(self) -> None:
        log = mock.Mock()
        runner = lab.CommandRunner(log)
        with mock.patch.object(
            lab.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired(["fastboot"], 1.0, output="waiting"),
        ):
            returncode, output = runner.run(["fastboot"], 1.0)

        self.assertEqual(returncode, 124)
        self.assertIn("timed out", output)

    def test_fastboot_surface_is_explicit_and_has_no_storage_mutations(self) -> None:
        client = lab.FastbootClient("fastboot", "exact-a5-serial")
        commands = (
            client.getvar_command("compatible"),
            client.boot_command(Path("quiet.img")),
            client.oem_command("dmesg"),
            client.stage_command(Path("drm-inventory.sh")),
            client.oem_command("shell-staged"),
            client.get_staged_command(Path("evidence.txt")),
        )
        self.assertEqual(
            commands[1],
            ["fastboot", "-s", "exact-a5-serial", "boot", "quiet.img"],
        )
        for command in commands:
            self.assertEqual(command[1:3], ["-s", "exact-a5-serial"])
            self.assertFalse(
                {"flash", "erase", "format", "set_active", "reboot", "continue"}
                & set(command)
            )

    def test_large_inventory_uses_ram_stage_not_inline_oem_packet(self) -> None:
        client = lab.FastbootClient("fastboot", "exact-a5-serial")
        with self.assertRaisesRegex(lab.SafetyError, "packet limit"):
            client.oem_command("shell:" + lab.INVENTORY_SCRIPT)
        self.assertEqual(
            client.stage_command(Path("drm-inventory.sh")),
            ["fastboot", "-s", "exact-a5-serial", "stage", "drm-inventory.sh"],
        )
        command = client.oem_command("shell-staged")
        self.assertLessEqual(
            len("oem shell-staged".encode("utf-8")), lab.FASTBOOT_COMMAND_MAX_BYTES
        )
        self.assertEqual(command[-2:], ["oem", "shell-staged"])

    def test_inventory_shell_is_read_only(self) -> None:
        self.assertNotIn(">", lab.INVENTORY_SCRIPT)
        self.assertNotIn("readlink -f", lab.INVENTORY_SCRIPT)
        for forbidden in (" mount ", " umount ", " dd ", " rm ", " mv ", " chmod "):
            self.assertNotIn(forbidden, " " + lab.INVENTORY_SCRIPT + " ")
        self.assertIn("/sys/kernel/debug/dri/", lab.INVENTORY_SCRIPT)
        self.assertIn("*[!0-9]*", lab.INVENTORY_SCRIPT)
        self.assertIn("-DSI-", lab.INVENTORY_SCRIPT)

    def test_identity_gate_requires_exact_pocketboot_a5u(self) -> None:
        client = lab.FastbootClient("fastboot", "exact-a5-serial")
        client.getvar = mock.Mock(
            side_effect=[
                (0, "product: pocketboot\n"),
                (0, "serialno: exact-a5-serial\n"),
                (0, "compatible: samsung,a5u-eur\n"),
                (0, "is-userspace: yes\n"),
            ]
        )

        identity = lab.verify_identity(client, 1.0)

        self.assertEqual(identity["compatible"], "samsung,a5u-eur")
        self.assertEqual(
            [call.args[0] for call in client.getvar.call_args_list],
            ["product", "serialno", "compatible", "is-userspace"],
        )

    def test_identity_gate_rejects_wrong_compatible(self) -> None:
        client = lab.FastbootClient("fastboot", "exact-a5-serial")
        client.getvar = mock.Mock(
            side_effect=[
                (0, "product: pocketboot\n"),
                (0, "serialno: exact-a5-serial\n"),
                (0, "compatible: google,crosshatch\n"),
            ]
        )

        with self.assertRaisesRegex(lab.SafetyError, "google,crosshatch"):
            lab.verify_identity(client, 1.0)

    def test_inventory_ties_enabled_dsi_mode_to_native_msm_card(self) -> None:
        proof = lab.validate_inventory(good_inventory())

        self.assertEqual(proof.card, "card0")
        self.assertEqual(proof.connector, "DSI-1")
        self.assertEqual(proof.driver, "msm-kms")
        self.assertEqual(proof.mode, "720x1280")
        self.assertEqual(proof.platform_driver, "msm_mdp")

    def test_inventory_accepts_combined_msm_driver(self) -> None:
        self.assertEqual(lab.validate_inventory(good_inventory("msm")).driver, "msm")

    def test_inventory_rejects_simpledrm_even_with_matching_dsi_records(self) -> None:
        with self.assertRaisesRegex(lab.LabError, "non-MSM DRM card"):
            lab.validate_inventory(good_inventory("simpledrm"))

    def test_inventory_rejects_advertised_but_disabled_mode(self) -> None:
        inventory = good_inventory().replace(
            "connector-enabled\t/sys/class/drm/card0-DSI-1\tenabled",
            "connector-enabled\t/sys/class/drm/card0-DSI-1\tdisabled",
        )
        with self.assertRaisesRegex(lab.LabError, "no enabled, connected"):
            lab.validate_inventory(inventory)

    def test_inventory_requires_bounded_flip_cmdline(self) -> None:
        inventory = good_inventory().replace("pocketboot.drm_page_flips=16", "")
        with self.assertRaisesRegex(lab.LabError, "pocketboot.drm_page_flips=16"):
            lab.validate_inventory(inventory)

    def test_dmesg_proves_ready_mode_and_all_ordered_flips(self) -> None:
        drm = lab.validate_inventory(good_inventory())

        result = lab.validate_dmesg(good_dmesg(), drm)

        self.assertTrue(result["ready"])
        self.assertEqual(result["page_flips_completed"], 16)
        self.assertEqual(result["sequences"], list(range(1, 17)))

    def test_dmesg_rejects_partial_flip_result(self) -> None:
        dmesg = good_dmesg().replace(
            "sequence=16 remaining=0\n", ""
        ).replace("completed=16", "completed=15")
        drm = lab.validate_inventory(good_inventory())

        with self.assertRaisesRegex(lab.LabError, "page-flip sequence"):
            lab.validate_dmesg(dmesg, drm)

    def test_dmesg_rejects_wrong_ready_dimensions(self) -> None:
        drm = lab.validate_inventory(good_inventory())
        with self.assertRaisesRegex(lab.LabError, "720x1280 POCKETBOOT_DRM_READY"):
            lab.validate_dmesg(good_dmesg().replace("width=720", "width=1080"), drm)

    def test_dmesg_rejects_fatal_ui_exit(self) -> None:
        drm = lab.validate_inventory(good_inventory())
        with self.assertRaisesRegex(lab.LabError, "fatal marker"):
            lab.validate_dmesg(good_dmesg() + "pocketboot::ui: UI thread exited\n", drm)

    def test_parses_common_fastboot_getvar_formats(self) -> None:
        self.assertEqual(
            lab.getvar_value("(bootloader) compatible: samsung,a5u-eur\nOKAY", "compatible"),
            "samsung,a5u-eur",
        )
        self.assertIsNone(lab.getvar_value("product: pocketboot\n", "compatible"))

    def test_dry_run_does_not_require_image_or_fastboot_to_exist(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--dry-run",
                "--image",
                "/does/not/exist.img",
                "--fastboot",
                "/also/missing/fastboot",
                "--fastboot-serial",
                "exact-a5-serial",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("getvar compatible", result.stdout)
        self.assertIn("transient boot:", result.stdout)
        self.assertIn("partition/storage mutations: none", result.stdout)


if __name__ == "__main__":
    unittest.main()
