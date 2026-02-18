/* ==========================================================================
 * RISC-V Root of Trust — Memory Map
 *
 * Physical memory layout for a CFI-hardened RoT with PMP isolation.
 * Designed for RV32IMAC + Zicfilp + Zicfiss + PMP.
 *
 * All regions are sized as powers-of-2 and naturally aligned so they can
 * be expressed as single NAPOT PMP entries (most efficient encoding).
 * ========================================================================== */

MEMORY
{
    /* ── M-mode (Root of Trust) regions ────────────────────────────────── */

    /* Boot ROM / M-mode code — immutable after manufacturing.
     * PMP: M=RX, U=none.  Locked so even M-mode cannot write. */
    ROM         : ORIGIN = 0x80000000, LENGTH = 64K

    /* M-mode private RAM — secrets, keys, M-mode stack, M-mode data.
     * PMP: M=RW, U=none.  Never accessible from U-mode. */
    M_RAM       : ORIGIN = 0x80010000, LENGTH = 32K

    /* M-mode shadow stack (hardware Zicfiss).
     * PMP: M=RW, U=none.  Separate from M_RAM for spatial isolation. */
    M_SHADOW    : ORIGIN = 0x80018000, LENGTH = 4K

    /* M-mode software shadow stack.
     * PMP: M=RW, U=none. */
    M_SW_SHADOW : ORIGIN = 0x80019000, LENGTH = 4K

    /* ── U-mode (Application Firmware) regions ─────────────────────────── */

    /* U-mode code — verified/measured firmware loaded by RoT.
     * PMP: M=RWX, U=RX.  Read+Execute only for U-mode. */
    U_CODE      : ORIGIN = 0x80020000, LENGTH = 128K

    /* U-mode read-only data.
     * PMP: M=RW, U=R.  No execute, no write. */
    U_RODATA    : ORIGIN = 0x80040000, LENGTH = 32K

    /* U-mode read-write data (heap, stack, BSS).
     * PMP: M=RW, U=RW.  No execute (W^X enforcement). */
    U_RAM       : ORIGIN = 0x80048000, LENGTH = 64K

    /* U-mode shadow stack (Zicfiss hardware shadow stack region).
     * On Zicfiss hardware: pages have SS PTE attribute (only sspush/sspop
     * can write). With PMP-only: M=RW, U=RW but spatially isolated. */
    U_SHADOW    : ORIGIN = 0x80058000, LENGTH = 4K

    /* U-mode software shadow stack (fallback for cores without Zicfiss).
     * PMP: M=RW, U=RW. */
    U_SW_SHADOW : ORIGIN = 0x80059000, LENGTH = 4K

    /* ── Shared / MMIO regions ─────────────────────────────────────────── */

    /* UART (QEMU virt 16550 at 0x10000000).
     * PMP: M=RW, U=RW (or U=none if UART is M-mode only). */
    UART_MMIO   : ORIGIN = 0x10000000, LENGTH = 4K

    /* QEMU test finisher device.
     * PMP: M=RW, U=none. */
    TEST_FINISH : ORIGIN = 0x00100000, LENGTH = 4K
}

/* ── Region boundary symbols (used by PMP setup & linker sections) ──── */

/* Shadow stack sizes */
_m_shadow_stack_size    = 4K;
_m_sw_shadow_stack_size = 4K;
_u_shadow_stack_size    = 4K;
_u_sw_shadow_stack_size = 4K;

/* U-mode stack */
_u_stack_size = 8K;
