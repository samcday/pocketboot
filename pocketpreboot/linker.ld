ENTRY(_start)

SECTIONS
{
    . = 0;
    __image_start = .;

    .text ALIGN(8) : {
        KEEP(*(.text.header))
        *(.text .text.*)
    }

    .rodata ALIGN(8) : {
        *(.rodata .rodata.*)
    }

    .data ALIGN(8) : {
        *(.data .data.*)
        *(.got .got.*)
    }

    .rela.dyn ALIGN(8) : {
        __rela_start = .;
        *(.rela.dyn)
        __rela_end = .;
    }

    .dynsym ALIGN(8) : { *(.dynsym) }
    .dynstr ALIGN(8) : { *(.dynstr) }
    .hash ALIGN(8) : { *(.hash) }
    .gnu.hash ALIGN(8) : { *(.gnu.hash) }
    .dynamic ALIGN(8) : { *(.dynamic) }

    .bss ALIGN(64) (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    .stack ALIGN(16) (NOLOAD) : {
        . = . + 0x4000;
        __stack_top = .;
        . = . + 0x1000;
        __exception_stack_top = .;
        KEEP(*(.pb_ram_trace))
    }

    __image_end = .;
    __image_size = __image_end - __image_start;

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame*)
        *(.note*)
        *(.ARM.attributes)
    }
}
