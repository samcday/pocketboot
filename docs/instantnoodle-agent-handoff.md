# IN2017: first local-agent session

**Goal: establish the next unlock-policy question, without changing the phone.**
This is not an unlock procedure and does not authorize a pocketboot boot trial.
Use the owner's latest device reports as the baseline; do not repeatedly request
a complete inventory. Keep handset-specific reports in the collaboration
context, not in this generic guide or a public PR.

## Put the agent on the right host

- The agent's terminal must run on the machine physically connected to the
  OnePlus 8. A Linux phone with working USB host mode can be that machine; a PC
  or Windows VM is not a prerequisite for ordinary adb/fastboot queries.
- With Delta, the owner should send from the desktop app using the local runner
  on that host. A browser turn has no local shell/USB access, and another
  participant's local turn runs on that participant's machine, not this host.
  A local OpenCode terminal session is another option.
- Verify `adb` and `fastboot` in the agent's actual execution environment, not
  merely in a different terminal. Record their versions. Missing tools or
  permissions are a host setup issue, not an unlock diagnosis.

## Permission and target selection

Get one explicit authorization for this bounded, read-only pass. The owner
confirms which phone is connected and disconnects other test devices.
Capture discovery output internally: `adb devices` and `fastboot devices`
contain serials and must not be pasted into the transcript or report.

Require exactly one intended target, with no ambiguous ADB/fastboot transports.
Select it explicitly with `-s` using its locally discovered identifier.
Keep that identifier private, including in errors and subprocess diagnostics.
If it disconnects or changes identity, stop rather than selecting a replacement.
`product=kona` identifies a platform, not a unique handset or a retail IN2017.

Discover and consume the identifier inside the same local process; do not
paste it into a recorded tool-call command. Do not assume shell variables
survive between agent tool invocations. The examples below show the allowed
operations, not a device selector to fill in publicly.

Give each USB operation a finite timeout, for example 15 seconds. Stop on
missing/unauthorized devices, permission errors, timeouts, ambiguous selection,
or unexpected mode. Do not reset USB, restart ADB, elevate privileges, change
settings, or reboot the phone to work around these within this pass.

## If already in bootloader fastboot

Set `DEVICE` locally to the verified target; do not print it. These Linux
examples use GNU `timeout`, or the agent can enforce the equivalent subprocess
timeout. Capture and redact output before returning it to the conversation.

First establish the mode:

```sh
timeout 15s fastboot -s "${DEVICE:?select the verified target locally}" getvar is-userspace
```

Continue only if this reports `no`. `yes` means fastbootd, not bootloader
fastboot; an unsupported/empty response is unknown, not permission to assume
the mode. Stop and report either case without changing modes.

Then check the current identity/state against the owner's baseline:

```sh
timeout 15s fastboot -s "${DEVICE:?}" getvar product
timeout 15s fastboot -s "${DEVICE:?}" getvar unlocked
timeout 15s fastboot -s "${DEVICE:?}" getvar current-slot
```

If the values are missing or differ unexpectedly, report that and stop.
Otherwise, the one new eligibility observation is:

```sh
timeout 15s fastboot -s "${DEVICE:?}" flashing get_unlock_ability
```

The final command is a **query**, despite the word `flashing`. Do not shorten
it to an unlock command. Capture both stdout and stderr and label each result
with its command name; fastboot normally reports values on stderr. Distinguish
an explicit `0`/`1` from unsupported, empty, denied or timed-out responses.

- `0`: the bootloader reports unlocking is not currently allowed. It does not
  identify which user, carrier, device-policy or vendor condition blocks it.
- `1`: that permission query is positive. It is neither an unlocked bootloader
  nor proof that a carrier token is unnecessary or that unlocking will succeed.
- Anything else: retain the uncertainty; do not try alternative OEM commands.

`secure=yes` is not a substitute for `unlocked`. If the state differs from the
owner's baseline, report the difference and stop before interpreting eligibility.
Even if it now says unlocked, this session does not authorize a boot trial.

## If already in normal Android instead

Do not automatically reboot it. With one authorized, explicitly selected ADB
target, confirm only the current identity and relevant state:

```text
adb -s DEVICE shell getprop ro.product.model
adb -s DEVICE shell getprop ro.product.device
adb -s DEVICE shell getprop ro.build.display.id
adb -s DEVICE shell getprop ro.boot.flash.locked
adb -s DEVICE shell getprop ro.oem_unlock_supported
adb -s DEVICE shell getprop sys.oem_unlock_allowed
```

Apply the same timeout/redaction rules; `DEVICE` above is a placeholder, not
an instruction to paste a serial into the conversation. Leave empty properties
empty/unknown, not `0`. These properties are reported identity/policy hints,
not proof of board revision, carrier eligibility or a particular unlock cause.
Do not repeat root attempts or DT reads already known to be denied.

Report that the bootloader-only query is still outstanding. A reboot into
bootloader mode and back requires separate owner approval and device selection.

## Stop with a useful report

Return a short, labelled report: host tools, observed mode, queried values,
differences from the baseline, facts versus inferences, and the next unresolved
question. Do not collect `getvar all`, `getprop` without a property name, full
bugreports, device identifiers, unlock challenges, or tokens.

If needed, ask the owner to **observe**, not change, the OEM unlocking toggle
(checked/off/greyed out and its displayed reason), the carrier/network-unlock
status, or an existing token request's status/error. Do not infer SIM lock from
`sys.oem_unlock_allowed=0`, or invent Binder transaction numbers to probe it.

Android 10's carrier/user OEM-lock APIs are privileged; a normal shell has no
standard query that attributes the aggregate bit to a particular cause.
An enabled, unchecked OEM-unlocking control is consistent with the user setting
being off and the UI gate open. A disabled/absent control or generic restriction
message leaves the cause indeterminate. Record any owner-reported management
restriction, but do not collect an enterprise/account inventory.

No unlock/relock, flash/erase/format, slot switch, EDL/MSM/OPS operation,
firmware conversion, root attempt, or experimental boot is authorized here.
Do not submit vendor forms or publish handset evidence without owner approval.
The [already-unlocked hardware-test gate](instantnoodle-bringup.md#first-hardware-test)
remains in force.

## References and later work

- [OnePlus token portal](https://www.oneplus.com/us/unlock_token): explicitly
  T-Mobile-only. Vendor-token issuance, network unlock, the user OEM-unlocking
  permission and bootloader lock state are distinct.
- [Lineage IN2017 unlock section](https://wiki.lineageos.org/devices/instantnoodle/install/variant2/#unlocking-the-bootloader):
  ordinary fastboot is not Windows-only. Its other firmware-installation steps
  are not prerequisites or instructions for this pocketboot experiment.
- [Android 10 OEM-lock service](https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-10.0.0_r1/services/core/java/com/android/server/oemlock/OemLockService.java):
  separates carrier/user/admin conditions and the aggregate allowed state.
  This defines AOSP behavior, not proof of OnePlus's proprietary implementation.
- [Delta execution environments](https://delta.dev/docs/collaboration/collaborate-thread#where-the-agent-runs):
  sharing a thread does not share a participant's USB devices or shell.

Offline firmware inspection can proceed separately on a suitable build host.
Inventory an archive before choosing an extractor: an MSM/OPS archive is not
necessarily an OTA with `payload.bin`. Same carrier family or a nearby version
is not an exact match for the installed build. Do not flash an archive merely
because it is useful as a reference.
