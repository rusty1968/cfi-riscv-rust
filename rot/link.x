/* ==========================================================================
 * RISC-V Root of Trust — Linker Script
 *
 * Lays out M-mode (RoT kernel) and U-mode (application) code/data into
 * PMP-aligned memory regions.  Each section maps to a specific PMP entry
 * with distinct permissions.
 *
 * Key invariants:
 *   - M-mode code is in ROM (immutable, execute-only from U-mode perspective)
 *   - M-mode data/secrets are in M_RAM (invisible to U-mode)
 *   - Shadow stacks are in dedicated regions (spatially isolated)
 *   - U-mode code is RX-only (W^X: no writable+executable pages)
 *   - U-mode data is RW-only (no execute)
 * ========================================================================== */

ENTRY(_start)

SECTIONS
{
    /* ==================================================================
     * M-MODE (Root of Trust) SECTIONS
     * ================================================================== */

    /* M-mode boot + kernel code — placed in ROM.
     * .text.init MUST come first (_start entry point). */
    .text : ALIGN(4) {
        _m_text_start = .;
        KEEP(*(.text.init))
        KEEP(*(.text.trap))
        *(.text .text.*)
        _m_text_end = .;
    } > ROM

    /* M-mode read-only data (in ROM, after code) */
    .rodata : ALIGN(4) {
        _m_rodata_start = .;
        *(.rodata .rodata.*)
        _m_rodata_end = .;
    } > ROM

    /* M-mode initialized data (loaded from ROM, lives in M_RAM) */
    .data : ALIGN(4) {
        _m_data_start = .;
        *(.data .data.*)
        _m_data_end = .;
    } > M_RAM AT > ROM
    _m_data_load = LOADADDR(.data);

    /* M-mode BSS (zero-initialized, in M_RAM) */
    .bss (NOLOAD) : ALIGN(4) {
        _m_bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        _m_bss_end = .;
    } > M_RAM

    /* M-mode stack (in M_RAM, grows down) */
    .m_stack (NOLOAD) : ALIGN(16) {
        _m_stack_bottom = .;
        . += 4K;
        _m_stack_top = .;
    } > M_RAM

    /* ==================================================================
     * M-MODE SHADOW STACKS (dedicated PMP regions)
     * ================================================================== */

    /* Hardware shadow stack for M-mode (Zicfiss) */
    .m_shadow_stack (NOLOAD) : ALIGN(4) {
        _m_shadow_stack_bottom = .;
        . += _m_shadow_stack_size;
        _m_shadow_stack_top = .;
    } > M_SHADOW

    /* Software shadow stack for M-mode (fallback) */
    .m_sw_shadow_stack (NOLOAD) : ALIGN(4) {
        _m_sw_shadow_stack_bottom = .;
        . += _m_sw_shadow_stack_size;
        _m_sw_shadow_stack_top = .;
    } > M_SW_SHADOW

    /* ==================================================================
     * U-MODE (Application Firmware) SECTIONS
     *
     * In a real RoT these would come from a separate ELF (the firmware
     * image), loaded and measured by M-mode before launch.  For this
     * reference we link them statically with distinct sections.
     * ================================================================== */

    /* U-mode code — RX only (no write from U-mode).
     * Landing pads enforced by Zicfilp here. */
    .u_text : ALIGN(4) {
        _u_text_start = .;
        *(.u_text .u_text.*)
        _u_text_end = .;
    } > U_CODE

    /* U-mode read-only data */
    .u_rodata : ALIGN(4) {
        _u_rodata_start = .;
        *(.u_rodata .u_rodata.*)
        _u_rodata_end = .;
    } > U_RODATA

    /* U-mode read-write data */
    .u_data : ALIGN(4) {
        _u_data_start = .;
        *(.u_data .u_data.*)
        _u_data_end = .;
    } > U_RAM

    /* U-mode BSS */
    .u_bss (NOLOAD) : ALIGN(4) {
        _u_bss_start = .;
        *(.u_bss .u_bss.*)
        _u_bss_end = .;
    } > U_RAM

    /* U-mode stack (in U_RAM, grows down) */
    .u_stack (NOLOAD) : ALIGN(16) {
        _u_stack_bottom = .;
        . += _u_stack_size;
        _u_stack_top = .;
    } > U_RAM

    /* ==================================================================
     * U-MODE SHADOW STACKS (dedicated PMP regions)
     * ================================================================== */

    /* Hardware shadow stack for U-mode (Zicfiss) */
    .u_shadow_stack (NOLOAD) : ALIGN(4) {
        _u_shadow_stack_bottom = .;
        . += _u_shadow_stack_size;
        _u_shadow_stack_top = .;
    } > U_SHADOW

    /* Software shadow stack for U-mode */
    .u_sw_shadow_stack (NOLOAD) : ALIGN(4) {
        _u_sw_shadow_stack_bottom = .;
        . += _u_sw_shadow_stack_size;
        _u_sw_shadow_stack_top = .;
    } > U_SW_SHADOW

    /* ==================================================================
     * DISCARD
     * ================================================================== */
    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
