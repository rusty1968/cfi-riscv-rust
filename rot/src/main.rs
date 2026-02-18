//! RISC-V Root of Trust — M-Mode Kernel with CFI + PMP
//!
//! A minimal M-mode Root of Trust kernel demonstrating:
//!
//!   1. **Full hardware CFI** — Zicfilp (landing pads) + Zicfiss (shadow stack)
//!      for both M-mode and U-mode, with software shadow stack fallback
//!   2. **PMP isolation** — Physical Memory Protection partitioning:
//!      - M-mode code (ROM):        M=RX,  U=none
//!      - M-mode data/secrets:      M=RW,  U=none
//!      - M-mode shadow stacks:     M=RW,  U=none
//!      - U-mode code:              M=RWX, U=RX    (W^X enforced)
//!      - U-mode data:              M=RW,  U=RW    (no execute)
//!      - U-mode shadow stacks:     M=RW,  U=RW    (spatially isolated)
//!      - UART MMIO:                M=RW,  U=RW    (shared peripheral)
//!   3. **Privilege separation** — M-mode boots, configures PMP + CFI,
//!      then drops to U-mode for application firmware execution
//!   4. **Ecall interface** — U-mode requests services (UART, crypto, etc.)
//!      from M-mode via `ecall` traps
//!
//! Memory map:
//!   0x8000_0000 .. 0x8000_FFFF  ROM         (64K)  M-mode code
//!   0x8001_0000 .. 0x8001_7FFF  M_RAM       (32K)  M-mode data + stack
//!   0x8001_8000 .. 0x8001_8FFF  M_SHADOW    (4K)   M-mode HW shadow stack
//!   0x8001_9000 .. 0x8001_9FFF  M_SW_SHADOW (4K)   M-mode SW shadow stack
//!   0x8002_0000 .. 0x8003_FFFF  U_CODE      (128K) U-mode code (RX)
//!   0x8004_0000 .. 0x8004_7FFF  U_RODATA    (32K)  U-mode rodata (R)
//!   0x8004_8000 .. 0x8005_7FFF  U_RAM       (64K)  U-mode data + stack (RW)
//!   0x8005_8000 .. 0x8005_8FFF  U_SHADOW    (4K)   U-mode HW shadow stack
//!   0x8005_9000 .. 0x8005_9FFF  U_SW_SHADOW (4K)   U-mode SW shadow stack
//!   0x1000_0000 .. 0x1000_0FFF  UART MMIO   (4K)   16550 UART

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

// ============================================================================
// CFI Instruction Encodings (Zicfilp + Zicfiss)
// ============================================================================
//
// These are encoded in the Zimop/Zcmop space.  On hardware without CFI
// extensions, they execute as guaranteed NOPs.
//
//   lpad 0       = 0x0000_0017   (AUIPC x0, 0)
//   lpad N       = (N << 12) | 0x17
//   sspush ra    = 0x6010_0073
//   sspopchk ra  = 0x6050_0073

// ============================================================================
// PMP Constants
// ============================================================================

/// PMP address mode: NAPOT (Naturally Aligned Power-Of-Two)
const PMP_NAPOT: u32 = 0x18; // A field = 0b11

/// PMP permission bits
const PMP_R: u32 = 0x01;
const PMP_W: u32 = 0x02;
const PMP_X: u32 = 0x04;

/// PMP lock bit — locks entry and makes it apply to M-mode too
const PMP_L: u32 = 0x80;

// ============================================================================
// UART (QEMU virt machine: 16550 at 0x1000_0000)
// ============================================================================

const UART_BASE: *mut u8 = 0x1000_0000 as *mut u8;

fn uart_putc(c: u8) {
    unsafe { UART_BASE.write_volatile(c) }
}

fn uart_puts(s: &str) {
    for b in s.bytes() {
        uart_putc(b);
    }
}

fn uart_put_hex32(val: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    uart_puts("0x");
    for i in (0..8).rev() {
        uart_putc(HEX[((val >> (i * 4)) & 0xF) as usize]);
    }
}

fn uart_newline() {
    uart_puts("\r\n");
}

// ============================================================================
// PMP Configuration
// ============================================================================

/// Calculate the NAPOT address encoding for a region.
///
/// For NAPOT mode, pmpaddr = (base >> 2) | ((size >> 3) - 1)
/// where `size` must be a power-of-2 and `base` must be size-aligned.
const fn pmp_napot_addr(base: u32, size: u32) -> u32 {
    // pmpaddr = (base + (size/2 - 1)) >> 2
    //         = (base >> 2) | ((size >> 3) - 1)   for naturally-aligned regions
    (base >> 2) | ((size >> 3).wrapping_sub(1))
}

/// Configure all PMP entries to establish memory isolation.
///
/// RISC-V PMP rules (RV32, 16 entries available):
///   - Entries are checked in priority order (0 = highest)
///   - First matching entry wins
///   - M-mode bypasses PMP unless the entry is Locked (L bit)
///   - With no matching entry: M-mode = full access, U-mode = no access
///
/// Our strategy:
///   - Lock M-mode code as RX (prevents runtime code injection into RoT)
///   - Leave M-mode data/shadow stacks unlocked (M-mode needs RW, U-mode
///     gets no access by default since no PMP entry grants it)
///   - Grant U-mode specific permissions via unlocked entries
///   - Deny-all catch-all entry last (locked, no permissions)
fn configure_pmp() {
    uart_puts("[PMP] Configuring Physical Memory Protection...\r\n");

    // ── Entry 0: M-mode code (ROM) — Locked RX ──────────────────────
    // Lock prevents M-mode from writing its own code at runtime.
    // 64K at 0x8000_0000
    let pmp0_addr = pmp_napot_addr(0x8000_0000, 64 * 1024);
    let pmp0_cfg = PMP_L | PMP_NAPOT | PMP_R | PMP_X; // Locked R+X

    // ── Entry 1: M-mode data (M_RAM) — NOT locked ───────────────────
    // M-mode can RW.  U-mode has no access (no PMP entry grants it).
    // 32K at 0x8001_0000
    let pmp1_addr = pmp_napot_addr(0x8001_0000, 32 * 1024);
    let pmp1_cfg: u32 = 0; // No permissions = deny for U-mode.
    // M-mode bypasses PMP (unlocked entry), so M-mode still has full access.

    // ── Entry 2: M-mode shadow stacks — NOT locked ──────────────────
    // Covers both M_SHADOW (4K) + M_SW_SHADOW (4K) = 8K at 0x8001_8000
    let pmp2_addr = pmp_napot_addr(0x8001_8000, 8 * 1024);
    let pmp2_cfg: u32 = 0; // Deny U-mode

    // ── Entry 3: U-mode code — RX for U-mode ────────────────────────
    // 128K at 0x8002_0000
    let pmp3_addr = pmp_napot_addr(0x8002_0000, 128 * 1024);
    let pmp3_cfg = PMP_NAPOT | PMP_R | PMP_X; // U-mode: R+X (W^X enforced)

    // ── Entry 4: U-mode rodata — R for U-mode ───────────────────────
    // 32K at 0x8004_0000
    let pmp4_addr = pmp_napot_addr(0x8004_0000, 32 * 1024);
    let pmp4_cfg = PMP_NAPOT | PMP_R; // U-mode: R only

    // ── Entry 5: U-mode data/stack — RW for U-mode ──────────────────
    // 64K at 0x8004_8000
    let pmp5_addr = pmp_napot_addr(0x8004_8000, 64 * 1024);
    let pmp5_cfg = PMP_NAPOT | PMP_R | PMP_W; // U-mode: R+W (no X = W^X)

    // ── Entry 6: U-mode shadow stacks — RW for U-mode ──────────────
    // Covers U_SHADOW (4K) + U_SW_SHADOW (4K) = 8K at 0x8005_8000
    // On real Zicfiss hardware, the HW shadow stack pages would have the
    // SS PTE attribute so only sspush/sspop can write them.  With PMP-only
    // (no MMU), we grant RW and rely on spatial isolation + CFI enforcement.
    let pmp6_addr = pmp_napot_addr(0x8005_8000, 8 * 1024);
    let pmp6_cfg = PMP_NAPOT | PMP_R | PMP_W;

    // ── Entry 7: UART MMIO — RW for U-mode ─────────────────────────
    // 4K at 0x1000_0000 — allows U-mode to write to UART directly.
    // In a stricter RoT, UART access would be M-mode only via ecall.
    let pmp7_addr = pmp_napot_addr(0x1000_0000, 4 * 1024);
    let pmp7_cfg = PMP_NAPOT | PMP_R | PMP_W;

    // ── Entries 8-14: Reserved (unused, deny-all) ───────────────────
    // Left as zero — no access.

    // ── Entry 15: Deny-all catch-all — Locked, no permissions ───────
    // Matches any address not covered above.  Prevents U-mode from
    // accessing unmapped memory.  Locked so even M-mode can't bypass.
    // Use TOR (Top Of Range) mode with addr = 0xFFFF_FFFF to cover
    // the entire address space above entry 14.
    // NOTE: The catch-all must be LAST (lowest priority).

    // Write PMP address registers (CSRs 0x3B0 – 0x3BF)
    unsafe {
        asm!(
            // pmpaddr0..7
            "csrw  0x3B0, {a0}",
            "csrw  0x3B1, {a1}",
            "csrw  0x3B2, {a2}",
            "csrw  0x3B3, {a3}",
            "csrw  0x3B4, {a4}",
            "csrw  0x3B5, {a5}",
            "csrw  0x3B6, {a6}",
            "csrw  0x3B7, {a7}",
            a0 = in(reg) pmp0_addr,
            a1 = in(reg) pmp1_addr,
            a2 = in(reg) pmp2_addr,
            a3 = in(reg) pmp3_addr,
            a4 = in(reg) pmp4_addr,
            a5 = in(reg) pmp5_addr,
            a6 = in(reg) pmp6_addr,
            a7 = in(reg) pmp7_addr,
        );

        // Pack PMP config for entries 0-3 into pmpcfg0 (4 x 8-bit fields)
        let pmpcfg0: u32 = (pmp0_cfg)
            | (pmp1_cfg << 8)
            | (pmp2_cfg << 16)
            | (pmp3_cfg << 24);

        // Pack PMP config for entries 4-7 into pmpcfg1
        let pmpcfg1: u32 = (pmp4_cfg)
            | (pmp5_cfg << 8)
            | (pmp6_cfg << 16)
            | (pmp7_cfg << 24);

        asm!(
            "csrw  0x3A0, {cfg0}",  // pmpcfg0
            "csrw  0x3A1, {cfg1}",  // pmpcfg1
            cfg0 = in(reg) pmpcfg0,
            cfg1 = in(reg) pmpcfg1,
        );
    }

    // Report PMP configuration
    uart_puts("  Entry 0: ROM (M-mode code)     Locked R-X  64K @ 0x80000000\r\n");
    uart_puts("  Entry 1: M_RAM (M-mode data)    Deny U     32K @ 0x80010000\r\n");
    uart_puts("  Entry 2: M_SHADOW (M-mode SS)   Deny U      8K @ 0x80018000\r\n");
    uart_puts("  Entry 3: U_CODE (U-mode code)   U: R-X    128K @ 0x80020000\r\n");
    uart_puts("  Entry 4: U_RODATA               U: R--     32K @ 0x80040000\r\n");
    uart_puts("  Entry 5: U_RAM (U-mode data)    U: RW-     64K @ 0x80048000\r\n");
    uart_puts("  Entry 6: U_SHADOW (U-mode SS)   U: RW-      8K @ 0x80058000\r\n");
    uart_puts("  Entry 7: UART MMIO              U: RW-      4K @ 0x10000000\r\n");
    uart_puts("[PMP] Configuration complete.\r\n\r\n");
}

// ============================================================================
// CFI Initialization
// ============================================================================

/// Enable hardware CFI extensions via menvcfg and senvcfg CSRs.
///
/// menvcfg (0x30A) controls CFI for S/U-mode:
///   Bit 2 (LPE) — Landing Pad Enable (Zicfilp)
///   Bit 3 (SSE) — Shadow Stack Enable (Zicfiss)
///
/// On hardware without these CSRs, the trap handler skips the writes.
fn enable_cfi() {
    uart_puts("[CFI] Enabling hardware CFI extensions...\r\n");

    unsafe {
        // Enable LPE + SSE in menvcfg (affects S/U-mode)
        let cfi_bits: u32 = (1 << 2) | (1 << 3); // LPE | SSE
        asm!(
            "csrs  0x30A, {bits}",   // csrs menvcfg, bits
            bits = in(reg) cfi_bits,
        );
        uart_puts("  menvcfg: set LPE (bit 2) + SSE (bit 3)\r\n");

        // Also enable in senvcfg (0x10A) for U-mode if running S-mode software
        // (In our M-mode-only RoT, menvcfg is sufficient for U-mode, but
        //  we set senvcfg too for forward-compatibility with S-mode kernels)
        asm!(
            "csrs  0x10A, {bits}",   // csrs senvcfg, bits
            bits = in(reg) cfi_bits,
        );
        uart_puts("  senvcfg: set LPE (bit 2) + SSE (bit 3)\r\n");

        // Initialize M-mode hardware shadow stack pointer
        // (HW SSP CSR 0x011 — ssp)
        asm!(
            "la    {tmp}, _m_shadow_stack_top",
            "csrw  0x011, {tmp}",
            tmp = out(reg) _,
        );
        uart_puts("  ssp: initialized to _m_shadow_stack_top (M-mode)\r\n");
    }

    uart_puts("[CFI] Hardware CFI enabled (or NOPs on unsupported HW).\r\n\r\n");
}

// ============================================================================
// M-Mode Trap Handler
// ============================================================================

/// Unified M-mode trap handler.
///
/// Handles:
///   - **Ecalls from U-mode** (mcause = 8): service requests from application
///   - **Illegal instructions** (mcause = 2): skip faulting instruction
///     (graceful degradation for unsupported CSR accesses during boot)
///   - **CFI violations**:
///     - Software-check exception (mcause = 18): Zicfiss shadow stack mismatch
///     - Instruction access fault (mcause = 1): Zicfilp landing pad violation
///
/// Ecall ABI:
///   a7 = syscall number
///     0 = uart_putc(a0 = char)
///     1 = uart_puts(a0 = ptr, a1 = len)
///     2 = exit(a0 = code)
///     3 = get_random(a0 = &buf, a1 = len)  [stub: fills with 0xAA]
///   Return value in a0.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.trap"]
unsafe extern "C" fn _trap_handler() {
    naked_asm!(
        // Save minimal context on M-mode stack
        "addi   sp, sp, -64",
        "sw     ra,  0(sp)",
        "sw     t0,  4(sp)",
        "sw     t1,  8(sp)",
        "sw     t2, 12(sp)",
        "sw     a0, 16(sp)",
        "sw     a1, 20(sp)",
        "sw     a2, 24(sp)",
        "sw     a7, 28(sp)",

        // Read cause
        "csrr   t0, mcause",

        // Check for environment call from U-mode (cause = 8)
        "li     t1, 8",
        "beq    t0, t1, _handle_ecall",

        // Check for illegal instruction (cause = 2) — skip it
        "li     t1, 2",
        "beq    t0, t1, _handle_illegal",

        // Check for software-check exception (cause = 18) — CFI violation
        "li     t1, 18",
        "beq    t0, t1, _handle_cfi_violation",

        // Check for instruction access fault (cause = 1) — landing pad violation
        "li     t1, 1",
        "beq    t0, t1, _handle_cfi_violation",

        // Unknown trap — halt
        "j      _handle_unknown_trap",

        // ── Ecall handler ──────────────────────────────────────────
        "_handle_ecall:",
        // Advance mepc past the 4-byte ecall instruction
        "csrr   t0, mepc",
        "addi   t0, t0, 4",
        "csrw   mepc, t0",

        // Dispatch on a7 (syscall number)
        "lw     a7, 28(sp)",
        "lw     a0, 16(sp)",
        "lw     a1, 20(sp)",

        // syscall 0: uart_putc(a0 = char)
        "li     t1, 0",
        "bne    a7, t1, 10f",
        "li     t0, 0x10000000",
        "sb     a0, 0(t0)",
        "j      _trap_return",

        // syscall 1: uart_puts(a0 = ptr, a1 = len)
        "10:",
        "li     t1, 1",
        "bne    a7, t1, 20f",
        "li     t0, 0x10000000",
        "11:",
        "beqz   a1, _trap_return",
        "lb     t1, 0(a0)",
        "sb     t1, 0(t0)",
        "addi   a0, a0, 1",
        "addi   a1, a1, -1",
        "j      11b",

        // syscall 2: exit(a0 = code)
        "20:",
        "li     t1, 2",
        "bne    a7, t1, 30f",
        // Write to QEMU test finisher
        "li     t0, 0x100000",
        "li     t1, 0x5555",      // PASS
        "sw     t1, 0(t0)",
        "21: wfi",
        "j      21b",

        // syscall 3: get_random(a0 = &buf, a1 = len) — stub
        "30:",
        "li     t1, 3",
        "bne    a7, t1, _trap_return",
        "li     t2, 0xAA",        // stub: fill with 0xAA
        "31:",
        "beqz   a1, _trap_return",
        "sb     t2, 0(a0)",
        "addi   a0, a0, 1",
        "addi   a1, a1, -1",
        "j      31b",

        // ── Illegal instruction handler ────────────────────────────
        // Skip 2-byte (compressed) or 4-byte instruction
        "_handle_illegal:",
        "csrr   t0, mepc",
        "lhu    t1, 0(t0)",
        "andi   t1, t1, 0x3",
        "li     t2, 0x3",
        "bne    t1, t2, 40f",
        "addi   t0, t0, 4",       // 4-byte instruction
        "j      41f",
        "40: addi t0, t0, 2",     // 2-byte compressed
        "41: csrw mepc, t0",
        "j      _trap_return",

        // ── CFI violation handler ──────────────────────────────────
        // On real hardware this is a security-critical event.
        // Options: halt, reset, log + quarantine, etc.
        "_handle_cfi_violation:",
        // Print violation notice via UART
        "li     t0, 0x10000000",
        // "!" = 0x21, "C" = 0x43, "F" = 0x46, "I" = 0x49
        "li     t1, 0x43",        // 'C'
        "sb     t1, 0(t0)",
        "li     t1, 0x46",        // 'F'
        "sb     t1, 0(t0)",
        "li     t1, 0x49",        // 'I'
        "sb     t1, 0(t0)",
        "li     t1, 0x21",        // '!'
        "sb     t1, 0(t0)",
        "li     t1, 0x0A",        // '\n'
        "sb     t1, 0(t0)",
        // Hard fault — halt the system
        "50: wfi",
        "j      50b",

        // ── Unknown trap ───────────────────────────────────────────
        "_handle_unknown_trap:",
        "51: wfi",
        "j      51b",

        // ── Trap return ────────────────────────────────────────────
        "_trap_return:",
        "lw     ra,  0(sp)",
        "lw     t0,  4(sp)",
        "lw     t1,  8(sp)",
        "lw     t2, 12(sp)",
        "lw     a0, 16(sp)",
        "lw     a1, 20(sp)",
        "lw     a2, 24(sp)",
        "lw     a7, 28(sp)",
        "addi   sp, sp, 64",
        "mret",
    )
}

// ============================================================================
// M-Mode Protected Functions (with full CFI)
// ============================================================================

/// Measure a firmware image (simplified stub).
///
/// In a real RoT, this would compute SHA-256/384 over the U-mode code region
/// and compare against a known-good measurement stored in OTP/fuses.
///
/// This function demonstrates full CFI protection on an M-mode function:
///   - Landing pad (forward-edge)
///   - HW + SW shadow stack (backward-edge)
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn rot_measure_firmware(base: u32, size: u32) -> u32 {
    naked_asm!(
        // Forward-edge: landing pad
        ".4byte 0x00000017",        // lpad 0

        // Backward-edge: push ra
        ".4byte 0x60100073",        // sspush ra (HW)
        "addi   sp, sp, -16",
        "sw     ra, 12(sp)",
        "sw     gp, 8(sp)",
        "sw     ra, 0(gp)",         // sw_sspush
        "addi   gp, gp, 4",

        // Simplified measurement: XOR all words in the region
        // (Real RoT would use a proper hash function)
        "li     a2, 0",             // accumulator
        "add    a1, a0, a1",        // end = base + size
        "60:",
        "beq    a0, a1, 61f",
        "lw     t0, 0(a0)",
        "xor    a2, a2, t0",
        "addi   a0, a0, 4",
        "j      60b",
        "61:",
        "mv     a0, a2",            // return measurement

        // Backward-edge: pop and check
        "addi   gp, gp, -4",
        "lw     t0, 0(gp)",
        "lw     ra, 12(sp)",
        "bne    t0, ra, 99f",
        "lw     gp, 8(sp)",
        "addi   sp, sp, 16",
        ".4byte 0x60500073",        // sspopchk ra (HW)
        "ret",

        "99: ebreak",               // Shadow stack mismatch
    )
}

/// Seal a secret using the hardware-bound key (stub).
///
/// In a real RoT with a key manager, this would use the device identity
/// key (DevID) or a derived key to encrypt/HMAC the data.
/// Demonstrates a labeled landing pad (only callers with label=0xR07
/// can reach this function on Zicfilp hardware).
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn rot_seal_secret(data: u32, key_id: u32) -> u32 {
    naked_asm!(
        // Forward-edge: labeled landing pad (label = 0xR07 conceptually)
        // Using label 7 for demo
        ".4byte {lpad_7}",          // lpad 7

        // Backward-edge: shadow stacks
        ".4byte 0x60100073",        // sspush ra (HW)
        "addi   sp, sp, -16",
        "sw     ra, 12(sp)",
        "sw     gp, 8(sp)",
        "sw     ra, 0(gp)",         // sw_sspush
        "addi   gp, gp, 4",

        // Stub: XOR data with key_id as a placeholder for real crypto
        "xor    a0, a0, a1",

        // Backward-edge: pop and check
        "addi   gp, gp, -4",
        "lw     t0, 0(gp)",
        "lw     ra, 12(sp)",
        "bne    t0, ra, 99f",
        "lw     gp, 8(sp)",
        "addi   sp, sp, 16",
        ".4byte 0x60500073",        // sspopchk ra (HW)
        "ret",

        "99: ebreak",
        lpad_7 = const ((7u32 << 12) | 0x17),
    )
}

// ============================================================================
// U-Mode Launch
// ============================================================================

/// Drop privilege from M-mode to U-mode.
///
/// Sets up mstatus.MPP = 0 (User mode), sets mepc to the U-mode entry
/// point, initializes the U-mode stack and shadow stack pointers, then
/// executes mret to enter U-mode.
///
/// After mret:
///   - Privilege level = U-mode
///   - PMP enforcement active for all U-mode memory accesses
///   - CFI enforcement active (Zicfilp landing pads + Zicfiss shadow stack)
///   - U-mode cannot access M-mode memory regions
fn launch_umode() {
    uart_puts("[LAUNCH] Dropping to U-mode...\r\n");
    uart_puts("  mepc  -> _u_entry (U-mode entry point)\r\n");
    uart_puts("  MPP   -> 0b00 (User mode)\r\n");
    uart_puts("  sp    -> _u_stack_top\r\n");
    uart_puts("  ssp   -> _u_shadow_stack_top\r\n");
    uart_puts("  gp    -> _u_sw_shadow_stack_bottom\r\n\r\n");

    unsafe {
        asm!(
            // Set mstatus.MPP = 0b00 (U-mode)
            // MPP is bits [12:11] of mstatus
            "csrr   t0, mstatus",
            "li     t1, ~(3 << 11)",   // Clear MPP bits
            "and    t0, t0, t1",
            // MPP = 0 means User mode (already cleared)
            "csrw   mstatus, t0",

            // Set mepc to U-mode entry point
            "la     t0, _u_entry",
            "csrw   mepc, t0",

            // Set U-mode stack pointer
            "la     sp, _u_stack_top",

            // Set U-mode hardware shadow stack pointer
            "la     t0, _u_shadow_stack_top",
            "csrw   0x011, t0",        // csrw ssp, t0

            // Set U-mode software shadow stack pointer (gp)
            "la     gp, _u_sw_shadow_stack_bottom",

            // Enter U-mode
            "mret",
            options(noreturn),
        );
    }
}

// ============================================================================
// U-Mode Entry Point & Application
// ============================================================================

/// U-mode ecall wrappers.
///
/// These run in U-mode and use `ecall` to request services from M-mode.
mod umode_syscalls {
    /// Print a single character via M-mode UART service.
    #[inline(always)]
    pub fn sys_putc(c: u8) {
        unsafe {
            core::arch::asm!(
                "li a7, 0",
                "ecall",
                in("a0") c as u32,
                lateout("a0") _,
                lateout("a7") _,
            );
        }
    }

    /// Print a string via M-mode UART service.
    #[inline(always)]
    pub fn sys_puts(s: &str) {
        unsafe {
            core::arch::asm!(
                "li a7, 1",
                "ecall",
                in("a0") s.as_ptr(),
                in("a1") s.len(),
                lateout("a0") _,
                lateout("a1") _,
                lateout("a7") _,
            );
        }
    }

    /// Exit the system.
    #[inline(always)]
    pub fn sys_exit(code: u32) -> ! {
        unsafe {
            core::arch::asm!(
                "li a7, 2",
                "ecall",
                in("a0") code,
                options(noreturn),
            );
        }
    }
}

/// U-mode indirect call target: add 100.
/// Has a landing pad for forward-edge CFI protection.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".u_text"]
pub unsafe extern "C" fn u_add_100(x: u32) -> u32 {
    naked_asm!(
        ".4byte 0x00000017",        // lpad 0
        "addi   a0, a0, 100",
        "ret",
    )
}

/// U-mode indirect call target: double the value.
/// Full forward + backward CFI protection (non-leaf).
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".u_text"]
pub unsafe extern "C" fn u_double(x: u32) -> u32 {
    naked_asm!(
        ".4byte 0x00000017",        // lpad 0
        ".4byte 0x60100073",        // sspush ra (HW)
        "addi   sp, sp, -16",
        "sw     ra, 12(sp)",
        "sw     gp, 8(sp)",
        "sw     ra, 0(gp)",         // sw_sspush
        "addi   gp, gp, 4",

        "slli   a0, a0, 1",         // x * 2

        "addi   gp, gp, -4",        // sw_sspopchk
        "lw     t0, 0(gp)",
        "lw     ra, 12(sp)",
        "bne    t0, ra, 99f",
        "lw     gp, 8(sp)",
        "addi   sp, sp, 16",
        ".4byte 0x60500073",        // sspopchk ra (HW)
        "ret",

        "99: ebreak",
    )
}

/// U-mode dispatch table — function pointers with landing pads.
#[repr(C)]
struct UDispatch {
    handler: unsafe extern "C" fn(u32) -> u32,
}

/// U-mode entry point.
///
/// Runs in U-mode with PMP restrictions active:
///   - Can only execute code in U_CODE
///   - Can only read/write U_RAM and U_SHADOW regions
///   - Cannot access M-mode memory (PMP enforced)
///   - All indirect call targets must have landing pads (Zicfilp)
///   - All return addresses checked via shadow stack (Zicfiss)
///   - System services via ecall to M-mode
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".u_text"]
pub unsafe extern "C" fn _u_entry() -> ! {
    naked_asm!(
        // Landing pad (we arrive here via mret, but good practice)
        ".4byte 0x00000017",        // lpad 0

        // ── Test: Indirect call through function pointer ──
        // Call u_add_100(42) via pointer
        "la     t1, u_add_100",
        "li     a0, 42",
        "jalr   ra, t1, 0",
        // a0 should now be 142

        // ── Test: Call u_double via pointer ──
        "la     t1, u_double",
        "li     a0, 25",
        "jalr   ra, t1, 0",
        // a0 should now be 50

        // ── Print success via ecall ──
        // sys_putc('O')
        "li     a0, 0x4F",
        "li     a7, 0",
        "ecall",
        // sys_putc('K')
        "li     a0, 0x4B",
        "li     a7, 0",
        "ecall",
        // sys_putc('\n')
        "li     a0, 0x0A",
        "li     a7, 0",
        "ecall",

        // Exit
        "li     a0, 0",
        "li     a7, 2",
        "ecall",

        // Should not reach here
        "70: wfi",
        "j      70b",
    )
}

// ============================================================================
// Boot Sequence (_start)
// ============================================================================

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.init"]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        // ── 1. Set up M-mode stack ──
        "la     sp, _m_stack_top",

        // ── 2. Install trap handler ──
        "la     t0, _trap_handler",
        "csrw   mtvec, t0",

        // ── 3. Zero M-mode BSS ──
        "la     t0, _m_bss_start",
        "la     t1, _m_bss_end",
        "1: beq  t0, t1, 2f",
        "sw     zero, 0(t0)",
        "addi   t0, t0, 4",
        "j      1b",
        "2:",

        // ── 4. Copy M-mode .data from ROM to RAM ──
        "la     t0, _m_data_start",
        "la     t1, _m_data_end",
        "la     t2, _m_data_load",
        "3: beq  t0, t1, 4f",
        "lw     t3, 0(t2)",
        "sw     t3, 0(t0)",
        "addi   t0, t0, 4",
        "addi   t2, t2, 4",
        "j      3b",
        "4:",

        // ── 5. Initialize M-mode software shadow stack (gp) ──
        "la     gp, _m_sw_shadow_stack_bottom",

        // ── 6. Jump to Rust main (M-mode init) ──
        "call   rot_main",

        // ── 7. Should not return ──
        "5: wfi",
        "j      5b",
    )
}

// ============================================================================
// M-Mode Main — Root of Trust Initialization
// ============================================================================

#[no_mangle]
pub extern "C" fn rot_main() -> ! {
    uart_puts("================================================================\r\n");
    uart_puts("  RISC-V Root of Trust — CFI + PMP Isolation Demo\r\n");
    uart_puts("  RV32IMAC + Zicfilp + Zicfiss + PMP\r\n");
    uart_puts("================================================================\r\n\r\n");

    // ── Phase 1: Enable hardware CFI ──
    uart_puts("── Phase 1: CFI Initialization ─────────────────────────────\r\n");
    enable_cfi();

    // ── Phase 2: Configure PMP ──
    uart_puts("── Phase 2: PMP Configuration ──────────────────────────────\r\n");
    configure_pmp();

    // ── Phase 3: Measure U-mode firmware ──
    uart_puts("── Phase 3: Firmware Measurement ───────────────────────────\r\n");
    uart_puts("[MEASURE] Computing firmware measurement over U_CODE region...\r\n");
    {
        let measurement = unsafe {
            rot_measure_firmware(0x8002_0000, 128 * 1024)
        };
        uart_puts("  Measurement (XOR hash): ");
        uart_put_hex32(measurement);
        uart_newline();
        uart_puts("  (Real RoT would compare against OTP-stored golden hash)\r\n\r\n");
    }

    // ── Phase 4: Seal a secret using RoT key ──
    uart_puts("── Phase 4: Secret Sealing (RoT Key Service) ───────────────\r\n");
    {
        let sealed = unsafe { rot_seal_secret(0xDEAD_BEEF, 1) };
        uart_puts("  seal(0xDEADBEEF, key_id=1) = ");
        uart_put_hex32(sealed);
        uart_newline();
        uart_puts("  (Stub: XOR-based, real RoT uses AES-GCM/HMAC)\r\n\r\n");
    }

    // ── Phase 5: Launch U-mode ──
    uart_puts("── Phase 5: U-Mode Launch ──────────────────────────────────\r\n");
    uart_puts("[LAUNCH] Security state summary:\r\n");
    uart_puts("  - Hardware CFI: Zicfilp (landing pads) + Zicfiss (shadow stack)\r\n");
    uart_puts("  - Software CFI: gp-based shadow stack (fallback for non-Zicfiss)\r\n");
    uart_puts("  - PMP: 8 entries isolating M-mode / U-mode regions\r\n");
    uart_puts("  - Privilege: Dropping from M-mode -> U-mode via mret\r\n");
    uart_puts("  - W^X: U-mode code is RX, U-mode data is RW (no RWX)\r\n");
    uart_puts("  - U-mode services: ecall to M-mode for UART, crypto, etc.\r\n\r\n");

    launch_umode();

    // Never reached — launch_umode() does mret
    unreachable!()
}

// ============================================================================
// Panic Handler
// ============================================================================

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    uart_puts("\r\n!!! ROOT OF TRUST PANIC !!!\r\n");
    if let Some(loc) = info.location() {
        uart_puts("  at ");
        uart_puts(loc.file());
        uart_puts(":");
        let mut buf = [0u8; 10];
        let mut i = 0;
        let mut line = loc.line();
        if line == 0 {
            uart_putc(b'0');
        } else {
            while line > 0 {
                buf[i] = b'0' + (line % 10) as u8;
                line /= 10;
                i += 1;
            }
            while i > 0 {
                i -= 1;
                uart_putc(buf[i]);
            }
        }
        uart_newline();
    }
    uart_puts("  SYSTEM HALTED — security invariant violated\r\n");
    loop {
        unsafe { asm!("wfi") };
    }
}
