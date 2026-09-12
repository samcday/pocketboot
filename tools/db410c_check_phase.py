#!/usr/bin/env python3
"""Check one running MSM8916 kernel through fastboot, writing only target RAM.

Requires artifacts from db410c_kexec.py plus the separately prepared
coherency-install scripts and their manifest. Does not boot or flash anything.
Defaults to DB410c identity; pass --compatible explicitly for another board.
Keep an independent UART capture when available.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serial", required=True)
    parser.add_argument("--compatible", default="qcom,apq8016-sbc")
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--marker", required=True, help="expected pocketboot.lab value")
    args = parser.parse_args()
    records = json.loads((args.artifacts / "manifest.json").read_text())["artifacts"]
    records.update(json.loads((args.artifacts / "coherency-workload-manifest.json").read_text()))
    args.output.mkdir(parents=True, exist_ok=False)

    def verify(path):
        key = str(path.relative_to(args.artifacts))
        expected = records[key]
        data = path.read_bytes()
        if len(data) != expected["bytes"] or hashlib.sha256(data).hexdigest() != expected["sha256"]:
            raise ValueError(f"artifact hash mismatch: {path}")

    def fb(arguments, check=True):
        if arguments[0] == "oem" and len(("oem " + " ".join(arguments[1:])).encode()) > 64:
            raise ValueError("fastboot command exceeds 64 bytes")
        command = ["fastboot", "-s", args.serial, *map(str, arguments)]
        with (args.output / "fastboot.log").open("ab") as log:
            log.write((repr(command) + "\n").encode())
            log.flush()
            try:
                result = subprocess.run(command, capture_output=True, timeout=35)
            except subprocess.TimeoutExpired as error:
                log.write((error.stdout or b"") + (error.stderr or b"") + b"TIMEOUT\n")
                raise
            log.write(result.stdout + result.stderr + f"exit={result.returncode}\n".encode())
        if check and result.returncode:
            raise RuntimeError((result.stdout + result.stderr).decode(errors="replace"))
        return result

    for key, expected in [("product", "pocketboot"), ("serialno", args.serial),
                          ("compatible", args.compatible), ("is-userspace", "yes")]:
        identity = fb(["getvar", key])
        if f"{key}: {expected}\n".encode() not in identity.stdout + identity.stderr:
            raise RuntimeError(f"unexpected device identity: {key}")

    def shell(path, result_name=None):
        verify(path)
        fb(["stage", path])
        result = fb(["oem", "shell-staged"], check=False)
        if result_name or result.returncode:
            destination = args.output / (result_name or f"failed-{path.parent.name}-{path.name}.log")
            fb(["get_staged", destination])
        if result.returncode:
            raise RuntimeError(f"target script failed: {path}; see {destination}")

    for directory in ["workload-v2-install", "coherency-install"]:
        scripts = sorted((args.artifacts / directory).glob("*.sh"))
        if not scripts:
            raise ValueError(f"missing installation scripts: {directory}")
        for script in scripts:
            shell(script)
    shell(args.artifacts / "snapshot-v2.sh", "snapshot.log")
    snapshot = (args.output / "snapshot.log").read_text()
    if f"pocketboot.lab={args.marker}" not in snapshot.split() or "online=0-3\n" not in snapshot:
        raise RuntimeError("unexpected boot marker or CPU online set")
    workers = re.findall(r"^cpu=(\d+) start_cpu=(\d+) end_cpu=(\d+) hash=([0-9a-f]+) cpu_seconds=([0-9.]+) PASS$", snapshot, re.M)
    if (len(workers) != 4 or {row[0] for row in workers} != {"0", "1", "2", "3"}
            or any(cpu != start or cpu != end or digest != "292d74d0d0222325" or float(elapsed) <= .02
                   for cpu, start, end, digest, elapsed in workers)
            or "SMP_WORK_V2 " not in snapshot or "SMP_WORK_PASS all_four_cpus" not in snapshot):
        raise RuntimeError("four measured CPU workloads were not proved")
    shell(args.artifacts / "coherency-run.sh", "coherency.log")
    coherent = (args.output / "coherency.log").read_text()
    if "SMP_COHERENCY_PASS all_four_cpus_shared_memory_and_migration" not in coherent:
        raise RuntimeError("shared-memory test did not pass")
    for command, filename in [("cat:/sys/firmware/fdt", "live.dtb"), ("dmesg", "dmesg.log")]:
        fb(["oem", command])
        fb(["get_staged", args.output / filename])
    for line in (snapshot + coherent).splitlines():
        if line.startswith(("cmdline=", "online=", "cpu=", "SMP_", "phase=")):
            print(line)
    print(f"MSM8916_PHASE_PASS {args.marker}; evidence: {args.output}")


if __name__ == "__main__":
    main()
