# ARM32 kexec bring-up

This work is separate from the Samsung Express device kernels. Expressltexx
and Expressatt use different integration trees and must be validated
independently on hardware.

The initial target is little-endian ARMv7 Linux with a device tree and the
legacy `kexec_load` syscall. The loader must preserve the live RAM map,
exclude reserved memory, keep the destination kernel's working area clear,
and deliver the command line and optional initrd through the destination DTB.

Linux's ARM kexec implementation already relocates the submitted segments
and handles cache/MMU shutdown. The userspace handoff trampoline is not a
second relocation engine, nor a full integrity-checking kexec-tools
purgatory.

Qualcomm's current ARM32 SMP operations lack `cpu_kill`; the initial loader
kernels therefore use `CONFIG_SMP=n`. Supporting SMP in the destination OS
does not remove that first-kernel constraint.

Validation must cover host-side malformed-image and placement tests, ARMv7
cross-compilation, and a real two-kernel handoff under QEMU before asking for
phone testing. Emulator success does not validate Qualcomm display, USB, or
other device shutdown paths.

Status: implementation and emulator validation in progress. No phone
handoff is claimed.
