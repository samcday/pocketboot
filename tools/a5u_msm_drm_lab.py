#!/usr/bin/env python3
"""Ephemerally boot and prove the A5U's native MSM8916 DRM display path.

The starting target must already be Pocketboot userspace fastboot.  This tool
identity-gates that exact target, uses only ``fastboot boot``, and retrieves
read-only diagnostics from the newly booted Pocketboot instance.  It has no
flash, erase, reboot, continue, or partition command surface.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import json
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import time
from typing import Sequence


EXPECTED_PRODUCT = "pocketboot"
EXPECTED_COMPATIBLE = "samsung,a5u-eur"
EXPECTED_USERSPACE = "yes"
EXPECTED_PAGE_FLIPS = 16
FASTBOOT_COMMAND_MAX_BYTES = 64
INVENTORY_HEADER = "POCKETBOOT_A5U_DRM_INVENTORY_V1"
ACCEPTED_MSM_DRM_DRIVERS = frozenset(("msm", "msm-kms"))

# Keep this command read-only.  Each record is a three-column TSV line so the
# host can relate a DSI connector to the DRM card whose debugfs name is msm.
INVENTORY_SCRIPT = " ".join(
    (
        "set -eu;",
        f"printf '%s\\n' '{INVENTORY_HEADER}';",
        "printf 'cmdline\\t/proc/cmdline\\t'; cat /proc/cmdline;",
        "for pb_p in /sys/kernel/debug/dri/*/name; do",
        "[ -r \"$pb_p\" ] || continue;",
        "pb_b=${pb_p%/name}; pb_b=${pb_b##*/};",
        "case \"$pb_b\" in ''|*[!0-9]*) continue;; esac;",
        "printf 'dri-name\\t%s\\t' \"$pb_p\"; cat \"$pb_p\";",
        "done;",
        "for pb_p in /sys/class/drm/card[0-9]*; do",
        "[ -d \"$pb_p\" ] || continue; pb_b=${pb_p##*/};",
        "case \"$pb_b\" in *-*) continue;; esac;",
        "pb_d=; [ ! -L \"$pb_p/device/driver\" ] || pb_d=$(readlink \"$pb_p/device/driver\" || true);",
        "printf 'card-driver\\t%s\\t%s\\n' \"$pb_p\" \"${pb_d##*/}\";",
        "done;",
        "for pb_p in /sys/class/drm/card[0-9]*-DSI-*; do",
        "[ -d \"$pb_p\" ] || continue;",
        "for pb_f in status enabled; do",
        "[ -r \"$pb_p/$pb_f\" ] || continue; pb_v=$(cat \"$pb_p/$pb_f\");",
        "printf 'connector-%s\\t%s\\t%s\\n' \"$pb_f\" \"$pb_p\" \"$pb_v\";",
        "done;",
        "[ ! -r \"$pb_p/modes\" ] || while IFS= read -r pb_v; do",
        "printf 'connector-mode\\t%s\\t%s\\n' \"$pb_p\" \"$pb_v\";",
        "done < \"$pb_p/modes\";",
        "done",
    )
)

FATAL_DMESG_EXPRESSIONS = (
    r"Kernel panic - not syncing",
    r"Internal error: Oops",
    r"(?:soft|hard) LOCKUP",
    r"pocketboot::ui: UI thread exited",
)


class LabError(Exception):
    """An expected, user-facing proof failure."""


class SafetyError(LabError):
    """The explicit target or requested operation failed a safety gate."""


class CommandError(LabError):
    """A host fastboot command failed."""


class HostLog:
    def __init__(self, path: Path, quiet: bool) -> None:
        self.file = path.open("x", encoding="utf-8", buffering=1)
        self.quiet = quiet

    def close(self) -> None:
        self.file.close()

    def line(self, source: str, value: str) -> None:
        timestamp = dt.datetime.now(dt.timezone.utc).isoformat(timespec="milliseconds")
        rendered = f"{timestamp} {source:<8} {value.rstrip()}"
        self.file.write(rendered + "\n")
        if not self.quiet:
            print(rendered, flush=True)


def getvar_value(output: str, name: str) -> str | None:
    expression = re.compile(
        rf"(?:\(bootloader\)\s*)?{re.escape(name)}\s*:\s*(\S+)",
        re.IGNORECASE,
    )
    match = expression.search(output)
    return match.group(1) if match else None


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def validate_explicit_serial(value: str) -> str:
    if not value or value.strip() != value:
        raise SafetyError("fastboot serial must be explicit, non-empty, and unpadded")
    if any(ord(character) < 0x20 or ord(character) == 0x7F for character in value):
        raise SafetyError("fastboot serial contains a control character")
    return value


def validate_image(path: Path) -> Path:
    try:
        resolved = path.resolve(strict=True)
        status = resolved.stat()
    except OSError as error:
        raise SafetyError(f"cannot resolve boot image {path}: {error}") from error
    if not resolved.is_file():
        raise SafetyError(f"boot image is not a regular file: {path}")
    if status.st_size == 0:
        raise SafetyError(f"boot image is empty: {path}")
    return resolved


class CommandRunner:
    def __init__(self, log: HostLog) -> None:
        self.log = log

    def run(self, command: Sequence[str], timeout: float) -> tuple[int, str]:
        self.log.line("HOST", f"run {shlex.join(command)}")
        try:
            result = subprocess.run(
                command,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            output = error.stdout or ""
            if isinstance(output, bytes):
                output = output.decode("utf-8", errors="replace")
            for line in output.splitlines():
                self.log.line("FASTBOOT", line)
            message = f"command timed out after {timeout:g}s"
            self.log.line("FASTBOOT", message)
            return 124, "\n".join(part for part in (output.rstrip(), message) if part)
        for line in result.stdout.splitlines():
            self.log.line("FASTBOOT", line)
        return result.returncode, result.stdout


class FastbootClient:
    """The complete fastboot surface available to this runner."""

    def __init__(self, executable: str, serial: str, runner: CommandRunner | None = None) -> None:
        self.executable = executable
        self.serial = serial
        self.runner = runner

    def getvar_command(self, name: str) -> list[str]:
        return [self.executable, "-s", self.serial, "getvar", name]

    def boot_command(self, image: Path) -> list[str]:
        return [self.executable, "-s", self.serial, "boot", str(image)]

    def oem_command(self, command: str) -> list[str]:
        packet = f"oem {command}".encode("utf-8")
        if len(packet) > FASTBOOT_COMMAND_MAX_BYTES:
            raise SafetyError(
                f"fastboot OEM command is {len(packet)} bytes; "
                f"packet limit is {FASTBOOT_COMMAND_MAX_BYTES}"
            )
        return [self.executable, "-s", self.serial, "oem", command]

    def stage_command(self, source: Path) -> list[str]:
        return [self.executable, "-s", self.serial, "stage", str(source)]

    def get_staged_command(self, destination: Path) -> list[str]:
        return [self.executable, "-s", self.serial, "get_staged", str(destination)]

    def _run(self, command: Sequence[str], timeout: float) -> tuple[int, str]:
        if self.runner is None:
            raise RuntimeError("FastbootClient has no command runner")
        return self.runner.run(command, timeout)

    def getvar(self, name: str, timeout: float) -> tuple[int, str]:
        return self._run(self.getvar_command(name), timeout)

    def boot(self, image: Path, timeout: float) -> None:
        returncode, output = self._run(self.boot_command(image), timeout)
        if returncode != 0:
            raise CommandError(
                f"transient fastboot boot failed: {output.strip() or f'exit status {returncode}'}"
            )

    def stage_oem(self, command: str, timeout: float) -> None:
        returncode, output = self._run(self.oem_command(command), timeout)
        if returncode != 0:
            raise CommandError(
                f"fastboot oem command failed: {output.strip() or f'exit status {returncode}'}"
            )

    def stage_file(self, source: Path, timeout: float) -> None:
        returncode, output = self._run(self.stage_command(source), timeout)
        if returncode != 0:
            raise CommandError(
                f"fastboot RAM staging failed: {output.strip() or f'exit status {returncode}'}"
            )

    def fetch_staged(self, destination: Path, timeout: float) -> None:
        temporary = destination.with_name(destination.name + ".part")
        if temporary.exists():
            temporary.unlink()
        returncode, output = self._run(self.get_staged_command(temporary), timeout)
        if returncode != 0:
            temporary.unlink(missing_ok=True)
            raise CommandError(
                f"fastboot get_staged failed: {output.strip() or f'exit status {returncode}'}"
            )
        if not temporary.is_file():
            raise CommandError("fastboot get_staged succeeded without creating its destination")
        temporary.replace(destination)


IDENTITY_REQUIREMENTS = (
    ("product", EXPECTED_PRODUCT),
    ("serialno", None),
    ("compatible", EXPECTED_COMPATIBLE),
    ("is-userspace", EXPECTED_USERSPACE),
)


def verify_identity(client: FastbootClient, timeout: float) -> dict[str, str]:
    """Prove the explicitly addressed device is the expected Pocketboot A5U."""

    observed: dict[str, str] = {}
    for name, fixed_expected in IDENTITY_REQUIREMENTS:
        expected = client.serial if fixed_expected is None else fixed_expected
        returncode, output = client.getvar(name, timeout)
        value = getvar_value(output, name)
        if returncode != 0 or value is None:
            raise SafetyError(
                f"could not prove target {name}: {output.strip() or f'exit status {returncode}'}"
            )
        if value.casefold() != expected.casefold():
            raise SafetyError(
                f"target {name} is {value!r}, expected {expected!r}; refusing operation"
            )
        observed[name] = value
    return observed


def wait_for_pocketboot(
    client: FastbootClient,
    timeout: float,
    getvar_timeout: float,
    poll_interval: float,
) -> dict[str, str]:
    deadline = time.monotonic() + timeout
    last_output = "device did not answer"
    while time.monotonic() < deadline:
        remaining = max(0.1, deadline - time.monotonic())
        returncode, output = client.getvar("product", min(getvar_timeout, remaining))
        product = getvar_value(output, "product")
        last_output = output.strip() or f"fastboot exited {returncode}"
        if returncode == 0 and product is not None:
            if product.casefold() != EXPECTED_PRODUCT.casefold():
                raise SafetyError(
                    f"explicit fastboot target reappeared as product {product!r}, "
                    f"expected {EXPECTED_PRODUCT!r}"
                )
            return verify_identity(client, min(getvar_timeout, remaining))
        time.sleep(min(poll_interval, remaining))
    raise LabError(f"timed out waiting for booted Pocketboot A5U: {last_output}")


@dataclasses.dataclass(frozen=True)
class InventoryRecord:
    kind: str
    path: str
    value: str


@dataclasses.dataclass(frozen=True)
class DrmProof:
    card: str
    connector: str
    driver: str
    mode: str
    platform_driver: str


def parse_inventory(text: str) -> list[InventoryRecord]:
    lines = text.splitlines()
    if not lines or lines[0] != INVENTORY_HEADER:
        raise LabError(f"DRM inventory is missing exact {INVENTORY_HEADER} header")
    known_kinds = {
        "cmdline",
        "dri-name",
        "card-driver",
        "connector-status",
        "connector-enabled",
        "connector-mode",
    }
    records: list[InventoryRecord] = []
    for line_number, line in enumerate(lines[1:], start=2):
        fields = line.split("\t", 2)
        if len(fields) != 3 or fields[0] not in known_kinds:
            raise LabError(f"malformed DRM inventory record on line {line_number}: {line!r}")
        records.append(InventoryRecord(*fields))
    return records


def validate_inventory(text: str) -> DrmProof:
    records = parse_inventory(text)
    cmdlines = [record.value for record in records if record.kind == "cmdline"]
    if len(cmdlines) != 1:
        raise LabError(f"DRM inventory contains {len(cmdlines)} kernel cmdlines, expected one")
    required_parameter = f"pocketboot.drm_page_flips={EXPECTED_PAGE_FLIPS}"
    if required_parameter not in cmdlines[0].split():
        raise LabError(f"booted kernel cmdline does not contain {required_parameter}")

    drm_names: dict[str, str] = {}
    platform_drivers: dict[str, str] = {}
    connectors: dict[tuple[str, str], dict[str, list[str]]] = {}
    for record in records:
        if record.kind == "dri-name":
            match = re.fullmatch(r"/sys/kernel/debug/dri/([0-9]+)/name", record.path)
            if match is None or not record.value.strip():
                raise LabError(f"invalid DRM debugfs name record: {record}")
            drm_names[f"card{match.group(1)}"] = record.value.split()[0]
        elif record.kind == "card-driver":
            match = re.fullmatch(r"/sys/class/drm/(card[0-9]+)", record.path)
            if match is None:
                raise LabError(f"invalid DRM card driver record: {record}")
            platform_drivers[match.group(1)] = record.value
        elif record.kind.startswith("connector-"):
            match = re.fullmatch(
                r"/sys/class/drm/(card[0-9]+)-(DSI-[0-9]+)", record.path
            )
            if match is None:
                raise LabError(f"invalid DSI connector record: {record}")
            key = (match.group(1), match.group(2))
            field = record.kind.removeprefix("connector-")
            connectors.setdefault(key, {}).setdefault(field, []).append(record.value)

    non_msm = {
        card: driver
        for card, driver in drm_names.items()
        if driver not in ACCEPTED_MSM_DRM_DRIVERS
    }
    if non_msm:
        rendered = ", ".join(f"{card}={driver}" for card, driver in sorted(non_msm.items()))
        raise LabError(f"non-MSM DRM card present: {rendered}")

    for (card, connector), fields in sorted(connectors.items()):
        driver = drm_names.get(card)
        if driver not in ACCEPTED_MSM_DRM_DRIVERS:
            continue
        if fields.get("status") != ["connected"]:
            continue
        if fields.get("enabled") != ["enabled"]:
            continue
        if "720x1280" not in fields.get("mode", []):
            continue
        return DrmProof(
            card=card,
            connector=connector,
            driver=driver,
            mode="720x1280",
            platform_driver=platform_drivers.get(card, ""),
        )
    raise LabError(
        "no enabled, connected 720x1280 DSI connector belongs to a native msm/msm-kms DRM card"
    )


def page_flip_sequences(dmesg: str) -> list[int]:
    return [
        int(value)
        for value in re.findall(r"POCKETBOOT_DRM_PAGE_FLIP sequence=([0-9]+)\b", dmesg)
    ]


def dmesg_has_complete_page_flips(dmesg: str) -> bool:
    return (
        page_flip_sequences(dmesg) == list(range(1, EXPECTED_PAGE_FLIPS + 1))
        and re.search(
            rf"POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested={EXPECTED_PAGE_FLIPS} "
            rf"completed={EXPECTED_PAGE_FLIPS}(?![0-9])",
            dmesg,
        )
        is not None
    )


def validate_dmesg(dmesg: str, drm: DrmProof) -> dict[str, object]:
    for expression in FATAL_DMESG_EXPRESSIONS:
        if re.search(expression, dmesg, re.IGNORECASE):
            raise LabError(f"dmesg contains fatal marker {expression!r}")

    ready = re.compile(
        rf"POCKETBOOT_DRM_READY\b[^\n]*path=/dev/dri/{re.escape(drm.card)}\b"
        rf"[^\n]*connector={re.escape(drm.connector)}\b"
        r"[^\n]*width=720\b[^\n]*height=1280\b"
    )
    if ready.search(dmesg) is None:
        raise LabError(
            f"dmesg lacks a 720x1280 POCKETBOOT_DRM_READY marker for "
            f"/dev/dri/{drm.card} {drm.connector}"
        )

    start = (
        f"POCKETBOOT_DRM_PAGE_FLIP_TEST_START requested={EXPECTED_PAGE_FLIPS}"
    )
    if start not in dmesg:
        raise LabError(f"dmesg lacks exact startup marker {start!r}")
    sequences = page_flip_sequences(dmesg)
    expected = list(range(1, EXPECTED_PAGE_FLIPS + 1))
    if sequences != expected:
        raise LabError(f"completed page-flip sequence is {sequences!r}, expected {expected!r}")
    result = (
        f"POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested={EXPECTED_PAGE_FLIPS} "
        f"completed={EXPECTED_PAGE_FLIPS}"
    )
    if re.search(re.escape(result) + r"(?![0-9])", dmesg) is None:
        raise LabError(f"dmesg lacks exact successful page-flip result {result!r}")
    return {
        "ready": True,
        "page_flips_requested": EXPECTED_PAGE_FLIPS,
        "page_flips_completed": len(sequences),
        "sequences": sequences,
    }


def stage_and_fetch(
    client: FastbootClient,
    oem_command: str,
    destination: Path,
    timeout: float,
) -> str:
    client.stage_oem(oem_command, timeout)
    client.fetch_staged(destination, timeout)
    try:
        return destination.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise LabError(f"cannot read staged diagnostic {destination}: {error}") from error


def stage_script_and_fetch(
    client: FastbootClient,
    script: Path,
    destination: Path,
    timeout: float,
) -> str:
    """Stage an exact script in RAM, execute it, then retrieve its output."""

    client.stage_file(script, timeout)
    client.stage_oem("shell-staged", timeout)
    client.fetch_staged(destination, timeout)
    try:
        return destination.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise LabError(f"cannot read staged diagnostic {destination}: {error}") from error


def wait_for_page_flip_dmesg(
    client: FastbootClient,
    destination: Path,
    timeout: float,
    command_timeout: float,
    poll_interval: float,
) -> str:
    deadline = time.monotonic() + timeout
    last_dmesg = ""
    while time.monotonic() < deadline:
        last_dmesg = stage_and_fetch(client, "dmesg", destination, command_timeout)
        if dmesg_has_complete_page_flips(last_dmesg):
            return last_dmesg
        time.sleep(min(poll_interval, max(0.0, deadline - time.monotonic())))
    sequences = page_flip_sequences(last_dmesg)
    raise LabError(
        f"timed out waiting for {EXPECTED_PAGE_FLIPS} completed page flips; "
        f"observed sequences {sequences!r}"
    )


def utc_run_name() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")


def create_output_directory(requested: Path | None) -> Path:
    path = requested or Path("target/a5u-msm-drm-lab") / utc_run_name()
    try:
        path.mkdir(parents=True, exist_ok=False)
    except OSError as error:
        raise SafetyError(f"cannot create new evidence directory {path}: {error}") from error
    return path.resolve()


def dry_run(args: argparse.Namespace) -> int:
    client = FastbootClient(args.fastboot, args.fastboot_serial)
    print("A5U MSM DRM proof plan (no commands executed)")
    for name, _expected in IDENTITY_REQUIREMENTS:
        print(f"identity: {shlex.join(client.getvar_command(name))}")
    print(f"transient boot: {shlex.join(client.boot_command(args.image))}")
    print("post-boot: repeat exact product/serialno/compatible/is-userspace identity gates")
    print(f"stage dmesg: {shlex.join(client.oem_command('dmesg'))}")
    script = Path("EVIDENCE_DIR/drm-inventory.sh")
    print(f"stage read-only DRM inventory script in RAM: {shlex.join(client.stage_command(script))}")
    print(f"run staged inventory: {shlex.join(client.oem_command('shell-staged'))}")
    print("retrieve each diagnostic with: fastboot -s SERIAL get_staged HOST_PATH")
    print(
        "assert: native msm/msm-kms card; enabled connected DSI 720x1280; "
        "POCKETBOOT_DRM_READY; 16/16 ordered page flips"
    )
    print("partition/storage mutations: none; image and script staging are RAM-only")
    return 0


def run(args: argparse.Namespace) -> int:
    validate_explicit_serial(args.fastboot_serial)
    executable = shutil.which(args.fastboot)
    if executable is None:
        raise SafetyError(f"fastboot executable not found: {args.fastboot}")
    image = validate_image(args.image)
    output = create_output_directory(args.output_dir)
    log = HostLog(output / "host.log", args.quiet)
    summary: dict[str, object] = {
        "status": "running",
        "fastboot_serial": args.fastboot_serial,
        "expected_product": EXPECTED_PRODUCT,
        "expected_compatible": EXPECTED_COMPATIBLE,
        "image": str(image),
        "image_sha256": sha256_file(image),
        "dmesg": str(output / "dmesg.txt"),
        "inventory": str(output / "drm-inventory.tsv"),
        "inventory_script": str(output / "drm-inventory.sh"),
    }
    summary_path = output / "summary.json"
    try:
        client = FastbootClient(executable, args.fastboot_serial, CommandRunner(log))
        log.line("HOST", "verifying pre-boot Pocketboot A5U identity")
        summary["preboot_identity"] = verify_identity(client, args.command_timeout)
        log.line("HOST", f"verified image sha256={summary['image_sha256']}")
        log.line("HOST", "issuing identity-gated ephemeral fastboot boot")
        client.boot(image, args.boot_command_timeout)

        log.line("HOST", "waiting for the explicitly addressed Pocketboot A5U to reappear")
        summary["postboot_identity"] = wait_for_pocketboot(
            client,
            args.boot_timeout,
            args.command_timeout,
            args.poll_interval,
        )

        dmesg = wait_for_page_flip_dmesg(
            client,
            output / "dmesg.txt",
            args.proof_timeout,
            args.command_timeout,
            args.poll_interval,
        )
        inventory_script = output / "drm-inventory.sh"
        inventory_script.write_text(INVENTORY_SCRIPT + "\n", encoding="utf-8")
        summary["inventory_script_sha256"] = sha256_file(inventory_script)
        inventory = stage_script_and_fetch(
            client,
            inventory_script,
            output / "drm-inventory.tsv",
            args.command_timeout,
        )
        drm = validate_inventory(inventory)
        markers = validate_dmesg(dmesg, drm)
        summary["drm"] = dataclasses.asdict(drm)
        summary["markers"] = markers
        summary["status"] = "pass"
        summary["reason"] = "native MSM8916 DRM/DSI and 16 completed page flips proven"
        log.line(
            "PASS",
            f"{drm.driver} /dev/dri/{drm.card} {drm.connector} "
            f"{drm.mode}; {EXPECTED_PAGE_FLIPS}/{EXPECTED_PAGE_FLIPS} page flips",
        )
        return 0
    except LabError as error:
        summary["status"] = "fail"
        summary["reason"] = str(error)
        log.line("FAIL", str(error))
        raise
    finally:
        summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
        log.close()


def positive_float(value: str) -> float:
    try:
        parsed = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError(f"not a number: {value!r}") from error
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Ephemerally boot and prove Samsung A5U MSM8916 DRM/DSI"
    )
    result.add_argument("--image", required=True, type=Path, help="quiet A5U boot image")
    result.add_argument(
        "--fastboot-serial",
        required=True,
        help="exact A5U fastboot serial; devices are never auto-selected",
    )
    result.add_argument("--fastboot", default="fastboot", help="fastboot executable")
    result.add_argument(
        "--output-dir",
        type=Path,
        help="new evidence directory (default: target/a5u-msm-drm-lab/TIMESTAMP)",
    )
    result.add_argument("--boot-timeout", type=positive_float, default=180.0)
    result.add_argument("--boot-command-timeout", type=positive_float, default=180.0)
    result.add_argument("--proof-timeout", type=positive_float, default=60.0)
    result.add_argument("--command-timeout", type=positive_float, default=15.0)
    result.add_argument("--poll-interval", type=positive_float, default=1.0)
    result.add_argument("--quiet", action="store_true", help="write evidence without live output")
    result.add_argument(
        "--dry-run",
        action="store_true",
        help="print the exact command/proof plan without touching paths or USB",
    )
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        validate_explicit_serial(args.fastboot_serial)
        if args.dry_run:
            return dry_run(args)
        return run(args)
    except LabError as error:
        print(f"a5u-msm-drm-lab: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
