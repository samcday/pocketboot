.section .text.header,"ax"
.global _start
_start:
 b main
 .long 0
 .quad 0x80000
 .quad 0x10000
 .quad 0xa
 .quad 0,0,0
 .ascii "ARM\x64"
 .long 0
main:
 msr daifset, #0xf
 mov x20, #0x41000000
 mov x9, x20
 mov x10, #512
zero_page:
 str xzr, [x9], #8
 subs x10, x10, #1
 b.ne zero_page
 adr x9, pocketboot_spin_table_start
 adr x10, pocketboot_spin_table_end
 mov x11, x20
copy_code:
 ldrb w12, [x9], #1
 strb w12, [x11], #1
 cmp x9, x10
 b.lo copy_code
 dsb sy
 ic iallu
 dsb sy
 isb
 mov x22, #1
start_cpus:
 add x21, x20, #0x400
 add x21, x21, x22, lsl #7
 mov x9, #1
 str x9, [x21, #8]
 dsb sy
 movz x0, #3
 movk x0, #0xc400, lsl #16
 mov x1, x22
 add x2, x20, #0x100
 mov x3, #0
 hvc #0
 cbnz x0, fail
 add x22, x22, #1
 cmp x22, #4
 b.lo start_cpus
 mov x22, #1
wait_first:
 add x21, x20, #0x400
 add x21, x21, x22, lsl #7
 ldr x9, [x21, #0x40]
 cmp x9, #1
 b.ne wait_first
 ldr x9, [x21, #0x48]
 cmp x9, #4
 b.ne fail
 ldr x9, [x21, #0x50]
 tbz x9, #6, noncoherent
 adr x9, stage1
 str x9, [x21]
 dsb sy
 sev
 add x22, x22, #1
 cmp x22, #4
 b.lo wait_first
 mov x22, #1
wait_stage1:
 add x21, x20, #0x400
 add x21, x21, x22, lsl #7
 ldr x9, [x21, #0x58]
 cmp x9, #1
 b.ne wait_stage1
 str xzr, [x21]
 mov x9, #2
 str x9, [x21, #8]
 dsb sy
 sev
 add x22, x22, #1
 cmp x22, #4
 b.lo wait_stage1
 mov x22, #1
wait_second:
 add x21, x20, #0x400
 add x21, x21, x22, lsl #7
 ldr x9, [x21, #0x40]
 cmp x9, #2
 b.ne wait_second
 ldr x9, [x21, #0x48]
 cmp x9, #4
 b.ne fail
 adr x9, stage2
 str x9, [x21]
 dsb sy
 sev
 add x22, x22, #1
 cmp x22, #4
 b.lo wait_second
 mov x22, #1
wait_stage2:
 add x21, x20, #0x400
 add x21, x21, x22, lsl #7
 ldr x9, [x21, #0x58]
 cmp x9, #2
 b.ne wait_stage2
 add x22, x22, #1
 cmp x22, #4
 b.lo wait_stage2
 adr x4, pass_msg
 adr x1, exit_success
 b print_exit
// QEMU's A53 CPUECTLR model reads zero. In that case verify the real
// resident reports the value and refuses release; do not fake coherency.
noncoherent:
 cmp x22, #1
 b.ne fail
noncoherent_wait:
 add x21, x20, #0x400
 add x21, x21, x22, lsl #7
 ldr x9, [x21, #0x40]
 cmp x9, #1
 b.ne noncoherent_wait
 ldr x9, [x21, #0x48]
 cmp x9, #4
 b.ne fail
 ldr x9, [x21, #0x50]
 tbnz x9, #6, fail
 adr x9, stage1
 str x9, [x21]
 dsb sy
 sev
 add x22, x22, #1
 cmp x22, #4
 b.lo noncoherent_wait
 mrs x9, cntfrq_el0
 lsr x9, x9, #2
 mrs x10, cntvct_el0
 add x10, x10, x9
noncoherent_delay:
 mrs x9, cntvct_el0
 cmp x9, x10
 b.lo noncoherent_delay
 mov x22, #1
noncoherent_check:
 add x21, x20, #0x400
 add x21, x21, x22, lsl #7
 ldr x9, [x21, #0x58]
 cbnz x9, fail
 add x22, x22, #1
 cmp x22, #4
 b.lo noncoherent_check
 adr x4, noncoherent_msg
 adr x1, exit_success
 b print_exit
fail:
 adr x4, fail_msg
 adr x1, exit_failure
print_exit:
 mov x5, #0x09000000
print_loop:
 ldrb w6, [x4], #1
 cbz w6, test_exit
 str w6, [x5]
 b print_loop
test_exit:
 mov x0, #0x20
 hlt #0xf000
halt:
 wfe
 b halt
stage1:
 orr x4, x0, x1
 orr x4, x4, x2
 orr x4, x4, x3
 cbnz x4, fail
 mrs x10, mpidr_el1
 and x10, x10, #0xff
 mov x9, #0x41000000
 add x9, x9, #0x400
 add x9, x9, x10, lsl #7
 mov x11, #1
 str x11, [x9, #0x58]
 dsb sy
stage1_wait:
 ldr x11, [x9, #8]
 cmp x11, #2
 b.ne stage1_wait
 mov x0, #0xffff
 mov x1, #0xffff
 mov x2, #0xffff
 mov x3, #0xffff
 mov x16, #0x41000000
 add x16, x16, #0x100
 br x16
stage2:
 orr x4, x0, x1
 orr x4, x4, x2
 orr x4, x4, x3
 cbnz x4, fail
 mrs x10, mpidr_el1
 and x10, x10, #0xff
 mov x9, #0x41000000
 add x9, x9, #0x400
 add x9, x9, x10, lsl #7
 mov x11, #2
 str x11, [x9, #0x58]
 dsb sy
 b halt
.balign 8
exit_success: .quad 0x20026, 0
exit_failure: .quad 0x20026, 1
pass_msg: .asciz "PASS resident v1: 3 cores, 2 generations, entry registers zero\n"
noncoherent_msg: .asciz "PASS resident v1: 3 cores report SMPEN=0 and refuse release\n"
fail_msg: .asciz "FAIL resident v1\n"
.include "spin_table.S"
