# Expressltexx PBL extraction

The PBL window at physical `0x00000000–0x0001ffff` was successfully read
through `/dev/mem` on the inspected MSM8930 handset. Two complete 128 KiB
reads agreed byte-for-byte. No EDL programmer or TrustZone exploit was
needed. This does not bypass secure boot or establish that an unsigned
SBL1 will execute.

## Target and address evidence

The inspected GT-I8730 identifies as `samsung,expressltexx`, MSM8930,
SoC ID 116, revision 1.2. Its bootloader command line identifies
`I8730XWANE1`. Do not substitute Expressatt/MSM8960 images: those are
different devices even though some firmware source paths say `msm8960`.

The initial address-zero, 128 KiB hypothesis came from:

- Qualcomm's [APQ8064 datasheet](https://www.qualcomm.com/content/dam/qcomm-martech/dm-assets/documents/snapdragon_600_apq_8064_data_sheet.pdf),
  section 3.1.1 and figure 4-1: ARM7 RPM is the primary boot processor,
  and its 128 KiB boot ROM is mapped at zero.
- A contemporaneous [SCH-I535/MSM8960 `viewmem` extraction report](https://xdaforums.com/t/r-d-unlock-bootloaders.1769411/post-30084661).
  This is related-chip evidence, not a provenanced MSM8930 binary.

Do not use a broad tool configuration's `0x00100000` ROM range blindly:
this handset's live RPM interface occupies `0x00108000–0x00108fff`.
Do not sweep MMIO looking for executable-looking bytes.

## Validated result

On 2026-10-01 UTC, the diagnostic kernel RAM-booted successfully and the
reader acquired a word, a page, then two identical full-window dumps.
Reset vectors, coherent startup/data-initialization code, PBL source
filenames, and the `BOOT ROM VERSION: 2.0` / `QHSUSB VERSION: 02.02.07`
strings identify a Qualcomm PBL image linked for zero.

The inspected startup copies data from this window into separate working
memory. That supports the ROM interpretation, but
read consistency alone cannot distinguish silicon ROM from an identical
alias or mirror. Preserve the whole captured window, including zero
padding; do not claim this exhausts every ROM on the SoC.

The full persistent BOOT partition matched its pre-test backup. The phone
was then rebooted into its original Pocketboot kernel, with ADB and
fastboot working and `/dev/mem` absent again. No flash-writing commands or
manual security-register writes were issued.

The detailed acquisition/build identities, hashes, instruction offsets,
transport observations and restoration checks are in `refs/notes/evidence`
on the commit recording this result. The earlier preparation evidence is
on `fc27b695f5bf04d5b567c3d6172601c98326deaf`.

## Separate diagnostic build

The inspected Pocketboot kernel had neither `/dev/mem` nor module support.
The [Expressltexx handoff](samsung-expressltexx.md#hardware-validation)
already works without flashing another boot image. Keep the normal device
configuration unchanged and use a separate, ignored output directory.

Using the project's CI toolchain and the kernel revision pinned by the
device config, first build the ordinary image in the lab output:

```sh
device=qcom/msm8930-samsung-expressltexx
kernel=/absolute/path/to/the/pinned/linux/source
export CARGO_TARGET_DIR="$PWD/target/expressltexx-pbl"
cargo xtask build "$device" "$kernel"
out="$CARGO_TARGET_DIR/kernel/$device"
cp "$out/.config" "$out/config.before-pbl"
```

An already verified built-in initramfs can instead be supplied with
`--initrd /absolute/path/to/verified.cpio` to isolate the kernel change.
Record its hash; do not silently substitute another userspace build.

Change only the generated diagnostic config, then rebuild and repackage:

```sh
"$kernel/scripts/config" --file "$out/.config" \
    --enable DEVMEM --enable IKCONFIG --enable IKCONFIG_PROC \
    --set-str LOCALVERSION "-pbl"
make -C "$kernel" O="$out" ARCH=arm LLVM=-19 olddefconfig
make -C "$kernel" O="$out" ARCH=arm LLVM=-19 -j8 \
    zImage qcom/qcom-msm8930-samsung-expressltexx.dtb
cargo xtask bootimg "$device"
python3 tools/arm32-kexec/check-decompressor.py \
    "$out/arch/arm/boot/compressed/vmlinux"
```

Inspect the effective config: retain `SMP=n`, `PHYS_OFFSET=0x80200000`,
`AUTO_ZRELADDR=n` and `KEXEC=y`. Verify the decompressor's fixed
`zreladdr=0x80208000`, packaged DTB, kernel and initramfs identities.
Do not run a normal `cargo xtask build` against this modified output later:
its configuration stamp does not track manual `.config` edits.

## Read experiment

Use the [standalone read-only helper](../tools/physread/README.md). First
obtain approval for a RAM-only boot and a potentially faulting physical
read. Confirm the USB serial and the working persistent BOOT backup before
`fastboot boot`. This procedure does not write a flash partition.

After the handoff, verify the diagnostic kernel release, SoC ID, CPU 0-only
state, correct RAM map, `/chosen/linux,booted-from-kexec`, and `/dev/mem`.
Copy the helper into the RAM-backed `/run` directory and verify a complete,
byte-identical readback before executing it. The tested initramfs's ADB
upload reported completion but did not exit; a subsequent fastboot
`oem cat:/run/physread` / `get_staged` readback verified the upload.
Do not treat a progress message as proof that a transfer succeeded.

The first request is **one aligned word**, `physread 0x0 4`. Read-only
does not mean crash-proof: a bus firewall or inaccessible address can
fault, hang or reset the phone. Stop on failure rather than trying
neighbouring addresses or changing security registers.

The tested acquisition used fastboot's staged shell output. After setting
`serial` to the verified handset and installing `/run/physread`:

```sh
fastboot -s "$serial" oem 'shell:chmod 0755 /run/physread'
fastboot -s "$serial" oem 'shell:/run/physread 0x0 4'
fastboot -s "$serial" get_staged first-word.bin
wc -c first-word.bin
```

Require `shell exited 0` and exactly four output bytes. Shell stderr is
also staged: on failure, the staged output may be an error message, not
ROM bytes. A successful word read is not a dump: inspect it, then consider
a page and finally the corroborated window:

```sh
fastboot -s "$serial" oem 'shell:/run/physread 0x0 0x1000'
fastboot -s "$serial" get_staged first-page.bin
fastboot -s "$serial" oem 'shell:/run/physread 0x0 0x20000'
fastboot -s "$serial" get_staged rom-window-a.bin
fastboot -s "$serial" oem 'shell:/run/physread 0x0 0x20000'
fastboot -s "$serial" get_staged rom-window-b.bin
wc -c first-page.bin rom-window-a.bin rom-window-b.bin
cmp rom-window-a.bin rom-window-b.bin
sha256sum rom-window-a.bin rom-window-b.bin
```

Every operation must succeed; expected sizes are 4096, 131072 and 131072.
Inspect vectors and internal references as well as comparing bytes.
Stable zeros or plausible disassembly alone do not establish provenance.

Keep each fastboot command, including `oem `, within 64 bytes. For longer
scripts use `fastboot stage script.sh` followed by
`fastboot oem shell-staged`, both with the selected serial. An overlong
post-acquisition inventory command left fastboot unresponsive in this
run; ADB remained available and a normal reboot restored both interfaces.
This is a transport observation, not a PBL read failure.

If using raw ADB exec instead, it combines stdout/stderr and does not
carry shell-v2 exit status. Capture binary output, stderr and exit status
into separate RAM files before retrieving them; do not call a raw exec
byte stream a successful dump without those checks.

Keep raw dumps and build products ignored. Record commands, source/build
identities, addresses, lengths, hashes, relevant logs and interpretation
in `refs/notes/evidence` on the experiment commit. Never publish unrelated
RAM, EFS, modem-state or userdata as a supposed ROM dump.
