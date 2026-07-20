from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import pty
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "crosshatch_drm_lab.py"
SPEC = importlib.util.spec_from_file_location("crosshatch_drm_lab", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
lab = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = lab
SPEC.loader.exec_module(lab)


class MarkerTrackerTests(unittest.TestCase):
    def test_crosshatch_panel_milestones_are_required_by_default(self) -> None:
        args = lab.build_parser().parse_args(
            [
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
            ]
        )
        tracker = lab.marker_tracker(args)
        self.assertEqual(
            [requirement.name for requirement in tracker.requirements],
            ["panel-prepared", "panel-enabled", "ready"],
        )
        tracker.observe("POCKETBOOT_DRM_READY")
        self.assertFalse(tracker.complete)
        tracker.observe("S6E3HA8_CROSSHATCH_PREPARED")
        self.assertFalse(tracker.complete)
        tracker.observe("S6E3HA8_CROSSHATCH_ENABLED")
        self.assertTrue(tracker.complete)

    def test_requires_ready_pageflips_and_blank_pairs(self) -> None:
        tracker = lab.MarkerTracker(
            [
                lab.MarkerRequirement("ready", r"READY", 1),
                lab.MarkerRequirement("flip", r"FLIP", 2),
                lab.MarkerRequirement("blank", r"BLANK$", 1),
                lab.MarkerRequirement("unblank", r"UNBLANK", 1),
            ],
            [r"underflow"],
        )
        for line in ["READY", "FLIP", "FLIP", "BLANK", "UNBLANK"]:
            self.assertIsNone(tracker.observe(line))
        self.assertTrue(tracker.complete)
        self.assertEqual(tracker.observe("DPU UNDERFLOW"), r"underflow")

    def test_default_pageflip_marker_counts_only_sequence_lines(self) -> None:
        args = lab.build_parser().parse_args(
            [
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
                "--pageflip-count",
                "16",
            ]
        )
        tracker = lab.marker_tracker(args)
        page_flip = next(
            requirement
            for requirement in tracker.requirements
            if requirement.name == "page-flip"
        )
        page_flip_result = next(
            requirement
            for requirement in tracker.requirements
            if requirement.name == "page-flip-result"
        )
        tracker.observe("S6E3HA8_CROSSHATCH_PREPARED")
        tracker.observe("S6E3HA8_CROSSHATCH_ENABLED")
        tracker.observe("POCKETBOOT_DRM_READY")
        tracker.observe("POCKETBOOT_DRM_PAGE_FLIP_TEST_START requested=16")
        for sequence in range(1, 16):
            tracker.observe(f"POCKETBOOT_DRM_PAGE_FLIP sequence={sequence}")
        tracker.observe("POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested=16 completed=15")
        self.assertEqual(page_flip.count, 15)
        self.assertFalse(page_flip.complete)
        self.assertFalse(page_flip_result.complete)

        tracker.observe("POCKETBOOT_DRM_PAGE_FLIP sequence=16")
        self.assertEqual(page_flip.count, 16)
        self.assertTrue(page_flip.complete)
        self.assertFalse(tracker.complete)

        tracker.observe("POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested=16 completed=16")
        self.assertTrue(page_flip_result.complete)
        self.assertTrue(tracker.complete)

    def test_default_failures_include_missing_drm_ui_exit(self) -> None:
        args = lab.build_parser().parse_args(
            [
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
            ]
        )
        tracker = lab.marker_tracker(args)
        self.assertEqual(
            tracker.observe(
                "WARN pocketboot::ui: UI thread exited error=open /dev/dri: "
                "No such file or directory"
            ),
            r"pocketboot::ui: UI thread exited",
        )

    def test_backlight_commands_are_short_exact_and_echo_safe(self) -> None:
        args = lab.build_parser().parse_args(
            [
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
                "--blank-cycles",
                "2",
                "--backlight-sysfs",
                "/sys/class/backlight/panel0-backlight",
                "--unblank-brightness",
                "512",
            ]
        )
        commands = lab.backlight_cycle_commands(args)
        self.assertEqual(len(commands), 3)
        self.assertEqual(lab.serial_test_commands(args), commands)
        for command in commands:
            with self.subTest(command=command):
                self.assertIn("/sys/class/backlight/panel0-backlight", command)
                self.assertLessEqual(
                    len(lab.encode_serial_command(command)),
                    lab.SERIAL_COMMAND_MAX_BYTES,
                )
                self.assertNotIn(lab.DEFAULT_BLANK_MARKER, command)
                self.assertNotIn(lab.DEFAULT_UNBLANK_MARKER, command)
                self.assertNotIn("POCKETBOOT_DRM_LAB_ERROR", command)
                syntax = subprocess.run(
                    ["sh", "-n", "-c", command],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    check=False,
                )
                self.assertEqual(syntax.returncode, 0, syntax.stderr)

        for cycle, command in enumerate(commands[1:], start=1):
            self.assertIn('echo 0 > "$pb_b"', command)
            self.assertIn('echo 512 > "$pb_b"', command)
            self.assertIn(f"BLANK cycle={cycle}", command)
            self.assertIn(f"UNBLANK cycle={cycle}", command)
            self.assertLess(command.index('echo 512 > "$pb_b"'), command.index("LAB_ERROR"))

    def test_backlight_tracker_requires_every_protocol_ack(self) -> None:
        args = lab.build_parser().parse_args(
            [
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
                "--blank-cycles",
                "2",
                "--backlight-sysfs",
                "/sys/class/backlight/panel0-backlight",
                "--unblank-brightness",
                "512",
            ]
        )
        tracker = lab.marker_tracker(args)
        requirements = {item.name: item for item in tracker.requirements}
        self.assertEqual(requirements["shell-ack"].minimum, 1)
        self.assertEqual(requirements["serial-stage-ack"].minimum, 3)

    def test_backlight_path_rejects_wildcards_and_parent_traversal(self) -> None:
        for path in [
            "/sys/class/backlight",
            "/sys/class/backlight/*",
            "/sys/class/backlight/../wrong",
            "/tmp/panel0-backlight",
        ]:
            with self.subTest(path=path), self.assertRaises(lab.LabError):
                lab.validate_backlight_path(path)


class SerialCommandProtocolTests(unittest.TestCase):
    def test_commands_are_ack_gated_and_ordered(self) -> None:
        protocol = lab.SerialCommandProtocol(["true", "printf second"])
        probe = protocol.start(10.0)
        self.assertEqual([item.command for item in probe], [lab.SHELL_ACK_COMMAND])
        self.assertNotIn(lab.SHELL_ACK_MARKER, lab.SHELL_ACK_COMMAND)
        self.assertEqual(protocol.observe(lab.SHELL_ACK_COMMAND), ())
        self.assertEqual(protocol.poll(10.99), ())
        self.assertEqual(len(protocol.poll(11.0)), 1)

        self.assertEqual(protocol.observe(f"{lab.SHELL_ACK_MARKER}-wrong"), ())
        first = protocol.observe(lab.SHELL_ACK_MARKER)
        self.assertEqual([item.command for item in first], ["true", lab.stage_ack_command(1)])
        self.assertNotIn(lab.STAGE_ACK_MARKER, first[1].command)
        self.assertEqual(protocol.observe(first[1].command), ())
        self.assertEqual(
            protocol.observe(f"{lab.STAGE_ACK_MARKER} sequence=2 rc=0"), ()
        )

        second = protocol.observe(f"{lab.STAGE_ACK_MARKER} sequence=1 rc=0")
        self.assertEqual(
            [item.command for item in second],
            ["printf second", lab.stage_ack_command(2)],
        )
        self.assertFalse(protocol.complete)
        self.assertEqual(protocol.observe(f"{lab.STAGE_ACK_MARKER} sequence=2 rc=0"), ())
        self.assertTrue(protocol.complete)

    def test_failed_stage_is_reported_and_not_retried(self) -> None:
        protocol = lab.SerialCommandProtocol(["false", "echo not-sent"])
        protocol.start(0.0)
        transmissions = protocol.observe(lab.SHELL_ACK_MARKER)
        self.assertEqual(transmissions[0].command, "false")
        self.assertEqual(
            protocol.observe(f"{lab.STAGE_ACK_MARKER} sequence=1 rc=7"), ()
        )
        self.assertEqual(protocol.phase, "failed")
        self.assertIn("stage 1/2 exited 7", protocol.failure_reason)

    def test_failure_waits_for_active_stage_ack_before_abort(self) -> None:
        protocol = lab.SerialCommandProtocol(["sleep 1", "echo not-sent"])
        protocol.start(0.0)
        protocol.observe(lab.SHELL_ACK_MARKER)
        protocol.request_stop()
        self.assertFalse(protocol.safe_to_abort)

        self.assertEqual(
            protocol.observe(f"{lab.STAGE_ACK_MARKER} sequence=1 rc=0"), ()
        )
        self.assertTrue(protocol.safe_to_abort)
        self.assertEqual(protocol.phase, "stopped")

    def test_empty_protocol_needs_no_handshake(self) -> None:
        protocol = lab.SerialCommandProtocol([])
        self.assertTrue(protocol.complete)
        self.assertFalse(protocol.can_start)
        self.assertEqual(protocol.start(0.0), ())

    def test_utf8_byte_budget_and_controls_are_enforced(self) -> None:
        self.assertEqual(
            len(lab.encode_serial_command("a" * (lab.SERIAL_COMMAND_MAX_BYTES - 1))),
            lab.SERIAL_COMMAND_MAX_BYTES,
        )
        self.assertEqual(
            len(lab.encode_serial_command("é" * 255 + "a")),
            lab.SERIAL_COMMAND_MAX_BYTES,
        )
        for command in ["a" * lab.SERIAL_COMMAND_MAX_BYTES, "é" * 256, "", "x\ny", "x\ry", "x\0y"]:
            with self.subTest(command=repr(command)), self.assertRaises(lab.LabError):
                lab.encode_serial_command(command)


class FastbootSafetyTests(unittest.TestCase):
    def test_sha256_known_vector(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "vector"
            path.write_bytes(b"abc")
            self.assertEqual(
                lab.sha256_file(path),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            )

    def test_fastboot_surface_has_no_partition_writing_commands(self) -> None:
        client = lab.FastbootClient("fastboot", "explicit-serial")
        self.assertEqual(
            client.getvar_command("product"),
            ["fastboot", "-s", "explicit-serial", "getvar", "product"],
        )
        command = client.boot_command(Path("boot.img"))
        self.assertEqual(command, ["fastboot", "-s", "explicit-serial", "boot", "boot.img"])
        self.assertNotIn(command[-2], {"flash", "erase", "format", "set_active"})
        self.assertEqual(
            client.reboot_bootloader_command(),
            ["fastboot", "-s", "explicit-serial", "reboot", "bootloader"],
        )

    def test_pocketboot_reboot_requires_exact_target_identity(self) -> None:
        client = lab.FastbootClient("fastboot", "explicit-serial")
        client.getvar = mock.Mock(
            side_effect=[
                (0, "product: pocketboot\n"),
                (0, "serialno: explicit-serial\n"),
                (0, "compatible: google,crosshatch\n"),
            ]
        )
        client._command_with_uart = mock.Mock(return_value=(0, "Rebooting\n"))
        serial = mock.Mock()
        log = mock.Mock()

        client.reboot_bootloader(5.0, serial, log)

        self.assertEqual(
            [call.args[0] for call in client.getvar.call_args_list],
            ["product", "serialno", "compatible"],
        )
        client._command_with_uart.assert_called_once_with(
            ["fastboot", "-s", "explicit-serial", "reboot", "bootloader"],
            5.0,
            serial,
            log,
            "fastboot reboot bootloader",
        )

    def test_pocketboot_identity_mismatch_refuses_reboot(self) -> None:
        client = lab.FastbootClient("fastboot", "explicit-serial")
        client.getvar = mock.Mock(
            side_effect=[
                (0, "product: pocketboot\n"),
                (0, "serialno: unrelated-device\n"),
            ]
        )
        client._command_with_uart = mock.Mock()

        with self.assertRaisesRegex(lab.SafetyError, "refusing reboot"):
            client.reboot_bootloader(5.0, mock.Mock(), mock.Mock())

        client._command_with_uart.assert_not_called()

    def test_failure_sysrq_recovery_precedes_between_attempt_fastboot(self) -> None:
        args = lab.build_parser().parse_args(
            [
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
                "--attempts",
                "2",
                "--pocketboot-reboot-bootloader-between-attempts",
                "--sysrq-reboot-on-failure",
                "--sysrq-dump",
                "w",
            ]
        )
        result = lab.AttemptResult(1, "failed", "panic", 1.0, {})
        serial = mock.Mock()
        fastboot = mock.Mock()
        log = mock.Mock()

        with mock.patch.object(lab, "send_sysrq_sequence") as send_sysrq:
            lab.return_to_bootloader_between_attempts(
                result, serial, fastboot, args, log
            )

        send_sysrq.assert_called_once_with(serial, ["w"], True, args, log)
        fastboot.reboot_bootloader.assert_not_called()

    def test_parses_common_fastboot_getvar_formats(self) -> None:
        self.assertEqual(lab.getvar_value("product: crosshatch\n", "product"), "crosshatch")
        self.assertEqual(
            lab.getvar_value("(bootloader) unlocked: yes\nFinished\n", "unlocked"),
            "yes",
        )
        self.assertIsNone(lab.getvar_value("FAILED (remote: unknown variable)\n", "product"))

    def test_product_mismatch_fails_before_boot(self) -> None:
        client = lab.FastbootClient("fastboot", "explicit-serial")
        client.getvar = mock.Mock(return_value=(0, "product: lk2nd-msm8916\n"))
        with self.assertRaisesRegex(lab.SafetyError, "expected 'crosshatch'"):
            client.wait_and_verify("crosshatch", False, 1.0, mock.Mock())

    def test_getvar_captures_uart_while_fastboot_is_running(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            fake_fastboot = directory / "fastboot"
            fake_fastboot.write_text(
                "#!/bin/sh\nsleep 0.1\nprintf 'product: crosshatch\\n' >&2\n",
                encoding="utf-8",
            )
            fake_fastboot.chmod(0o755)
            log_path = directory / "uart.log"
            log = lab.TimestampedLog(log_path, quiet=True)
            master, slave = pty.openpty()
            slave_path = Path(os.ttyname(slave))
            os.close(slave)

            def write_uart() -> None:
                time.sleep(0.02)
                os.write(master, b"UART-DURING-GETVAR\r\n")

            writer = threading.Thread(target=write_uart)
            writer.start()
            try:
                with lab.SerialPort(slave_path, 115200) as serial:
                    returncode, output = lab.FastbootClient(
                        str(fake_fastboot), "explicit-serial"
                    ).getvar("product", 1.0, serial=serial, log=log)
            finally:
                writer.join()
                os.close(master)
                log.close()

            self.assertEqual(returncode, 0)
            self.assertIn("product: crosshatch", output)
            self.assertIn("UART-DURING-GETVAR", log_path.read_text(encoding="utf-8"))

    def test_dry_run_does_not_require_devices_or_image(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--dry-run",
                "--image",
                "/does/not/exist/boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/does/not/exist/tty",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("getvar product", result.stdout)
        self.assertIn(" boot /does/not/exist/boot.img", result.stdout)
        self.assertIn(lab.DEFAULT_PANEL_PREPARED_MARKER, result.stdout)
        self.assertIn(lab.DEFAULT_PANEL_ENABLED_MARKER, result.stdout)
        self.assertNotIn(" flash ", result.stdout)

    def test_repeated_attempts_require_explicit_reboot_mechanism(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--dry-run",
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
                "--attempts",
                "2",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("repeated attempts require", result.stderr)

    def test_dry_run_describes_identity_checked_pocketboot_reboot(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--dry-run",
                "--image",
                "boot.img",
                "--fastboot-serial",
                "explicit-serial",
                "--serial-port",
                "/dev/ttyUSB-test",
                "--attempts",
                "2",
                "--pocketboot-reboot-bootloader-between-attempts",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("getvar product", result.stdout)
        self.assertIn("getvar serialno", result.stdout)
        self.assertIn("getvar compatible", result.stdout)
        self.assertIn("reboot bootloader", result.stdout)
        self.assertNotIn(" flash ", result.stdout)


if __name__ == "__main__":
    unittest.main()
