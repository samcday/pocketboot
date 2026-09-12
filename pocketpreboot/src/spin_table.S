// pocketboot,spin-table-v1 resident image. All addresses are PC-relative;
// this section is copied once into the no-map reservation before CPU startup.
.section .rodata.spin_table, "a"
.balign 8
.global pocketboot_spin_table_start
.global pocketboot_spin_table_end
pocketboot_spin_table_start:
    b       .Lpark_entry
    .space  0x80 - (. - pocketboot_spin_table_start)
    .ascii  "spin-tab"
    .quad   0                       // unused legacy shared release address
    .ascii  "PBSPIN01"
    .long   1, 4096, 0x100, 0x400, 0x80, 4
    .ascii  "PBSDIAG1"              // optional CPUECTLR snapshot at slot+0x50
    .space  0x100 - (. - pocketboot_spin_table_start)
.Lpark_entry:
    msr     daifset, #0xf
    // Both SCM cold boot and the kernel's physical cpu_die path must enter
    // with MMU and D-cache disabled. Never acknowledge otherwise.
    mrs     x9, CurrentEL
    mov     x12, x9
    cmp     x9, #4
    b.eq    .Lel1
    cmp     x9, #8
    b.ne    .Lhalt
    mrs     x9, sctlr_el2
    b       .Lcheck_sctlr
.Lel1:
    mrs     x9, sctlr_el1
.Lcheck_sctlr:
    tst     x9, #1
    b.ne    .Lhalt
    tst     x9, #4
    b.ne    .Lhalt
    mrs     x10, mpidr_el1
    // MPIDR.MT/U are not affinity fields. Aff3 and Aff2:Aff0 must encode 1..3.
    ubfx    x11, x10, #32, #8
    cbnz    x11, .Lhalt
    and     x10, x10, #0xffffff
    cmp     x10, #1
    b.lo    .Lhalt
    cmp     x10, #3
    b.hi    .Lhalt
    adr     x9, pocketboot_spin_table_start
    add     x9, x9, #0x400
    add     x9, x9, x10, lsl #7
    ldr     x11, [x9, #8]           // request token owned by the primary
    cbz     x11, .Lhalt
    str     x12, [x9, #0x48]        // report EL before publishing acknowledgment
    mrs     x13, S3_1_C15_C2_1      // Cortex-A53 CPUECTLR_EL1; never write it
    str     x13, [x9, #0x50]
    dsb     sy
    str     x11, [x9, #0x40]        // acknowledgment owned by this CPU
    dsb     sy
    sev
.Lwait:
    ldr     x16, [x9]
    cbnz    x16, .Lrelease
    wfe
    b       .Lwait
.Lrelease:
    tbz     x13, #6, .Lhalt        // firmware must enable SMPEN before Linux
    dsb     sy
    isb
    mov     x0, xzr
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    br      x16
.Lhalt:
    wfe
    b       .Lhalt
pocketboot_spin_table_end:
