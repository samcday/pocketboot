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
    mov     x19, x0

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
    mov     x0, x19
    bl      pocketpreboot_main

3:
    wfe
    b       3b
