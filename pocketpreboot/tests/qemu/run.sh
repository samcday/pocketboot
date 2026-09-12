#!/usr/bin/env bash
set -euo pipefail

# Generated files stay outside the worktree. These tests use QEMU's PSCI only
# to create emulated secondaries; production MSM8916 startup still uses SCM.
test_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
preboot_dir=$(cd -- "$test_dir/../.." && pwd)
output_dir=${1:-$(mktemp -d /tmp/pocketpreboot-qemu.XXXXXX)}
mkdir -p -- "$output_dir"

for tool in rustc aarch64-linux-gnu-as aarch64-linux-gnu-ld \
    aarch64-linux-gnu-objcopy qemu-system-aarch64 timeout; do
    command -v "$tool" >/dev/null
done

aarch64-linux-gnu-as -I "$preboot_dir/src" "$test_dir/resident.S" \
    -o "$output_dir/resident.o"
aarch64-linux-gnu-ld -T "$test_dir/resident.ld" "$output_dir/resident.o" \
    -o "$output_dir/resident.elf"
aarch64-linux-gnu-objcopy -O binary "$output_dir/resident.elf" "$output_dir/resident.bin"
timeout 10 qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a53 \
    -smp 4 -m 512M -nographic -monitor none \
    -semihosting-config enable=on,target=native -kernel "$output_dir/resident.bin"

rustc --edition=2024 --target aarch64-unknown-none -C opt-level=z \
    -C panic=abort -C relocation-model=pic -C linker=rust-lld \
    -C "link-arg=-T$preboot_dir/linker.ld" -C link-arg=-pie \
    -C link-arg=-znorelro -C link-arg=--no-dynamic-linker \
    "$test_dir/startup.rs" -o "$output_dir/startup.elf"
aarch64-linux-gnu-objcopy -O binary "$output_dir/startup.elf" "$output_dir/startup.bin"
timeout 10 qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a53 \
    -m 512M -nographic -monitor none \
    -semihosting-config enable=on,target=native -kernel "$output_dir/startup.bin"

printf 'QEMU artifacts: %s\n' "$output_dir"
