#!/usr/bin/env python3
"""Ephemeral Crosshatch MSM DRM bring-up runner over the SBU debug UART.

This program intentionally has no fastboot flash/erase support.  It opens an
explicitly named UART, verifies an explicitly named fastboot device, and uses
only ``fastboot boot`` plus an optional identity-gated reboot to ABL.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import stat
import subprocess
import sys
import termios
import time
from typing import Callable, Iterable, Sequence


DEFAULT_READY_MARKER = r"POCKETBOOT_DRM_READY"
DEFAULT_PANEL_PREPARED_MARKER = r"S6E3HA8_CROSSHATCH_PREPARED"
DEFAULT_PANEL_ENABLED_MARKER = r"S6E3HA8_CROSSHATCH_ENABLED"
DEFAULT_PAGEFLIP_MARKER = r"POCKETBOOT_DRM_PAGE_FLIP sequence="
DEFAULT_BLANK_MARKER = r"POCKETBOOT_DRM_BLANK"
DEFAULT_UNBLANK_MARKER = r"POCKETBOOT_DRM_UNBLANK"
SHELL_ACK_MARKER = "POCKETBOOT_DRM_LAB_SHELL_ACK"
STAGE_ACK_MARKER = "POCKETBOOT_DRM_LAB_STAGE_ACK"
SERIAL_COMMAND_MAX_BYTES = 512
SHELL_ACK_RETRY_SECONDS = 1.0
POCKETBOOT_PRODUCT = "pocketboot"
CROSSHATCH_COMPATIBLE = "google,crosshatch"
SHELL_ACK_COMMAND = (
    'pb_p=POCKETBOOT_DRM_LAB_; printf "%s%s\\n" "$pb_p" SHELL_ACK > /dev/kmsg'
)
DEFAULT_FAILURE_MARKERS = (
    r"Kernel panic - not syncing",
    r"Internal error: Oops",
    r"(?:soft|hard) LOCKUP",
    r"(?:dpu|mdp).*underflow",
    r"status stuck at 'on'",
    r"pocketboot::ui: UI thread exited",
    r"POCKETBOOT_DRM_LAB_ERROR",
)
SYSRQ_DUMP_KEYS = ("l", "m", "p", "t", "w")
FORBIDDEN_TTYS = {
    "/dev/console",
    "/dev/ptmx",
    "/dev/tty",
    "/dev/tty0",
}


class LabError(Exception):
    """An expected, user-facing lab runner error."""


class SafetyError(LabError):
    """A target or operation failed a safety check."""


@dataclasses.dataclass
class MarkerRequirement:
    name: str
    expression: str
    minimum: int
    count: int = 0

    def __post_init__(self) -> None:
        try:
            self.pattern = re.compile(self.expression)
        except re.error as error:
            raise LabError(f"invalid {self.name} marker regex {self.expression!r}: {error}") from error

    def observe(self, line: str) -> None:
        if self.pattern.search(line):
            self.count += 1

    @property
    def complete(self) -> bool:
        return self.count >= self.minimum


class MarkerTracker:
    def __init__(
        self,
        requirements: Iterable[MarkerRequirement],
        failure_expressions: Iterable[str],
    ) -> None:
        self.requirements = list(requirements)
        self.failures: list[tuple[str, re.Pattern[str]]] = []
        for expression in failure_expressions:
            try:
                pattern = re.compile(expression, re.IGNORECASE)
            except re.error as error:
                raise LabError(f"invalid failure marker regex {expression!r}: {error}") from error
            self.failures.append((expression, pattern))

    def observe(self, line: str) -> str | None:
        for requirement in self.requirements:
            requirement.observe(line)
        for expression, pattern in self.failures:
            if pattern.search(line):
                return expression
        return None

    @property
    def complete(self) -> bool:
        return all(requirement.complete for requirement in self.requirements)

    def count_summary(self) -> dict[str, dict[str, int | str]]:
        return {
            requirement.name: {
                "expression": requirement.expression,
                "minimum": requirement.minimum,
                "observed": requirement.count,
            }
            for requirement in self.requirements
        }


class TimestampedLog:
    def __init__(self, path: Path, quiet: bool) -> None:
        self.path = path
        self.quiet = quiet
        self.file = path.open("x", encoding="utf-8", buffering=1)

    def close(self) -> None:
        self.file.close()

    def line(self, source: str, text: str) -> None:
        timestamp = dt.datetime.now(dt.timezone.utc).isoformat(timespec="milliseconds")
        rendered = f"{timestamp} {source:<8} {text.rstrip()}"
        self.file.write(rendered + "\n")
        if not self.quiet:
            print(rendered, flush=True)


class LineBuffer:
    def __init__(self, emit: Callable[[str], None]) -> None:
        self.buffer = bytearray()
        self.emit = emit

    def feed(self, data: bytes) -> None:
        self.buffer.extend(data)
        while True:
            newline = self.buffer.find(b"\n")
            if newline < 0:
                break
            raw = bytes(self.buffer[:newline])
            del self.buffer[: newline + 1]
            self.emit(raw.rstrip(b"\r").decode("utf-8", errors="replace"))
        if len(self.buffer) > 1024 * 1024:
            self.flush()

    def flush(self) -> None:
        if self.buffer:
            raw = bytes(self.buffer)
            self.buffer.clear()
            self.emit(raw.rstrip(b"\r").decode("utf-8", errors="replace"))


def baud_constant(baud: int) -> int:
    value = getattr(termios, f"B{baud}", None)
    if value is None:
        raise LabError(f"unsupported host UART baud rate: {baud}")
    return value


def encode_serial_command(command: str) -> bytes:
    """Encode one complete interactive-shell line within the lab's safe budget."""

    if not command.strip():
        raise LabError("serial shell command must not be empty")
    if any(character in command for character in ("\x00", "\n", "\r")):
        raise LabError("serial shell command contains a NUL, newline, or carriage return")
    encoded = command.encode("utf-8") + b"\r"
    if len(encoded) > SERIAL_COMMAND_MAX_BYTES:
        raise LabError(
            "serial shell command is "
            f"{len(encoded)} UTF-8 bytes including carriage return; "
            f"maximum is {SERIAL_COMMAND_MAX_BYTES}"
        )
    return encoded


def stage_ack_command(sequence: int) -> str:
    """Build an echo-safe barrier which reports the preceding shell status."""

    command = (
        'pb_rc=$?; pb_p=POCKETBOOT_DRM_LAB_; '
        f'printf "%s%s%s\\n" "$pb_p" "STAGE_ACK sequence={sequence} rc=" '
        '"$pb_rc" > /dev/kmsg'
    )
    encode_serial_command(command)
    return command


@dataclasses.dataclass(frozen=True)
class SerialTransmission:
    description: str
    command: str


class SerialCommandProtocol:
    """Gate target shell stages behind echo-safe, ordered UART ACKs."""

    def __init__(self, commands: Iterable[str]) -> None:
        self.commands = tuple(commands)
        encode_serial_command(SHELL_ACK_COMMAND)
        for command in self.commands:
            encode_serial_command(command)
        self.barriers = tuple(
            stage_ack_command(sequence)
            for sequence in range(1, len(self.commands) + 1)
        )
        self.phase = "complete" if not self.commands else "idle"
        self.active_stage = 0
        self.next_probe_at: float | None = None
        self.stop_requested = False
        self.failure_reason: str | None = None

    @property
    def stage_count(self) -> int:
        return len(self.commands)

    @property
    def can_start(self) -> bool:
        return self.phase == "idle"

    @property
    def complete(self) -> bool:
        return self.phase == "complete"

    @property
    def safe_to_abort(self) -> bool:
        return self.phase != "stage-ack"

    def start(self, now: float) -> tuple[SerialTransmission, ...]:
        if not self.can_start:
            return ()
        self.phase = "shell-ack"
        self.next_probe_at = now + SHELL_ACK_RETRY_SECONDS
        return (SerialTransmission("shell ACK probe", SHELL_ACK_COMMAND),)

    def poll(self, now: float) -> tuple[SerialTransmission, ...]:
        if (
            self.phase != "shell-ack"
            or self.next_probe_at is None
            or now < self.next_probe_at
        ):
            return ()
        self.next_probe_at = now + SHELL_ACK_RETRY_SECONDS
        return (SerialTransmission("shell ACK probe retry", SHELL_ACK_COMMAND),)

    def request_stop(self) -> None:
        if self.phase == "stage-ack":
            self.stop_requested = True
        elif self.phase not in {"complete", "failed", "stopped"}:
            self.phase = "stopped"

    def observe(self, line: str) -> tuple[SerialTransmission, ...]:
        if self.phase == "shell-ack":
            if re.search(
                rf"{re.escape(SHELL_ACK_MARKER)}(?![A-Za-z0-9_-])", line
            ) is None:
                return ()
            self.next_probe_at = None
            self.active_stage = 1
            self.phase = "stage-ack"
            return self._active_transmissions()

        if self.phase != "stage-ack":
            return ()
        expression = re.compile(
            rf"{re.escape(STAGE_ACK_MARKER)} sequence={self.active_stage} "
            r"rc=(-?[0-9]+)(?![0-9])"
        )
        match = expression.search(line)
        if match is None:
            return ()
        returncode = int(match.group(1))
        if returncode != 0:
            self.failure_reason = (
                f"serial test stage {self.active_stage}/{self.stage_count} "
                f"exited {returncode}"
            )
            self.phase = "failed"
            return ()
        if self.stop_requested:
            self.phase = "stopped"
            return ()
        if self.active_stage == self.stage_count:
            self.phase = "complete"
            return ()
        self.active_stage += 1
        return self._active_transmissions()

    def _active_transmissions(self) -> tuple[SerialTransmission, ...]:
        index = self.active_stage - 1
        return (
            SerialTransmission(
                f"serial test stage {self.active_stage}/{self.stage_count}",
                self.commands[index],
            ),
            SerialTransmission(
                f"serial test stage {self.active_stage}/{self.stage_count} ACK barrier",
                self.barriers[index],
            ),
        )


def validate_serial_path(path: Path) -> Path:
    try:
        resolved = path.resolve(strict=True)
        mode = resolved.stat().st_mode
    except OSError as error:
        raise SafetyError(f"cannot resolve SBU UART serial port {path}: {error}") from error
    if str(resolved) in FORBIDDEN_TTYS or str(path) in FORBIDDEN_TTYS:
        raise SafetyError(f"refusing unsafe general-purpose tty: {path}")
    if not stat.S_ISCHR(mode):
        raise SafetyError(f"SBU UART serial port is not a character device: {path}")

    target_rdev = resolved.stat().st_rdev
    for fd, description in ((0, "stdin"), (1, "stdout"), (2, "stderr")):
        try:
            same_terminal = os.isatty(fd) and os.fstat(fd).st_rdev == target_rdev
        except OSError:
            same_terminal = False
        if same_terminal:
            raise SafetyError(f"refusing to use the process {description} terminal as SBU UART")
    return resolved


class SerialPort:
    def __init__(self, path: Path, baud: int) -> None:
        self.path = path
        self.baud = baud
        self.fd: int | None = None
        self.saved_attributes: list | None = None

    def __enter__(self) -> "SerialPort":
        self.fd = os.open(self.path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        self.saved_attributes = termios.tcgetattr(self.fd)
        attributes = termios.tcgetattr(self.fd)
        attributes[0] = 0
        attributes[1] = 0
        attributes[2] &= ~(termios.PARENB | termios.CSTOPB | termios.CSIZE)
        if hasattr(termios, "CRTSCTS"):
            attributes[2] &= ~termios.CRTSCTS
        attributes[2] |= termios.CS8 | termios.CREAD | termios.CLOCAL
        attributes[3] = 0
        speed = baud_constant(self.baud)
        attributes[4] = speed
        attributes[5] = speed
        attributes[6][termios.VMIN] = 0
        attributes[6][termios.VTIME] = 0
        termios.tcsetattr(self.fd, termios.TCSANOW, attributes)
        return self

    def __exit__(self, _type, _value, _traceback) -> None:
        if self.fd is None:
            return
        if self.saved_attributes is not None:
            try:
                termios.tcsetattr(self.fd, termios.TCSANOW, self.saved_attributes)
            except OSError:
                pass
        os.close(self.fd)
        self.fd = None

    def fileno(self) -> int:
        if self.fd is None:
            raise LabError("serial port is not open")
        return self.fd

    def read(self) -> bytes:
        try:
            return os.read(self.fileno(), 64 * 1024)
        except BlockingIOError:
            return b""
        except OSError as error:
            raise LabError(f"read SBU UART serial port: {error}") from error

    def write(self, data: bytes) -> None:
        offset = 0
        while offset < len(data):
            try:
                written = os.write(self.fileno(), data[offset:])
            except BlockingIOError:
                select.select([], [self.fileno()], [], 1.0)
                continue
            except OSError as error:
                raise LabError(f"write SBU UART serial port: {error}") from error
            if written <= 0:
                raise LabError("write SBU UART serial port returned no progress")
            offset += written

    def send_command(self, command: str) -> None:
        self.write(encode_serial_command(command))

    def send_sysrq(self, key: str, break_seconds: float, key_delay: float) -> None:
        fd = self.fileno()
        if hasattr(termios, "TIOCSBRK") and hasattr(termios, "TIOCCBRK"):
            fcntl.ioctl(fd, termios.TIOCSBRK)
            time.sleep(break_seconds)
            fcntl.ioctl(fd, termios.TIOCCBRK)
        else:
            termios.tcsendbreak(fd, 0)
        time.sleep(key_delay)
        self.write(key.encode("ascii"))


def getvar_value(output: str, name: str) -> str | None:
    pattern = re.compile(rf"(?:\(bootloader\)\s*)?{re.escape(name)}\s*:\s*(\S+)", re.IGNORECASE)
    match = pattern.search(output)
    return match.group(1) if match else None


class FastbootClient:
    """The deliberately tiny, non-writing fastboot surface."""

    def __init__(self, executable: str, serial: str) -> None:
        self.executable = executable
        self.serial = serial

    def getvar_command(self, name: str) -> list[str]:
        return [self.executable, "-s", self.serial, "getvar", name]

    def boot_command(self, image: Path) -> list[str]:
        return [self.executable, "-s", self.serial, "boot", str(image)]

    def reboot_bootloader_command(self) -> list[str]:
        return [self.executable, "-s", self.serial, "reboot", "bootloader"]

    def getvar(
        self,
        name: str,
        timeout: float,
        serial: SerialPort | None = None,
        log: TimestampedLog | None = None,
    ) -> tuple[int, str]:
        if serial is not None and log is not None:
            return self._getvar_with_uart(name, timeout, serial, log)
        try:
            result = subprocess.run(
                self.getvar_command(name),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired:
            return 124, "fastboot getvar timed out"
        return result.returncode, result.stdout

    def _getvar_with_uart(
        self,
        name: str,
        timeout: float,
        serial: SerialPort,
        log: TimestampedLog,
    ) -> tuple[int, str]:
        return self._command_with_uart(
            self.getvar_command(name), timeout, serial, log, "fastboot getvar"
        )

    def _command_with_uart(
        self,
        command: Sequence[str],
        timeout: float,
        serial: SerialPort,
        log: TimestampedLog,
        description: str,
    ) -> tuple[int, str]:
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        if process.stdout is None:
            terminate_process(process)
            raise LabError(f"{description} output pipe was not created")
        os.set_blocking(process.stdout.fileno(), False)
        output = bytearray()
        uart_buffer = LineBuffer(lambda line: log.line("UART", line))
        fastboot_buffer = LineBuffer(lambda line: log.line("FASTBOOT", line))
        deadline = time.monotonic() + timeout
        timed_out = False
        try:
            while process.poll() is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    timed_out = True
                    break
                readable, _, _ = select.select(
                    [serial.fileno(), process.stdout.fileno()],
                    [],
                    [],
                    min(0.25, remaining),
                )
                if serial.fileno() in readable:
                    data = serial.read()
                    if data:
                        uart_buffer.feed(data)
                if process.stdout.fileno() in readable:
                    try:
                        data = os.read(process.stdout.fileno(), 64 * 1024)
                    except BlockingIOError:
                        data = b""
                    if data:
                        output.extend(data)
                        fastboot_buffer.feed(data)
            if process.poll() is not None:
                while True:
                    try:
                        data = os.read(process.stdout.fileno(), 64 * 1024)
                    except BlockingIOError:
                        break
                    if not data:
                        break
                    output.extend(data)
                    fastboot_buffer.feed(data)
        finally:
            if process.poll() is None:
                terminate_process(process)
            uart_buffer.flush()
            fastboot_buffer.flush()
            process.stdout.close()
        if timed_out:
            return 124, f"{description} timed out"
        return process.returncode or 0, output.decode("utf-8", errors="replace")

    def wait_and_verify(
        self,
        expected_product: str,
        allow_locked: bool,
        timeout: float,
        log: TimestampedLog,
        serial: SerialPort | None = None,
    ) -> None:
        deadline = time.monotonic() + timeout
        last_output = "device did not answer"
        while time.monotonic() < deadline:
            remaining = max(0.1, deadline - time.monotonic())
            returncode, output = self.getvar(
                "product", min(5.0, remaining), serial=serial, log=log
            )
            last_output = output.strip() or f"fastboot exited {returncode}"
            product = getvar_value(output, "product")
            if product is not None:
                if product.casefold() != expected_product.casefold():
                    raise SafetyError(
                        f"fastboot product is {product!r}, expected {expected_product!r}; refusing to boot"
                    )
                if returncode != 0:
                    raise SafetyError(f"fastboot product check failed: {last_output}")
                break
            if serial is None:
                time.sleep(min(0.5, remaining))
            else:
                capture_uart(serial, log, min(0.5, remaining))
        else:
            raise LabError(f"timed out waiting for the explicit fastboot device: {last_output}")

        log.line("HOST", f"verified fastboot product {expected_product!r}")
        if allow_locked:
            log.line("HOST", "bootloader unlocked-state check explicitly disabled")
            return
        returncode, output = self.getvar(
            "unlocked", min(5.0, timeout), serial=serial, log=log
        )
        unlocked = getvar_value(output, "unlocked")
        if returncode != 0 or unlocked is None:
            raise SafetyError(f"could not prove bootloader is unlocked: {output.strip()}")
        if unlocked.casefold() not in {"1", "true", "yes"}:
            raise SafetyError(f"bootloader reports unlocked={unlocked!r}; refusing to boot")
        log.line("HOST", "verified bootloader unlocked state")

    def start_boot(self, image: Path) -> subprocess.Popen[bytes]:
        return subprocess.Popen(
            self.boot_command(image),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    def verify_pocketboot_reboot_target(
        self,
        timeout: float,
        serial: SerialPort | None = None,
        log: TimestampedLog | None = None,
    ) -> None:
        checks = (
            ("product", POCKETBOOT_PRODUCT),
            ("serialno", self.serial),
            ("compatible", CROSSHATCH_COMPATIBLE),
        )
        for name, expected in checks:
            returncode, output = self.getvar(
                name, min(5.0, timeout), serial=serial, log=log
            )
            observed = getvar_value(output, name)
            if returncode != 0 or observed is None:
                raise SafetyError(
                    f"could not prove Pocketboot {name} before reboot: {output.strip()}"
                )
            if observed.casefold() != expected.casefold():
                raise SafetyError(
                    f"Pocketboot {name} is {observed!r}, expected {expected!r}; "
                    "refusing reboot"
                )
        if log is not None:
            log.line(
                "HOST",
                "verified Pocketboot product, serial number, and Crosshatch compatibility",
            )

    def reboot_bootloader(
        self,
        timeout: float,
        serial: SerialPort,
        log: TimestampedLog,
    ) -> None:
        self.verify_pocketboot_reboot_target(timeout, serial=serial, log=log)
        log.line("HOST", "issuing identity-checked fastboot reboot bootloader")
        returncode, output = self._command_with_uart(
            self.reboot_bootloader_command(),
            timeout,
            serial,
            log,
            "fastboot reboot bootloader",
        )
        if returncode != 0:
            raise LabError(
                "fastboot reboot bootloader failed: "
                f"{output.strip() or f'exit status {returncode}'}"
            )


def terminate_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=2)


@dataclasses.dataclass
class AttemptResult:
    attempt: int
    status: str
    reason: str
    elapsed_seconds: float
    markers: dict[str, dict[str, int | str]]


def marker_tracker(
    args: argparse.Namespace, serial_stage_count: int | None = None
) -> MarkerTracker:
    if serial_stage_count is None:
        serial_stage_count = len(serial_test_commands(args))
    requirements = [
        MarkerRequirement("panel-prepared", DEFAULT_PANEL_PREPARED_MARKER, 1),
        MarkerRequirement("panel-enabled", DEFAULT_PANEL_ENABLED_MARKER, 1),
        MarkerRequirement("ready", args.ready_marker, 1),
    ]
    requirements.extend(
        MarkerRequirement(f"expect-{index}", expression, 1)
        for index, expression in enumerate(args.expect, start=1)
    )
    if args.pageflip_count:
        requirements.extend(
            [
                MarkerRequirement(
                    "page-flip", args.pageflip_marker, args.pageflip_count
                ),
                MarkerRequirement(
                    "page-flip-result",
                    rf"POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested={args.pageflip_count} "
                    rf"completed={args.pageflip_count}(?![0-9])",
                    1,
                ),
            ]
        )
    if args.blank_cycles:
        requirements.extend(
            [
                MarkerRequirement("blank", args.blank_marker, args.blank_cycles),
                MarkerRequirement("unblank", args.unblank_marker, args.blank_cycles),
            ]
        )
    if serial_stage_count:
        requirements.extend(
            [
                MarkerRequirement(
                    "shell-ack",
                    rf"{re.escape(SHELL_ACK_MARKER)}(?![A-Za-z0-9_-])",
                    1,
                ),
                MarkerRequirement(
                    "serial-stage-ack",
                    rf"{re.escape(STAGE_ACK_MARKER)} sequence=[0-9]+ rc=0(?![0-9])",
                    serial_stage_count,
                ),
            ]
        )
    failures = [] if args.no_default_failure_markers else list(DEFAULT_FAILURE_MARKERS)
    failures.extend(args.failure_marker)
    return MarkerTracker(requirements, failures)


def validate_backlight_path(value: str) -> str:
    if "\x00" in value or "\n" in value or "\r" in value:
        raise LabError("backlight sysfs path contains a control character")
    path = Path(value)
    if not path.is_absolute() or ".." in path.parts:
        raise LabError("backlight sysfs path must be absolute and contain no '..'")
    expected_prefix = ("/", "sys", "class", "backlight")
    if path.parts[:4] != expected_prefix or len(path.parts) != 5:
        raise LabError(
            "backlight sysfs path must name one exact /sys/class/backlight/DEVICE directory"
        )
    if not re.fullmatch(r"[A-Za-z0-9._:-]+", path.parts[4]):
        raise LabError("backlight sysfs device name contains unsafe or wildcard characters")
    return value


def backlight_cycle_commands(args: argparse.Namespace) -> list[str]:
    """Build short, independently parseable DCS-backlight shell stages.

    Marker names are assembled from a variable on-device so tty command echo
    cannot satisfy the host-side expectations before an operation succeeds.
    """

    directory = shlex.quote(validate_backlight_path(args.backlight_sysfs))
    brightness = shlex.quote(f"{args.backlight_sysfs}/brightness")
    level = args.unblank_brightness
    commands = [
        " ".join(
            [
                f"pb_d={directory};",
                "pb_r=0;",
                '[ -d "$pb_d" ] && [ -w "$pb_d/brightness" ] && '
                '[ -r "$pb_d/max_brightness" ] && [ -w /dev/kmsg ] || pb_r=1;',
                'pb_m=$(cat "$pb_d/max_brightness" 2>/dev/null) || pb_r=1;',
                'case "$pb_m" in \'\'|*[!0-9]*) pb_r=1;; esac;',
                f'[ "$pb_r" -eq 0 ] && [ {level} -le "$pb_m" ] || pb_r=1;',
                'if [ "$pb_r" -ne 0 ]; then pb_p=POCKETBOOT_DRM_; '
                'pb_e="${pb_p}LAB_ERROR backlight-preflight"; echo "$pb_e"; '
                'echo "$pb_e" > /dev/kmsg 2>/dev/null; fi;',
                '[ "$pb_r" -eq 0 ]',
            ]
        )
    ]
    for cycle in range(1, args.blank_cycles + 1):
        commands.append(
            " ".join(
                [
                    f"pb_b={brightness};",
                    "pb_p=POCKETBOOT_DRM_;",
                    "pb_r=0;",
                    f'{{ echo 0 > "$pb_b" && echo "${{pb_p}}BLANK cycle={cycle}" '
                    '> /dev/kmsg; } || pb_r=1;',
                    f"sleep {args.blank_hold_seconds:g} || pb_r=1;",
                    f'{{ echo {level} > "$pb_b" && echo "${{pb_p}}UNBLANK cycle={cycle}" '
                    '> /dev/kmsg; } || pb_r=1;',
                    f"sleep {args.unblank_hold_seconds:g} || pb_r=1;",
                    f'if [ "$pb_r" -ne 0 ]; then pb_e="${{pb_p}}LAB_ERROR cycle={cycle}"; '
                    'echo "$pb_e"; echo "$pb_e" > /dev/kmsg 2>/dev/null; fi;',
                    '[ "$pb_r" -eq 0 ]',
                ]
            )
        )
    return commands


def serial_test_commands(args: argparse.Namespace) -> list[str]:
    commands = list(args.serial_command)
    if args.blank_cycles:
        commands.extend(backlight_cycle_commands(args))
    return commands


def drain_uart(serial: SerialPort, log: TimestampedLog) -> None:
    line_buffer = LineBuffer(lambda line: log.line("UART-OLD", line))
    while select.select([serial.fileno()], [], [], 0)[0]:
        data = serial.read()
        if not data:
            break
        line_buffer.feed(data)
    line_buffer.flush()


def capture_uart(serial: SerialPort, log: TimestampedLog, duration: float) -> None:
    deadline = time.monotonic() + duration
    line_buffer = LineBuffer(lambda line: log.line("UART", line))
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        if not select.select([serial.fileno()], [], [], min(0.25, remaining))[0]:
            continue
        data = serial.read()
        if data:
            line_buffer.feed(data)
    line_buffer.flush()


def run_attempt(
    attempt: int,
    serial: SerialPort,
    fastboot: FastbootClient,
    image: Path,
    args: argparse.Namespace,
    log: TimestampedLog,
) -> AttemptResult:
    protocol = SerialCommandProtocol(serial_test_commands(args))
    tracker = marker_tracker(args, protocol.stage_count)
    drain_uart(serial, log)
    start = time.monotonic()
    process = fastboot.start_boot(image)
    if process.stdout is None:
        terminate_process(process)
        raise LabError("fastboot output pipe was not created")
    os.set_blocking(process.stdout.fileno(), False)
    log.line("HOST", f"attempt {attempt}: issued fastboot boot (ephemeral)")

    failure_reason: str | None = None

    def send_transmissions(transmissions: Iterable[SerialTransmission]) -> None:
        for transmission in transmissions:
            log.line("HOST", f"attempt {attempt}: sending {transmission.description}")
            serial.send_command(transmission.command)

    def uart_line(line: str) -> None:
        nonlocal failure_reason
        log.line("UART", line)
        failure = tracker.observe(line)
        if failure is not None and failure_reason is None:
            failure_reason = f"UART failure marker matched: {failure}"
            protocol.request_stop()
        ready = next(
            requirement for requirement in tracker.requirements if requirement.name == "ready"
        )
        if ready.complete and protocol.can_start and failure_reason is None:
            send_transmissions(protocol.start(time.monotonic()))
        send_transmissions(protocol.observe(line))
        if protocol.failure_reason is not None and failure_reason is None:
            failure_reason = protocol.failure_reason

    uart_buffer = LineBuffer(uart_line)
    fastboot_buffer = LineBuffer(lambda line: log.line("FASTBOOT", line))
    deadline = start + args.timeout
    fastboot_done = False
    fastboot_returncode: int | None = None

    try:
        while True:
            if failure_reason is not None and protocol.safe_to_abort:
                break
            if fastboot_done and fastboot_returncode != 0:
                failure_reason = f"fastboot boot exited {fastboot_returncode}"
                protocol.request_stop()
                if protocol.safe_to_abort:
                    break
            if fastboot_done and tracker.complete and protocol.complete:
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                missing = [
                    f"{item.name}={item.count}/{item.minimum}"
                    for item in tracker.requirements
                    if not item.complete
                ]
                detail = ", ".join(missing) if missing else "fastboot boot did not finish"
                failure_reason = f"attempt timeout ({detail})"
                break

            if failure_reason is None:
                send_transmissions(protocol.poll(time.monotonic()))

            readers = [serial.fileno()]
            if not fastboot_done:
                readers.append(process.stdout.fileno())
            readable, _, _ = select.select(readers, [], [], min(0.25, remaining))
            if serial.fileno() in readable:
                data = serial.read()
                if data:
                    uart_buffer.feed(data)
            if not fastboot_done and process.stdout.fileno() in readable:
                try:
                    output = os.read(process.stdout.fileno(), 64 * 1024)
                except BlockingIOError:
                    output = b""
                if output:
                    fastboot_buffer.feed(output)

            fastboot_returncode = process.poll()
            if fastboot_returncode is not None and not fastboot_done:
                fastboot_done = True
                while True:
                    try:
                        output = os.read(process.stdout.fileno(), 64 * 1024)
                    except BlockingIOError:
                        break
                    if not output:
                        break
                    fastboot_buffer.feed(output)
                fastboot_buffer.flush()
    finally:
        if process.poll() is None:
            terminate_process(process)
        uart_buffer.flush()
        fastboot_buffer.flush()
        process.stdout.close()

    elapsed = time.monotonic() - start
    status = "passed" if failure_reason is None else "failed"
    reason = "all marker expectations satisfied" if failure_reason is None else failure_reason
    log.line("HOST", f"attempt {attempt}: {status}: {reason}")
    return AttemptResult(attempt, status, reason, elapsed, tracker.count_summary())


def send_sysrq_sequence(
    serial: SerialPort,
    dump_keys: Sequence[str],
    reboot: bool,
    args: argparse.Namespace,
    log: TimestampedLog,
) -> None:
    for key in dump_keys:
        log.line("HOST", f"sending serial BREAK + SysRq-{key} diagnostic request")
        serial.send_sysrq(key, args.break_seconds, args.sysrq_key_delay)
        capture_uart(serial, log, args.sysrq_command_delay)
    if reboot:
        log.line("HOST", "sending serial BREAK + SysRq-b emergency reboot")
        serial.send_sysrq("b", args.break_seconds, args.sysrq_key_delay)


def return_to_bootloader_between_attempts(
    result: AttemptResult,
    serial: SerialPort,
    fastboot: FastbootClient,
    args: argparse.Namespace,
    log: TimestampedLog,
) -> None:
    if result.status == "failed" and args.sysrq_reboot_on_failure:
        send_sysrq_sequence(serial, args.sysrq_dump, True, args, log)
    elif args.pocketboot_reboot_bootloader_between_attempts:
        fastboot.reboot_bootloader(args.fastboot_wait_timeout, serial, log)
    elif result.status == "passed" and args.sysrq_reboot_between_attempts:
        send_sysrq_sequence(serial, (), True, args, log)
    elif result.status == "passed" and args.between_attempt_command:
        log.line("HOST", "sending configured between-attempt serial command")
        serial.send_command(args.between_attempt_command)
    else:
        log.line("HOST", "failure recovery not configured; waiting for external fastboot return")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def validate_args(args: argparse.Namespace, parser: argparse.ArgumentParser) -> None:
    if not args.fastboot_serial.strip():
        parser.error("--fastboot-serial must not be empty")
    if not args.expected_product.strip():
        parser.error("--expected-product must not be empty")
    if args.attempts < 1:
        parser.error("--attempts must be at least 1")
    if args.timeout <= 0 or args.fastboot_wait_timeout <= 0:
        parser.error("timeouts must be positive")
    if args.pageflip_count < 0 or args.blank_cycles < 0:
        parser.error("marker counts must not be negative")
    if args.blank_cycles and args.backlight_sysfs is None:
        parser.error("--blank-cycles requires an exact --backlight-sysfs path")
    if args.blank_cycles and args.unblank_brightness is None:
        parser.error("--blank-cycles requires --unblank-brightness")
    if not args.blank_cycles and (args.backlight_sysfs or args.unblank_brightness is not None):
        parser.error("backlight blanking options require --blank-cycles")
    if args.unblank_brightness is not None and args.unblank_brightness <= 0:
        parser.error("--unblank-brightness must be positive")
    if (
        not math.isfinite(args.blank_hold_seconds)
        or not math.isfinite(args.unblank_hold_seconds)
        or args.blank_hold_seconds < 0
        or args.unblank_hold_seconds < 0
    ):
        parser.error("backlight hold times must be finite and non-negative")
    if args.backlight_sysfs is not None:
        try:
            validate_backlight_path(args.backlight_sysfs)
        except LabError as error:
            parser.error(str(error))
    if args.break_seconds <= 0 or args.sysrq_key_delay < 0 or args.sysrq_command_delay < 0:
        parser.error("SysRq timings are invalid")
    if args.sysrq_dump and not args.sysrq_reboot_on_failure:
        parser.error("--sysrq-dump requires --sysrq-reboot-on-failure")
    reboot_mechanisms = (
        args.pocketboot_reboot_bootloader_between_attempts,
        args.sysrq_reboot_between_attempts,
        args.between_attempt_command is not None,
    )
    if args.attempts > 1 and not any(reboot_mechanisms):
        parser.error(
            "repeated attempts require --pocketboot-reboot-bootloader-between-attempts, "
            "--sysrq-reboot-between-attempts, or --between-attempt-command"
        )
    if sum(bool(mechanism) for mechanism in reboot_mechanisms) > 1:
        parser.error(
            "between-attempt reboot mechanisms are mutually exclusive"
        )
    try:
        protocol = SerialCommandProtocol(serial_test_commands(args))
        if args.between_attempt_command is not None:
            encode_serial_command(args.between_attempt_command)
        marker_tracker(args, protocol.stage_count)
    except LabError as error:
        parser.error(str(error))


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Run ephemeral Crosshatch MSM DRM attempts while recording the SBU UART. "
            "The fastboot surface cannot write partitions."
        )
    )
    parser.add_argument("--image", required=True, type=Path, help="boot.img to boot ephemerally")
    parser.add_argument(
        "--fastboot-serial",
        required=True,
        help="explicit Android fastboot device serial; devices are never auto-selected",
    )
    parser.add_argument(
        "--serial-port",
        required=True,
        type=Path,
        help="explicit SBU UART tty, preferably /dev/serial/by-id/...",
    )
    parser.add_argument("--expected-product", default="crosshatch")
    parser.add_argument("--fastboot", default="fastboot", help="fastboot executable")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--attempts", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=120.0, help="seconds per attempt")
    parser.add_argument("--fastboot-wait-timeout", type=float, default=90.0)
    parser.add_argument("--log-dir", type=Path, default=Path("target/crosshatch-drm-lab"))
    parser.add_argument("--ready-marker", default=DEFAULT_READY_MARKER, help="UART regex")
    parser.add_argument("--expect", action="append", default=[], metavar="REGEX")
    parser.add_argument("--pageflip-marker", default=DEFAULT_PAGEFLIP_MARKER, help="UART regex")
    parser.add_argument("--pageflip-count", type=int, default=0)
    parser.add_argument("--blank-marker", default=DEFAULT_BLANK_MARKER, help="UART regex")
    parser.add_argument("--unblank-marker", default=DEFAULT_UNBLANK_MARKER, help="UART regex")
    parser.add_argument("--blank-cycles", type=int, default=0)
    parser.add_argument(
        "--backlight-sysfs",
        metavar="/sys/class/backlight/DEVICE",
        help="exact target DCS-backlight sysfs directory used for opt-in blank cycles",
    )
    parser.add_argument(
        "--unblank-brightness",
        type=int,
        help="positive brightness restored after each opt-in blank",
    )
    parser.add_argument("--blank-hold-seconds", type=float, default=1.0)
    parser.add_argument("--unblank-hold-seconds", type=float, default=1.0)
    parser.add_argument("--failure-marker", action="append", default=[], metavar="REGEX")
    parser.add_argument("--no-default-failure-markers", action="store_true")
    parser.add_argument(
        "--serial-command",
        action="append",
        default=[],
        metavar="COMMAND",
        help="explicit shell command sent after the ready marker; repeatable",
    )
    parser.add_argument(
        "--between-attempt-command",
        help="explicit serial shell command used to return to fastboot between attempts",
    )
    parser.add_argument(
        "--pocketboot-reboot-bootloader-between-attempts",
        action="store_true",
        help=(
            "after each non-final attempt, verify Pocketboot product/serial/compatible "
            "and issue standard fastboot reboot bootloader"
        ),
    )
    parser.add_argument(
        "--sysrq-reboot-on-failure",
        action="store_true",
        help="after a failed attempt, send serial BREAK then SysRq-b",
    )
    parser.add_argument(
        "--sysrq-reboot-between-attempts",
        action="store_true",
        help="send serial BREAK then SysRq-b after each non-final successful attempt",
    )
    parser.add_argument(
        "--sysrq-dump",
        action="append",
        choices=SYSRQ_DUMP_KEYS,
        default=[],
        metavar="KEY",
        help="SysRq diagnostic key sent before failure recovery; repeatable",
    )
    parser.add_argument("--break-seconds", type=float, default=0.25)
    parser.add_argument("--sysrq-key-delay", type=float, default=0.1)
    parser.add_argument("--sysrq-command-delay", type=float, default=0.5)
    parser.add_argument(
        "--allow-locked",
        action="store_true",
        help="explicitly skip proof that the selected bootloader is unlocked",
    )
    parser.add_argument("--quiet", action="store_true", help="write UART only to the log")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the planned commands without touching fastboot or the tty",
    )
    return parser


def dry_run(args: argparse.Namespace) -> int:
    fastboot = FastbootClient(args.fastboot, args.fastboot_serial)
    protocol = SerialCommandProtocol(serial_test_commands(args))
    print("dry-run: no fastboot command will run and no serial port will be opened")
    print(f"verify product: {shlex.join(fastboot.getvar_command('product'))}")
    if not args.allow_locked:
        print(f"verify unlocked: {shlex.join(fastboot.getvar_command('unlocked'))}")
    print(f"UART: {args.serial_port} at {args.baud} baud")
    print(f"panel-prepared marker: {DEFAULT_PANEL_PREPARED_MARKER!r}")
    print(f"panel-enabled marker: {DEFAULT_PANEL_ENABLED_MARKER!r}")
    print(f"ready marker: {args.ready_marker!r}")
    if args.pageflip_count:
        print(f"page-flip expectation: {args.pageflip_count} x {args.pageflip_marker!r}")
        print(
            "page-flip result: "
            f"requested={args.pageflip_count} completed={args.pageflip_count}"
        )
    if args.blank_cycles:
        print(
            "DCS backlight cycles: "
            f"{args.blank_cycles} via {args.backlight_sysfs}, restore={args.unblank_brightness}"
        )
    if protocol.stage_count:
        print(
            "serial shell protocol: echo-safe shell ACK, then "
            f"{protocol.stage_count} ACK-gated stage(s); "
            f"{SERIAL_COMMAND_MAX_BYTES}-byte UTF-8 line budget"
        )
    for attempt in range(1, args.attempts + 1):
        print(f"attempt {attempt}: {shlex.join(fastboot.boot_command(args.image))}")
        if attempt < args.attempts:
            if args.pocketboot_reboot_bootloader_between_attempts:
                for name in ("product", "serialno", "compatible"):
                    print(
                        "between attempts, verify Pocketboot identity: "
                        f"{shlex.join(fastboot.getvar_command(name))}"
                    )
                print(
                    "between attempts: "
                    f"{shlex.join(fastboot.reboot_bootloader_command())}"
                )
            elif args.sysrq_reboot_between_attempts:
                print("between attempts: serial BREAK + SysRq-b")
            else:
                print("between attempts: configured serial shell command")
    return 0


def run(args: argparse.Namespace) -> int:
    if not args.image.is_file():
        raise LabError(f"boot image is not a regular file: {args.image}")
    executable = shutil.which(args.fastboot)
    if executable is None:
        raise LabError(f"fastboot executable not found: {args.fastboot}")
    serial_path = validate_serial_path(args.serial_port)
    image = args.image.resolve(strict=True)

    args.log_dir.mkdir(parents=True, exist_ok=True)
    session_id = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    log_path = args.log_dir / f"{session_id}.uart.log"
    summary_path = args.log_dir / f"{session_id}.summary.json"
    log = TimestampedLog(log_path, args.quiet)
    results: list[AttemptResult] = []
    fatal_error: str | None = None
    image_sha256: str | None = None
    try:
        image_sha256 = sha256_file(image)
        log.line("HOST", f"image={image.name} size={image.stat().st_size} sha256={image_sha256}")
        fastboot = FastbootClient(executable, args.fastboot_serial)
        with SerialPort(serial_path, args.baud) as serial:
            log.line("HOST", f"SBU UART opened at {args.baud} baud")
            for attempt in range(1, args.attempts + 1):
                fastboot.wait_and_verify(
                    args.expected_product,
                    args.allow_locked,
                    args.fastboot_wait_timeout,
                    log,
                    serial,
                )
                result = run_attempt(attempt, serial, fastboot, image, args, log)
                results.append(result)

                if attempt == args.attempts:
                    if result.status == "failed" and args.sysrq_reboot_on_failure:
                        send_sysrq_sequence(serial, args.sysrq_dump, True, args, log)
                    continue
                return_to_bootloader_between_attempts(
                    result,
                    serial,
                    fastboot,
                    args,
                    log,
                )
    except (LabError, OSError) as error:
        fatal_error = str(error)
        log.line("HOST", f"fatal lab error: {fatal_error}")
    finally:
        log.close()

    summary = {
        "format": 1,
        "image": {"name": image.name, "sha256": image_sha256},
        "expected_product": args.expected_product,
        "log": log_path.name,
        "fatal_error": fatal_error,
        "attempts": [dataclasses.asdict(result) for result in results],
    }
    summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(f"UART log: {log_path}")
    print(f"summary:  {summary_path}")
    if fatal_error is not None:
        raise LabError(fatal_error)
    return 0 if len(results) == args.attempts and all(r.status == "passed" for r in results) else 1


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    validate_args(args, parser)
    if args.dry_run:
        return dry_run(args)
    try:
        return run(args)
    except (LabError, OSError) as error:
        print(f"crosshatch DRM lab error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
