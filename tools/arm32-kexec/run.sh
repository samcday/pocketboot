#!/usr/bin/env bash
# Usage: bash tools/arm32-kexec/run.sh /path/to/linux [kernel make arguments...]
#        SMOKE_MODE=run bash tools/arm32-kexec/run.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
out="$root/target/arm32-kexec-qemu"
qemu=${QEMU_SYSTEM_ARM:-qemu-system-arm}
seconds=${SMOKE_TIMEOUT:-60}

mode=${SMOKE_MODE:-all}
case "$mode" in all|build|run) ;; *) echo "SMOKE_MODE must be all, build, or run" >&2; exit 1 ;; esac
if [[ "$mode" == run ]]; then
    # Replaying a container build on the host means the container's kernel
    # source path need not exist here, so parse the mode first and never
    # resolve it: run mode uses neither the path nor the make arguments.
    if (($#)); then
        echo "SMOKE_MODE=run ignores the kernel source path and make arguments: $*" >&2
    fi
else
    source_tree=$(realpath "${1:?usage: run.sh /path/to/linux [kernel make arguments...]}")
    shift
    export KBUILD_BUILD_USER=pocketboot KBUILD_BUILD_HOST=smoke
    export KBUILD_BUILD_TIMESTAMP='Thu Jan 1 00:00:00 UTC 1970'
    export KBUILD_BUILD_VERSION=1 SOURCE_DATE_EPOCH=0
    kernel_make=(make -C "$source_tree" O="$out/kernel" ARCH=arm
        CROSS_COMPILE="${CROSS_COMPILE:-arm-linux-gnueabihf-}" "$@")
    mkdir -p "$out/kernel"
    "${kernel_make[@]}" KCONFIG_ALLCONFIG="$root/tools/arm32-kexec/kernel.config" allnoconfig
    # Fail early if an incompatible source tree silently drops essential options.
    # EFI/MEMFD_CREATE keep the EFI-stub (MZ/PE32) payload path and the
    # memfd-backed preparation path covered; ARM_APPENDED_DTB makes the
    # destination kernel consume a stale appended DTB if the loader fails to
    # remove it from the kernel segment.
    for option in ARCH_VIRT AEABI ARM_THUMB VFP KEXEC ARM_APPENDED_DTB EFI EFI_STUB MEMFD_CREATE \
        BLK_DEV_INITRD RD_GZIP BINFMT_ELF PROC_FS SYSFS DEVTMPFS SERIAL_AMBA_PL011_CONSOLE; do
        grep -qx "CONFIG_$option=y" "$out/kernel/.config" || {
            echo "Required CONFIG_$option=y missing from generated kernel config" >&2
            exit 1
        }
    done
    grep -qx '# CONFIG_SMP is not set' "$out/kernel/.config"
    "${kernel_make[@]}" -j"${JOBS:-$(nproc)}" zImage
    python3 tools/arm32-kexec/check-decompressor.py "$out/kernel/arch/arm/boot/compressed/vmlinux"

    cargo build --locked --release --manifest-path tools/arm32-kexec/Cargo.toml \
        --bin arm32-kexec-smoke --target armv7-unknown-linux-musleabihf \
        --target-dir "$out/rust"
fi
if [[ "$mode" == build ]]; then
    exit 0
fi

kernel="$out/kernel/arch/arm/boot/zImage"
binary="$out/rust/armv7-unknown-linux-musleabihf/release/arm32-kexec-smoke"
for tool in "$qemu" fdtput gzip python3 timeout; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done
for input in "$kernel" "$binary"; do
    [[ -f "$input" ]] || { echo "missing $input; run SMOKE_MODE=build first" >&2; exit 1; }
done

# Generate the exact machine's DTB. fdtput edits properties; the guest checks
# the kernel's sysfs DT view, so the harness never parses FDT itself.
# Disconnect stdin: timeout's process group must not stop on terminal input.
timeout --kill-after=5s "${seconds}s" "$qemu" \
    -M "virt,dtb-randomness=off,dumpdtb=$out/live.dtb" \
    -cpu cortex-a15 -smp 1 -m 256M -nographic </dev/null
# Both fixtures carry a deliberately stale memory map plus the marker property
# the destination compares against; `appended` is poison for `supplied` (the
# explicit DTB must win) and the source of record for `appended`.
cp "$out/live.dtb" "$out/supplied.dtb"
fdtput -t x "$out/supplied.dtb" /memory@40000000 reg 0 50000000 0 08000000
fdtput -t s "$out/supplied.dtb" /chosen pocketboot,dtb-source supplied
cp "$out/live.dtb" "$out/appended.dtb"
fdtput -t x "$out/appended.dtb" /memory@40000000 reg 0 60000000 0 08000000
fdtput -t s "$out/appended.dtb" /chosen pocketboot,dtb-source appended

# `fallback` gets the plain zImage: no explicit DTB and nothing appended, so
# the loader must select the live tree. The other two cases get the same
# destination payload with the appended fixture after the zImage `end` word.
cp "$kernel" "$out/destination.zImage"
cat "$kernel" "$out/appended.dtb" > "$out/destination-appended.zImage"
# Wrap the complete appended payload to exercise decompression and memfd
# preparation as well as EFI classification in an actual guest.
gzip -n -c "$out/destination-appended.zImage" > "$out/destination-appended.zImage.gz"
python3 tools/arm32-kexec/initramfs.py "$binary" "$out" \
    "fallback=$out/destination.zImage" \
    "supplied=$out/destination-appended.zImage:$out/supplied.dtb" \
    "appended=$out/destination-appended.zImage.gz"

cases=(fallback supplied appended)
failed=()
for case in "${cases[@]}"; do
    echo "=== ARM32 kexec: $case ==="
    status=0
    # pipefail propagates timeout/QEMU failure. A PASS alone is insufficient:
    # PID 1 must also power off QEMU within the bound.
    timeout --kill-after=5s "${seconds}s" "$qemu" \
        -M virt -cpu cortex-a15 -smp 1 -m 256M -nographic -no-reboot \
        -kernel "$kernel" \
        -initrd "$out/source-$case.cpio.gz" \
        -append "console=ttyAMA0 rdinit=/init panic=-1 pocketboot.smoke.stage=source pocketboot.smoke.case=$case" \
        </dev/null 2>&1 | tee "$out/$case.log" || status=$?
    # Cases are independent and every log is kept: a failing case still runs
    # the rest, and the script fails once at the end. A guest FAIL record
    # powers the guest off, so a failure normally arrives in seconds rather
    # than at SMOKE_TIMEOUT.
    if [[ $status -ne 0 ]]; then
        echo "ARM32-KEXEC-SMOKE: case=$case: QEMU exit status $status (see $out/$case.log)" >&2
        failed+=("$case")
        continue
    fi
    if grep -q 'ARM32-KEXEC-SMOKE: FAIL' "$out/$case.log"; then
        echo "ARM32-KEXEC-SMOKE: case=$case: guest self-reported FAIL (see $out/$case.log)" >&2
        failed+=("$case")
        continue
    fi
    if ! grep -qx "ARM32-KEXEC-SMOKE: PASS case=$case" <(tr -d '\r' < "$out/$case.log"); then
        echo "ARM32-KEXEC-SMOKE: case=$case: no PASS record (see $out/$case.log)" >&2
        failed+=("$case")
        continue
    fi
    echo "ARM32-KEXEC-SMOKE: case=$case: PASS"
done
if ((${#failed[@]})); then
    echo "ARM32-KEXEC-SMOKE: FAIL ${#failed[@]}/${#cases[@]} cases: ${failed[*]}" >&2
    exit 1
fi
echo "ARM32-KEXEC-SMOKE: PASS all ${#cases[@]} cases"
