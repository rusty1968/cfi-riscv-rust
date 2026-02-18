ENTRY(_start)

SECTIONS
{
    /* Code — starts at FLASH */
    .text : ALIGN(4) {
        _text_start = .;
        KEEP(*(.text.init))     /* Boot code first */
        *(.text .text.*)
        _text_end = .;
    } > FLASH

    /* Read-only data */
    .rodata : ALIGN(4) {
        *(.rodata .rodata.*)
    } > FLASH

    /* Initialized data (loaded from FLASH, copied to RAM) */
    .data : ALIGN(4) {
        _data_start = .;
        *(.data .data.*)
        _data_end = .;
    } > RAM AT > FLASH
    _data_load = LOADADDR(.data);

    /* BSS */
    .bss (NOLOAD) : ALIGN(4) {
        _bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        _bss_end = .;
    } > RAM

    /* Regular stack (grows down) */
    .stack (NOLOAD) : ALIGN(16) {
        _stack_bottom = .;
        . += 8K;
        _stack_top = .;
    } > RAM

    /* Hardware shadow stack (Zicfiss) — separate region */
    .shadow_stack (NOLOAD) : ALIGN(4) {
        _shadow_stack_bottom = .;
        . += _shadow_stack_size;
        _shadow_stack_top = .;
    } > RAM

    /* Software shadow stack (fallback) — separate region */
    .sw_shadow_stack (NOLOAD) : ALIGN(4) {
        _sw_shadow_stack_bottom = .;
        . += _sw_shadow_stack_size;
        _sw_shadow_stack_top = .;
    } > RAM

    /* Discard debug-related sections that cause issues */
    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
