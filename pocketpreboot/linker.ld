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
    }

    .bss ALIGN(16) (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    .stack ALIGN(16) (NOLOAD) : {
        . = . + 0x4000;
        __stack_top = .;
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
