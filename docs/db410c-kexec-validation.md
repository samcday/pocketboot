# DB410c spin-table kexec validation

`tools/db410c_kexec.py` prepares host artifacts only. It does not access USB or
UART. Its Android v2 images contain the **bare kernel**, with the separately
supplied destination DTB in the v2 DTB section. The first two tests enter the
bare kernel directly. The optional third test uses a shim with an explicit
resident-reentry validator; a cold-start-only shim deliberately refuses an
occupied table and cannot serve as that destination.

The existing Pocketboot fastboot command that loads and executes kexec is:

```sh
fastboot -s bc72e60 boot /var/home/sam/src/pocketboot/target/db410c-lab/20260911/kexec-contract/generation-1.img
```

`fastboot stage IMAGE` followed by `fastboot oem kexec-load` loads without
executing. The target has no BusyBox `kexec` applet; `reboot -f` would perform an
ordinary reboot. `fastboot boot` exercises Pocketboot's actual Rust loader and
then its `reboot(LINUX_REBOOT_CMD_KEXEC)` call.

## Prepared artifacts and provenance

Prepared on 2026-09-11 with:

```sh
cd /var/home/sam/src/pocketboot
python3 tools/db410c_kexec.py \
  --kernel target/db410c-lab/20260911/parking.Image \
  --dtb target/kernel/qcom/apq8016-sbc/arch/arm64/boot/dts/qcom/apq8016-sbc.dtb \
  --cmdline-file target/db410c-lab/20260911/baseline-cmdline.txt \
  --reentry-preboot /tmp/pocketpreboot-db410c-v1-reentry.bin \
  --output target/db410c-lab/20260911/kexec-contract
```

The output directory must be new. Use a new directory if inputs change.
`manifest.json` records hashes of all inputs, generated images, DTBs, workload
binary and shell scripts. Fresh output also records `workload.version = 2`,
the target path, compiler flags and workload acceptance thresholds. The
generator verifies Android v2 header fields and
compares the embedded Image and DTB bytes against their sources.

- `generation-1.img`, `generation-2.img`: same patched kernel, different
  `pocketboot.lab=kexec-generation-N` command-line markers.
- `external-psci.dtb`: CPU MPIDRs 0–3 start with `enable-method = "psci"`, no
  release addresses and no v1 parking reservation. This deliberately challenges
  propagation from the running spin-table kernel.
- `negative-cpu3-disabled.img`: destination CPU3 is disabled.
- `negative-parking-overlap.img`: another destination reserved-memory owner
  occupies `0x854ff000..0x85500000`.
- `workload-v2-install/*.sh`: chunks that reconstruct `smp-work-v2` as
  `/tmp/pb-smp-v2/work` in target RAM
  using BusyBox `printf`. Each staged script is under the 64 KiB shell limit.
- `snapshot-v2.sh`: requires `online=0-3`, records boot ID, command line, CPU FDT
  methods/releases and `/proc/stat`, then runs four concurrent pinned workers.
  Boot ID is optional: this lab kernel does not expose
  `/proc/sys/kernel/random/boot_id`; the script records `unavailable` and uptime.

The existing lab directory also retains the original `workload-install/`,
`snapshot.sh` and their captured results. That first `-Os` workload was invalid:
the compiler reused a calculation performed before `fork`, leaving effectively
no timed child work. Do not use its `PASS` marker as SMP workload evidence. The
V2 source reads a volatile seed inside each child's timed loop, compares against
a constant expected hash, and sends results through a pipe for the parent to
print. The generator deliberately keeps `-Os` to cover this regression.

`workload-v2-manifest.json` records the separately added V2 artifacts in the
existing lab directory without replacing the original evidence. Its exact
67816-byte workload SHA256 is
`3b1a25c21801708e04464954efb10b0723b92a07efc4f91cdd08b85316536ab5`.

The frozen `parking.Image` was copied at 09:57:14, after the final loader source
edit at 09:53:22 and release Pocketboot build at 09:56:58. Stronger verification:
its offset 7451564 contains the exact DB build's `usr/initramfs_inc_data` gzip;
the extracted `/init` SHA256 is
`a6ba36b44ee8b5c7541c4c8efec2478d419057858bc93873fcc44357cadd32c8`, matching the
release Pocketboot binary. This establishes inclusion of the final loader
changes. Rebuild/repackage if subsequent kernel or userspace changes are needed.

## Capture and snapshots

Start with Pocketboot running on the DB410c, not U-Boot fastboot. Check:

```sh
fastboot -s bc72e60 getvar product
fastboot -s bc72e60 getvar compatible
fastboot -s bc72e60 getvar is-userspace
```

Expect `pocketboot`, `qcom,apq8016-sbc`, and `yes`. Keep the exact serial selector
on every fastboot command. Take exclusive ownership of UART before running a
capture; do not open it while another capture or terminal owns it.

```sh
python3 /var/home/sam/src/pocketboot/tools/db410c_uart.py \
  --uart /dev/serial/by-id/usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0 \
  --output /var/home/sam/src/pocketboot/target/db410c-lab/20260911/kexec-uart-1.log \
  --seconds 60
```

Run this capture concurrently with one handoff. Use a new log filename for the
second handoff. It sends nothing without explicit `--send` or `--break`.

The following helper stages a script, runs it and retrieves its output even
when the target script fails. Diagnostic OEM commands replace the staged
payload, so **never insert a diagnostic between staging an image and loading
that image**. Fastboot command packets are limited to 64 bytes including `oem `;
use `stage` + `oem shell-staged` for scripts. Do not inline a long `oem shell:`
command. All diagnostic command strings shown here fit that limit.

```sh
PB_ARTIFACTS=/var/home/sam/src/pocketboot/target/db410c-lab/20260911/kexec-contract
PB_EVIDENCE=/var/home/sam/src/pocketboot/target/db410c-lab/20260911/kexec-proof
mkdir -p "$PB_EVIDENCE"
pb_shell() {
    timeout 45 fastboot -s bc72e60 stage "$1" >> "$PB_EVIDENCE/fastboot.log" 2>&1 || return
    pb_status=0
    timeout 45 fastboot -s bc72e60 oem shell-staged >> "$PB_EVIDENCE/fastboot.log" 2>&1 || pb_status=$?
    timeout 45 fastboot -s bc72e60 get_staged "$2" >> "$PB_EVIDENCE/fastboot.log" 2>&1 || return
    return "$pb_status"
}
```

For each of `before`, `after-1`, `after-2`, reconstruct the workload (the new
initramfs does not retain `/tmp`), take its snapshot and save the actual FDT:

```sh
PB_PHASE=before
for pb_script in "$PB_ARTIFACTS"/workload-v2-install/*.sh; do
    pb_shell "$pb_script" "$PB_EVIDENCE/$PB_PHASE-install-v2-$(basename "$pb_script").txt" || break
done
pb_shell "$PB_ARTIFACTS/snapshot-v2.sh" "$PB_EVIDENCE/$PB_PHASE-snapshot-v2.txt"
fastboot -s bc72e60 oem cat:/sys/firmware/fdt
fastboot -s bc72e60 get_staged "$PB_EVIDENCE/$PB_PHASE-live.dtb"
fastboot -s bc72e60 oem dmesg
fastboot -s bc72e60 get_staged "$PB_EVIDENCE/$PB_PHASE-dmesg.txt"
```

Require the `SMP_WORK_V2` header, four distinct `PASS` records for CPU0–3 with
matching pinned, start and end CPU IDs, **each `cpu_seconds` greater than
0.02**, hash `292d74d0d0222325`, and the final `SMP_WORK_PASS all_four_cpus`.
The program rejects missing or duplicate CPU records and child failures. Require
all four live CPU methods to be spin-table and release addresses
`854ff400`, `854ff480`, `854ff500`, `854ff580`. Require the new generation's
command-line marker and corresponding UART boot trace after each handoff;
compare boot IDs as an additional check when that optional interface exists.
USB reconnection alone is insufficient.

## Load-only negative cases

Run these only after proving the outgoing live DTB has the active v1 contract.
On an ordinary PSCI baseline, the CPU-set rejection does not apply. Begin with
`/sys/kernel/kexec_loaded` equal to zero and save the command line and uptime
(also boot ID when available). These tests must
return fastboot failure **without** invoking a reboot:

```sh
fastboot -s bc72e60 stage "$PB_ARTIFACTS/negative-cpu3-disabled.img"
fastboot -s bc72e60 oem kexec-load
# Expected failure: destination CPU MPIDRs do not match the live spin-table CPUs.
fastboot -s bc72e60 stage "$PB_ARTIFACTS/negative-parking-overlap.img"
fastboot -s bc72e60 oem kexec-load
# Expected failure: destination reserved-memory conflicts with the live spin-table page.
```

Capture each command's stdout/stderr and exit status. After each failure,
retrieve `/sys/kernel/kexec_loaded` with `oem cat:` + `get_staged`, require it
still equals zero, verify the command line is unchanged and uptime has advanced
(also unchanged boot ID when available), and rerun the four-core workload. Unexpected success is a test failure: **do not boot the negative
image**. An acknowledgement timeout after CPU teardown needs a separate
instrumented kernel test; these fixtures test loader rejection before teardown.

## Two successive handoffs

With UART capture running and the outgoing snapshot accepted:

```sh
fastboot -s bc72e60 boot "$PB_ARTIFACTS/generation-1.img"
```

Wait for Pocketboot to reconnect, repeat the identity check and take `after-1`
artifacts. The UART/dmesg evidence must show all three secondary parking
acknowledgements before relocation, followed by all four CPUs booting. The
kernel patch emits `CPU1/2/3: persistent spin-table park acknowledged (epoch N)` per CPU.

After accepting that evidence, start another UART capture and run:

```sh
fastboot -s bc72e60 boot "$PB_ARTIFACTS/generation-2.img"
```

Repeat the full checks as `after-2`. The second handoff proves the new kernel
can itself park the already handed-over CPUs, rather than merely consuming a
one-time initial spin table. Retain both the initially PSCI destination DTB and
the captured live spin-table DTBs to demonstrate the loader's graft.

## Optional third handoff through reentry-capable pocketpreboot

Generation 3 was additionally packaged from the exact
`/tmp/pocketpreboot-db410c-v1-reentry.bin`. This shim validates the resident
version, descriptor and acknowledged slots before reusing them; it does not
reset CPUs or rewrite occupied resident code. Its command-line marker is
`pocketboot.lab=kexec-generation-3-preboot`.

```sh
fastboot -s bc72e60 boot "$PB_ARTIFACTS/generation-3-preboot.img"
```

Capture a fresh UART log and repeat the complete four-core/live-FDT snapshot as
`after-3`. Require the shim's `reusing acknowledged resident CPUs` message in
addition to the outgoing parking acknowledgements and incoming four-core work.

The shim has image_size `0x8bc50`; its kernel starts at offset `0x200000`.
The combined file is padded through the inner kernel's complete `0xa60000`
runtime footprint, including BSS. This matters because the legacy kexec loader
uses the larger of the outer header's image_size and the file size for its
reservation, while pocketpreboot uses the outer image_size to locate its
payload. Merely appending the shorter kernel file would leave some inner BSS
unprotected from placement of the next DTB or trampoline.

Prepared generation-3 Android image: 13058048 bytes,
SHA256 `67d82fb6a626dfcd80ee4c7c203dc242b3b3a20ff754ff97389e5f5d809d50f6`.

## Executed coherency validation

The newer `target/db410c-lab/20260911/kexec-coherency/` directory was prepared
with `/tmp/pocketpreboot-db410c-v1-coherency.bin`, the frozen `parking.Image`
and the V2 workload. It preserves the earlier `kexec-contract/` artifacts and
evidence. The matching `nonpsci-coherency` cold-boot FIT and all three new
destinations have now booted on DB410c. UART verifies acknowledgments at epochs
2, 3 and 4; generation 3 reused the resident CPUs and reported SMPEN enabled.
Every kernel passed the measured V2 arithmetic workload and the separate
shared-memory/migration diagnostic.

For that run, use this artifact directory for the V2 installation, snapshots,
negative cases and both bare-kernel generations described above:

```sh
PB_ARTIFACTS=/var/home/sam/src/pocketboot/target/db410c-lab/20260911/kexec-coherency
```

After accepting the earlier generations' evidence, the exact new generation-3
command is:

```sh
fastboot -s bc72e60 boot /var/home/sam/src/pocketboot/target/db410c-lab/20260911/kexec-coherency/generation-3-preboot.img
```

The new shim SHA256 is
`5f87eb4bd5ee2b9591c26f645cb132adca48f9cc8fd4166d25681fbc642de561`.
Its outer image_size is `0x8be50`; the inner kernel still starts at `0x200000`
and the combined file covers the complete `0xc60000` runtime footprint.
The new Android image is 13058048 bytes, SHA256
`627fbe47d6a147cbe20145ecae1f5e1a052a9198dd27aada57846d768ad50cdf`.
Its `manifest.json` records workload version 2 and all input/artifact hashes.
Host validation checked those hashes, Android section bytes, shell syntax and
an isolated transfer-script roundtrip against the exact V2 binary. The subsequent
hardware evidence is under `target/db410c-lab/20260911/coherency-proof/`.

With the separate `coherency-install/` scripts and their manifest present, one
running phase can now be checked with the host controller:

```sh
python3 tools/db410c_check_phase.py --serial bc72e60 \
  --artifacts target/db410c-lab/20260911/kexec-coherency \
  --output /tmp/db410c-fresh-generation-3-check \
  --marker kexec-generation-3-preboot
```

It verifies script hashes, requires a fresh output directory, installs into
target RAM, checks the expected boot marker and four measured CPU results,
runs the coherency diagnostic, and retrieves the live FDT and kernel log.
It neither boots nor flashes an image. Keep UART recording independently.
