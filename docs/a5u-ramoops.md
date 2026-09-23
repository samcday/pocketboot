# A5 UART-free panic recovery

The tested device is `cd0ee037`, `samsung,a5u-eur`. These commands do not target
the separately connected DB410c or Pixel. The installed lk2nd build includes
`oem ramoops raw` and forces fastboot on reboot.

## Test configuration

The A5 DT overlay reserves 512 KiB at `0x9ff80000`, matching lk2nd's raw download
window. Kernel config enables PSTORE_RAM, PSTORE_CONSOLE and compression.
The layout is 32 records of 8 KiB followed by a 256 KiB console region, with
64-byte Reed-Solomon ECC for each 128-byte data block and for each header.

Add `panic=5 oops=panic` to **both** the source and destination kernel command
lines. `tools/db410c_kexec.py` now adds these by default (`--panic-timeout`
changes the positive delay). This does not recover a hard lockup automatically.
Continuous console logging can still be useful after a manual reset.

Do not use `lk2nd.pass-ramoops` with this overlay: that lk2nd DT fixup would
replace the configured ECC size with zero. Its `zap` argument would also erase
the retained logs. The bulk raw-download command does neither.

## Download after a failure

Verify all three identity values before collecting the raw window:

```sh
fastboot -s cd0ee037 getvar product
fastboot -s cd0ee037 getvar serialno
fastboot -s cd0ee037 getvar lk2nd:compatible
# Expected: lk2nd-msm8916, cd0ee037, samsung,a5u-eur

fastboot -s cd0ee037 oem ramoops raw
fastboot -s cd0ee037 get_staged /tmp/a5-ramoops.bin
python3 tools/ramoops_decode.py \
  --input /tmp/a5-ramoops.bin --output /tmp/a5-ramoops-decoded \
  --kernel target/kernel/src/msm8916
```

The output directory must be new. Keep the original binary. The decoder
compiles a small host adapter around the supplied kernel's exact RS codec;
it requires a host C compiler, but no Python package installation. It does not
open USB or write device memory. Linux pstore's compressed records use raw
DEFLATE, not a zlib-wrapped stream.

`decoded.json` records corrected symbols, uncorrectable blocks, and invalid
headers. Complete records produce `.log` files. A record with unrecoverable
data produces `.damaged` bytes instead; parts may be readable, but it is not
a verified complete log. ECC has finite correction capacity. One measured
kexec panic reset exceeded it in some blocks, while two simpler panic resets
preserved the complete records without corrections.

When the next kernel boots successfully, it saves the outgoing console in
pstore before starting a fresh console ring:

```sh
mkdir -p /tmp/pb-pstore
mount -t pstore pstore /tmp/pb-pstore
cat /tmp/pb-pstore/console-ramoops-0
```

Run these on the device through Pocketboot's staged shell; use `oem cat:` and
`get_staged` for exact host capture. Do not delete pstore files until their
contents have been saved.

## Evidence and limits

[The archived A5 experiment log](https://github.com/samcday/pocketboot/blob/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/a5u-smp-experiments-2026-09-11.md) records the deliberate
SysRq panic, automatic return to lk2nd, the USB alignment failure, and the
subsequent display/IOMMU failure. Raw captures are under
[the archive’s ramoops directory](https://github.com/samcday/pocketboot/tree/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/evidence/a5u-smp-2026-09-11/ramoops/).

The deliberate panic was induced through `/proc/sysrq-trigger`; that root-only
interface bypasses the serial SysRq mask in this kernel. `/proc/sys/kernel`
controls are unavailable in this small config, so the command line is the
source of the panic timeout.

The resident spin-table page may survive a firmware reset, usually with sparse
bit flips. This is not a valid preboot re-entry. Earlier shims refused any page
whose `spin-tab` or `PBSPIN` marker survived intact, which left the A5 unable
to boot until the signatures were retired over lk2nd. Cold startup now
reclaims such a page once ACC shows every secondary held in reset. Do not
treat log collection as authority to clear arbitrary resident memory.
