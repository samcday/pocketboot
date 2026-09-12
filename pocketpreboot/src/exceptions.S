.section .text.exception_vectors, "ax"
.balign 2048
.global pocketpreboot_vectors
pocketpreboot_vectors:
    .rept 16
    b       .Lexception
    .balign 128
    .endr

.Lexception:
    msr     daifset, #0xf
    mrs     x3, CurrentEL
    cmp     x3, #8
    b.eq    .Lexception_el2
    mrs     x0, esr_el1
    mrs     x1, elr_el1
    mrs     x2, far_el1
    b       .Lreport_exception
.Lexception_el2:
    mrs     x0, esr_el2
    mrs     x1, elr_el2
    mrs     x2, far_el2
.Lreport_exception:
.ifdef PB_RAM_TRACE
    adrp    x16, pocketpreboot_ram_trace
    add     x16, x16, :lo12:pocketpreboot_ram_trace
    pb_trace_store x0, 48
    pb_trace_store x1, 56
    pb_trace_store x2, 64
    pb_trace_store x3, 32
    mov     x5, #0xe001
    pb_trace_store x5, 8
    dsb     sy
.endif
    adrp    x4, __exception_stack_top
    add     x4, x4, :lo12:__exception_stack_top
    mov     sp, x4
    bl      pocketpreboot_exception
1:
    wfe
    b       1b
