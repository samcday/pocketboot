.section .text.header, "ax"
.balign 8

.global _start
_start:
    b       .Lentry
    .long   0
    .quad   0
    .quad   __image_size
    .quad   0xa
    .quad   0
    .quad   0
    .quad   0
    .byte   0x41, 0x52, 0x4d, 0x64
    .long   0

.Lentry:
    msr     daifset, #0xf
    mov     x19, x0

.ifdef PB_RAM_TRACE
    .macro pb_trace_store reg, offset
    str     \reg, [x16, #(\offset)]
    str     \reg, [x16, #(4096 + \offset)]
    str     \reg, [x16, #(8192 + \offset)]
    .endm
    // Private lab buffer, outside the BSS cleared below. Record entry even
    // when Rust, UART or exception reporting cannot run.
    adrp    x16, pocketpreboot_ram_trace
    add     x16, x16, :lo12:pocketpreboot_ram_trace
    mov     x17, x16
    add     x18, x16, #3, lsl #12
.Ltrace_clear:
    str     xzr, [x17], #8
    cmp     x17, x18
    b.lo    .Ltrace_clear
    movz    x17, #0x4250
    movk    x17, #0x5254, lsl #16
    movk    x17, #0x4341, lsl #32
    movk    x17, #0x3145, lsl #48  // "PBTRACE1"
    pb_trace_store x17, 0
    mov     x17, #1
    pb_trace_store x17, 8
    pb_trace_store x19, 24
    mrs     x17, CurrentEL
    pb_trace_store x17, 32
    cmp     x17, #4
    b.eq    .Ltrace_sctlr_el1
    cmp     x17, #8
    b.ne    .Ltrace_entry_done
    mrs     x17, sctlr_el2
    b       .Ltrace_sctlr_store
.Ltrace_sctlr_el1:
    mrs     x17, sctlr_el1
.Ltrace_sctlr_store:
    pb_trace_store x17, 40
.Ltrace_entry_done:
    movz    x17, #0x2325
    movk    x17, #0x8422, lsl #16
    movk    x17, #0x9ce4, lsl #32
    movk    x17, #0xcbf2, lsl #48  // FNV-1a empty-text checksum
    pb_trace_store x17, 72
    dsb     sy
.endif

    // aarch64-unknown-none permits compiler-generated FP/SIMD instructions,
    // including in memory routines. Enable access before entering any Rust.
    mrs     x0, CurrentEL
    cmp     x0, #4
    b.eq    .Lenable_fp_el1
    cmp     x0, #8
    b.ne    3f
    mrs     x0, cptr_el2
    bic     x0, x0, #0x400          // CPTR_EL2.TFP
    msr     cptr_el2, x0
    b       .Lfp_ready
.Lenable_fp_el1:
    mrs     x0, cpacr_el1
    orr     x0, x0, #0x300000       // CPACR_EL1.FPEN
    msr     cpacr_el1, x0
.Lfp_ready:
    isb
    adr     x1, pocketpreboot_vectors
    mrs     x0, CurrentEL
    cmp     x0, #8
    b.eq    .Lvbar_el2
    msr     vbar_el1, x1
    b       .Lvbar_ready
.Lvbar_el2:
    msr     vbar_el2, x1
.Lvbar_ready:
    isb

    // Rust also emits absolute pointers inside slice/vtable/jump tables.
    // Link as PIE and rebase every R_AARCH64_RELATIVE record before Rust
    // touches those tables. The image is linked at zero and loaded at x20.
    adr     x20, _start
    adrp    x0, __rela_start
    add     x0, x0, :lo12:__rela_start
    adrp    x1, __rela_end
    add     x1, x1, :lo12:__rela_end
.Lrelocate:
    cmp     x0, x1
    b.hs    .Lrelocated
    ldp     x2, x3, [x0]
    ldr     x4, [x0, #16]
    cmp     x3, #1027              // R_AARCH64_RELATIVE, symbol index zero
    b.ne    3f
    add     x2, x20, x2
    add     x4, x20, x4
    str     x4, [x2]
    add     x0, x0, #24
    b       .Lrelocate
.Lrelocated:
    dsb     sy

    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     sp, x0

    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
1:
    cmp     x0, x1
    b.hs    2f
    str     xzr, [x0], #8
    b       1b
2:
.ifdef PB_RAM_TRACE
    adrp    x16, pocketpreboot_ram_trace
    add     x16, x16, :lo12:pocketpreboot_ram_trace
    mov     x17, #2
    pb_trace_store x17, 8
    dsb     sy
.endif
    mov     x0, x19
    bl      pocketpreboot_main

3:
    wfe
    b       3b
