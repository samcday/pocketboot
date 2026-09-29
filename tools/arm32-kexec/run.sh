#!/usr/bin/env bash
# Usage: bash tools/arm32-kexec/run.sh /path/to/linux [LLVM=-19 ...]
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
source_tree=$(realpath "${1:?usage: run.sh /path/to/linux [kernel make arguments...]}")
shift
cd "$root"
out="$root/target/arm32-kexec-qemu"
mkdir -p "$out/kernel"
qemu=${QEMU_SYSTEM_ARM:-qemu-system-arm}
seconds=${SMOKE_TIMEOUT:-60}

mode=${SMOKE_MODE:-all}
case "$mode" in all|build|run) ;; *) echo "SMOKE_MODE must be all, build, or run" >&2; exit 1 ;; esac
if [[ "$mode" != run ]]; then
export KBUILD_BUILD_USER=pocketboot KBUILD_BUILD_HOST=smoke
export KBUILD_BUILD_TIMESTAMP='Thu Jan 1 00:00:00 UTC 1970'
export KBUILD_BUILD_VERSION=1 SOURCE_DATE_EPOCH=0
kernel_make=(make -C "$source_tree" O="$out/kernel" ARCH=arm
    CROSS_COMPILE="${CROSS_COMPILE:-arm-linux-gnueabihf-}" "$@")
"${kernel_make[@]}" KCONFIG_ALLCONFIG="$root/tools/arm32-kexec/kernel.config" allnoconfig
# Fail early if an incompatible source tree silently drops essential options.
for option in ARCH_VIRT AEABI ARM_THUMB VFP KEXEC ARM_APPENDED_DTB BLK_DEV_INITRD RD_GZIP \
    BINFMT_ELF PROC_FS SYSFS DEVTMPFS SERIAL_AMBA_PL011_CONSOLE; do
    grep -qx "CONFIG_$option=y" "$out/kernel/.config" || {
        echo "Required CONFIG_$option=y missing from generated kernel config" >&2
        exit 1
    }
done
grep -qx '# CONFIG_SMP is not set' "$out/kernel/.config"
"${kernel_make[@]}" -j"${JOBS:-$(nproc)}" zImage
python3 tools/arm32-kexec/check-decompressor.py "$out/kernel/arch/arm/boot/compressed/vmlinux"

cargo build --locked --release --manifest-path tools/arm32-kexec/Cargo.toml \
    --example arm32-kexec-smoke --target armv7-unknown-linux-musleabihf \
    --target-dir "$out/rust"
fi
if [[ "$mode" == build ]]; then
    exit 0
fi

# Generate the exact machine's DTB. fdtput edits properties; no second FDT parser.
# Disconnect stdin: timeout's process group must not stop on terminal input.
timeout --kill-after=5s "${seconds}s" "$qemu" \
    -M "virt,dtb-randomness=off,dumpdtb=$out/live.dtb" \
    -cpu cortex-a15 -smp 1 -m 256M -nographic </dev/null
cp "$out/live.dtb" "$out/supplied.dtb"
fdtput -t x "$out/supplied.dtb" /memory@40000000 reg 0 50000000 0 08000000
fdtput "$out/supplied.dtb" /chosen pocketboot,supplied-dtb
cp "$out/live.dtb" "$out/appended.dtb"
fdtput -t x "$out/appended.dtb" /memory@40000000 reg 0 50000000 0 08000000
fdtput "$out/appended.dtb" /chosen pocketboot,stale-appended-dtb
cat "$out/kernel/arch/arm/boot/zImage" "$out/appended.dtb" > "$out/destination.zImage"
python3 tools/arm32-kexec/initramfs.py \
    "$out/rust/armv7-unknown-linux-musleabihf/release/examples/arm32-kexec-smoke" \
    "$out/destination.zImage" "$out/supplied.dtb" "$out"

for case in fallback supplied; do
    echo "=== ARM32 kexec: $case ==="
    # pipefail propagates timeout/QEMU failure. A PASS alone is insufficient:
    # PID 1 must also power off QEMU within the bound.
    timeout --kill-after=5s "${seconds}s" "$qemu" \
        -M virt -cpu cortex-a15 -smp 1 -m 256M -nographic -no-reboot \
        -kernel "$out/kernel/arch/arm/boot/zImage" \
        -initrd "$out/source.cpio.gz" \
        -append "console=ttyAMA0 rdinit=/init panic=-1 pocketboot.smoke.stage=source pocketboot.smoke.case=$case" \
        </dev/null 2>&1 | tee "$out/$case.log"
    grep -qx "ARM32-KEXEC-SMOKE: PASS case=$case" <(tr -d '\r' < "$out/$case.log")
    if grep -q 'ARM32-KEXEC-SMOKE: FAIL' "$out/$case.log"; then
        exit 1
    fi
done
echo "ARM32-KEXEC-SMOKE: PASS both cases"
