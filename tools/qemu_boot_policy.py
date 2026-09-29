#!/usr/bin/env python3
"""End-to-end QEMU checks for Pocketboot's autoboot / menu policy.

Build the image first:  cargo xtask qemu <KERNEL_TREE> --build-only
Then run:               python3 tools/qemu_boot_policy.py [scenario ...]

Needs qemu-system-aarch64, sgdisk, mkfs.vfat and mtools (mmd/mcopy). Each
scenario boots the xtask QEMU Image against a throwaway GPT disk whose ESP
holds BLS entries, drives volume-down over QMP where needed, and asserts on
the serial log (POCKETBOOT_BOOT_DECISION and friends). Logs land in
target/qemu-boot-policy/.
"""

import argparse
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IMAGE = os.path.join(ROOT, "target/kernel/qemu/aarch64-virt/arch/arm64/boot/Image")
WORK = os.path.join(ROOT, "target/qemu-boot-policy")
CONSOLE = "console=ttyAMA0 earlycon=pl011,mmio32,0x09000000 loglevel=7 pocketboot.log=info"
DECISION = re.compile(r'POCKETBOOT_BOOT_DECISION decision="(\w+)"(?: reason=([\w-]+))?')
GEN_START = "Booting Linux on physical CPU"
TIMEOUT = 120


def run(*args):
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def make_disk(name, files):
    """GPT disk with one ESP; files maps ESP paths to host paths or bytes."""
    disk = os.path.join(WORK, f"{name}.raw")
    esp = os.path.join(WORK, f"{name}.esp")
    for path in (disk, esp):
        if os.path.exists(path):
            os.remove(path)
    with open(esp, "wb") as f:
        f.truncate(48 << 20)
    run("mkfs.vfat", "-n", "PBESP", esp)
    run("mmd", "-i", esp, "::/loader", "::/loader/entries")
    for dest, src in files.items():
        if isinstance(src, (bytes, str)) and not (isinstance(src, str) and os.path.exists(src)):
            tmp = os.path.join(WORK, "payload.tmp")
            with open(tmp, "wb") as f:
                f.write(src.encode() if isinstance(src, str) else src)
            src = tmp
        run("mcopy", "-i", esp, src, f"::{dest}")
    with open(disk, "wb") as f:
        f.truncate(64 << 20)
    run("sgdisk", "--clear", "--new=1:2048:+48M", "--typecode=1:ef00", disk)
    with open(esp, "rb") as src, open(disk, "r+b") as dst:
        dst.seek(2048 * 512)
        shutil.copyfileobj(src, dst)
    os.remove(esp)
    return disk


def bls(title, linux, options):
    return f"title {title}\nlinux {linux}\noptions {options}\n"


class Qemu:
    def __init__(self, name, disk, append="", port=4460):
        self.log = os.path.join(WORK, f"{name}.log")
        self.port = port
        drive = []
        if disk:
            drive = ["-drive", f"if=none,id=pb,format=raw,file={disk},snapshot=on",
                     "-device", "virtio-blk-device,drive=pb"]
        self.proc = subprocess.Popen(
            ["qemu-system-aarch64", "-machine", "virt", "-cpu", "max", "-smp", "2",
             "-m", "512M", "-display", "none", "-monitor", "none",
             "-serial", f"file:{self.log}", "-no-reboot",
             "-global", "virtio-mmio.force-legacy=false",
             "-kernel", IMAGE, "-append", f"{CONSOLE} panic=1 {append}".strip(),
             *drive,
             "-device", "virtio-keyboard-device", "-device", "virtio-gpu-device",
             "-qmp", f"tcp:127.0.0.1:{port},server=on,wait=off"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.qmp = None

    def text(self):
        try:
            with open(self.log, errors="replace") as f:
                return f.read()
        except FileNotFoundError:
            return ""

    def generation(self, n):
        parts = self.text().split(GEN_START)
        return parts[n] if len(parts) > n else ""

    def wait(self, pattern, timeout=TIMEOUT, gen=None):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            text = self.generation(gen) if gen else self.text()
            match = re.search(pattern, text)
            if match:
                return match
            if self.proc.poll() is not None:
                break
            time.sleep(0.02)
        return None

    def connect(self):
        deadline = time.monotonic() + 10
        while True:
            try:
                sock = socket.create_connection(("127.0.0.1", self.port), timeout=2)
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.05)
        self.qmp = sock.makefile("rw")
        self.qmp.readline()
        self.command("qmp_capabilities")

    def command(self, name, arguments=None):
        msg = {"execute": name}
        if arguments:
            msg["arguments"] = arguments
        self.qmp.write(json.dumps(msg) + "\n")
        self.qmp.flush()
        while True:
            reply = json.loads(self.qmp.readline())
            if "return" in reply or "error" in reply:
                return reply

    def key(self, qcode, down):
        self.command("input-send-event", {"events": [
            {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": qcode}}}]})

    def stop(self):
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait()


class Result:
    def __init__(self, name):
        self.name = name
        self.checks = []

    def check(self, label, ok, detail=""):
        self.checks.append((label, bool(ok), detail))

    @property
    def passed(self):
        return all(ok for _, ok, _ in self.checks)


def decision(qemu, gen=1, timeout=TIMEOUT):
    match = qemu.wait(DECISION.pattern, timeout, gen=gen)
    return (match.group(1), match.group(2)) if match else (None, None)


def good_disk(name, entries=("pocketboot-gen2",)):
    files = {"/pocketboot-Image": IMAGE}
    for entry in entries:
        files[f"/loader/entries/{entry}.conf"] = bls(
            entry, "/pocketboot-Image", f"{CONSOLE} pocketboot.menu pocketboot.test={entry}")
    return make_disk(name, files)


def autoboot():
    r = Result("autoboot")
    q = Qemu("autoboot", good_disk("autoboot"))
    try:
        r.check("gen1 decides autoboot", decision(q) == ("autoboot", None))
        r.check("kexec starts", q.wait("Starting new kernel"))
        gen1 = q.generation(1)
        for marker in ("UI thread spawned", "USB gadget thread spawned",
                       "battery watcher thread spawned"):
            r.check(f"gen1 never logs '{marker}'", marker not in gen1)
        r.check("gen2 menu via pocketboot.menu", decision(q, gen=2) == ("menu", "cmdline"))
        start = re.search(r"\[\s*([\d.]+)\].*starting up", gen1)
        kexec = re.search(r"\[\s*([\d.]+)\] kexec_core: Starting new kernel", gen1)
        if start and kexec:
            r.check("gen1 starting up -> kexec", True,
                    f"{(float(kexec.group(1)) - float(start.group(1))) * 1000:.0f} ms")
    finally:
        q.stop()
    return r


def volume_down_hold():
    r = Result("volume-down-hold")
    q = Qemu("volume-down-hold", good_disk("volume-down-hold", ("b-first", "a-second")), port=4461)
    try:
        q.connect()
        # The guest drops input sent before virtio-input is up, so keep pressing.
        deadline = time.monotonic() + TIMEOUT
        while not DECISION.search(q.text()) and time.monotonic() < deadline:
            q.key("volumedown", True)
            time.sleep(0.05)
        r.check("decision is menu/volume-down", decision(q, timeout=1) == ("menu", "volume-down"))
        r.check("watcher logged break-in", "volume-down break-in detected" in q.text())
        time.sleep(2)
        q.key("volumedown", False)
        time.sleep(3)
        r.check("no kexec", "Starting new kernel" not in q.text())
        r.check("UI started", q.wait("starting frankenSlint UI", timeout=10))
    finally:
        q.stop()
    return r


def empty_disk():
    r = Result("empty-disk")
    disk = os.path.join(WORK, "empty.raw")
    with open(disk, "wb") as f:
        f.truncate(64 << 20)
    q = Qemu("empty-disk", disk, port=4462)
    try:
        r.check("decision is menu/no-bootable-entry",
                decision(q) == ("menu", "no-bootable-entry"))
        time.sleep(5)
        r.check("PID 1 still alive", q.proc.poll() is None and "Attempted to kill init" not in q.text())
    finally:
        q.stop()
    return r


def broken_default():
    r = Result("broken-default")
    disk = make_disk("broken-default", {
        "/not-a-kernel": bytes(65536),
        "/loader/entries/broken.conf": bls("broken kernel", "/not-a-kernel", CONSOLE),
    })
    q = Qemu("broken-default", disk, port=4463)
    try:
        r.check("autoboot attempted", decision(q) == ("autoboot", None))
        match = q.wait(r'decision="menu" reason=boot-failed entry="broken kernel" error="([^"]*)"')
        r.check("falls back to menu/boot-failed", match, match.group(1) if match else "")
        time.sleep(5)
        r.check("PID 1 still alive", q.proc.poll() is None and "Attempted to kill init" not in q.text())
        r.check("no kexec", "Starting new kernel" not in q.text())
    finally:
        q.stop()
    return r


def cmdline_menu():
    r = Result("cmdline-menu")
    q = Qemu("cmdline-menu", good_disk("cmdline-menu"), append="pocketboot.menu", port=4464)
    try:
        r.check("decision is menu/cmdline", decision(q) == ("menu", "cmdline"))
        r.check("discovery still completes", q.wait("boot discovery complete count=1"))
        time.sleep(3)
        r.check("no kexec", "Starting new kernel" not in q.text())
        r.check("no break-in watcher", "break-in watcher spawned" not in q.text())
    finally:
        q.stop()
    return r


SCENARIOS = {f.__name__.replace("_", "-"): f
             for f in (autoboot, volume_down_hold, empty_disk, broken_default, cmdline_menu)}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("scenarios", nargs="*", metavar="SCENARIO",
                        help=f"one of {', '.join(SCENARIOS)} (default: all)")
    args = parser.parse_args()
    unknown = set(args.scenarios) - set(SCENARIOS)
    if unknown:
        parser.error(f"unknown scenario(s): {', '.join(sorted(unknown))}")
    if not os.path.exists(IMAGE):
        sys.exit(f"missing {IMAGE}; run cargo xtask qemu <KERNEL_TREE> --build-only")
    os.makedirs(WORK, exist_ok=True)
    failed = False
    for name in args.scenarios or SCENARIOS:
        result = SCENARIOS[name]()
        print(f"{'PASS' if result.passed else 'FAIL'} {name}")
        for label, ok, detail in result.checks:
            print(f"  {'ok  ' if ok else 'FAIL'} {label}{f': {detail}' if detail else ''}")
        failed |= not result.passed
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
