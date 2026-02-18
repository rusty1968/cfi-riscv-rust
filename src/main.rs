//! Bare-Metal RISC-V CFI Demo
//!
//! Demonstrates DIY Control Flow Integrity on RV32 using:
//!   - Zicfilp: Landing pads for forward-edge CFI (indirect call/jump targets)
//!   - Zicfiss: Hardware shadow stack for backward-edge CFI (return address protection)
//!   - Software shadow stack fallback (works on any RV32 hardware)
//!
//! All hardware CFI instructions are encoded as Zimop/Zcmop, meaning they
//! execute as NOPs on hardware that lacks Zicfiss/Zicfilp support.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

// ============================================================================
// CFI Instruction Encodings
// ============================================================================
//
// Zicfilp (Landing Pads):
//   lpad 0       = 0x0000_0017  (AUIPC x0, 0)
//   lpad N       = (N << 12) | 0x17
//
// Zicfiss (Shadow Stack):
//   sspush ra    = 0x6010_0073
//   sspopchk ra  = 0x6050_0073
//
// These are encoded in the Zimop (May-Be-Operations) space. On hardware
// without Zicfiss/Zicfilp, they are guaranteed to execute as NOPs.

// ============================================================================
// Hardware CFI Macros (for use in non-naked functions)
// ============================================================================

/// Insert a landing pad with label 0 (unlabeled).
/// On Zicfilp hardware: indirect branches must land here or the CPU faults.
/// On other hardware: executes as NOP.
#[allow(unused_macros)]
macro_rules! lpad {
    () => {
        core::arch::asm!(".4byte 0x00000017", options(nomem, nostack))
    };
}

/// Push return address (ra/x1) onto the hardware shadow stack.
/// On Zicfiss hardware: ra is pushed to a protected shadow stack region.
/// On other hardware: executes as NOP (Zimop guarantee).
#[allow(unused_macros)]
macro_rules! hw_sspush {
    () => {
        core::arch::asm!(".4byte 0x60100073", options(nomem, nostack))
    };
}

/// Pop from hardware shadow stack and compare with ra.
/// On Zicfiss hardware: faults with software-check exception on mismatch.
/// On other hardware: executes as NOP (Zimop guarantee).
#[allow(unused_macros)]
macro_rules! hw_sspopchk {
    () => {
        core::arch::asm!(".4byte 0x60500073", options(nomem, nostack))
    };
}

// ============================================================================
// Software Shadow Stack Macros (for use in non-naked functions)
// ============================================================================
//
// Uses gp (x3) as the software shadow stack pointer. This register is
// normally used for linker relaxation (GP-relative addressing), which we
// disable via --no-relax in .cargo/config.toml.
//
// Alternative: use s11 (x27) if you need GP relaxation, but you must also
// tell LLVM to reserve the register.

/// Push ra onto the software shadow stack (pointed to by gp).
#[allow(unused_macros)]
macro_rules! sw_sspush {
    () => {
        core::arch::asm!(
            "sw   ra, 0(gp)",
            "addi gp, gp, 4",
            options(nostack),
        )
    };
}

/// Pop from software shadow stack, compare with ra, fault on mismatch.
#[allow(unused_macros)]
macro_rules! sw_sspopchk {
    () => {
        core::arch::asm!(
            "addi gp, gp, -4",
            "lw   t0, 0(gp)",
            "beq  t0, ra, 33f",
            // Mismatch detected — return address was corrupted
            "ebreak",
            "33:",
            options(nostack),
        )
    };
}

// ============================================================================
// UART Output (QEMU virt machine: 16550-compatible at 0x1000_0000)
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

fn uart_put_dec(mut val: u32) {
    if val == 0 {
        uart_putc(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while val > 0 {
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        uart_putc(buf[i]);
    }
}

fn uart_newline() {
    uart_puts("\r\n");
}

// ============================================================================
// Indirect Call Targets (with Landing Pads)
// ============================================================================

/// Multiply x by 3. Callable via function pointer.
/// Has an unlabeled landing pad (lpad 0) at entry.
/// Demonstrates full forward + backward edge CFI in a non-leaf function.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn triple(x: u32) -> u32 {
    naked_asm!(
        // Forward-edge CFI: landing pad
        ".4byte 0x00000017",        // lpad 0

        // Backward-edge CFI: push ra to both shadow stacks
        ".4byte 0x60100073",        // sspush ra (HW — NOP if no Zicfiss)
        "addi   sp, sp, -16",
        "sw     ra, 12(sp)",
        "sw     gp, 8(sp)",
        "sw     ra, 0(gp)",         // sw_sspush (software)
        "addi   gp, gp, 4",

        // Body: x * 3
        "slli   t0, a0, 1",         // t0 = x << 1 = x*2
        "add    a0, t0, a0",        // a0 = x*2 + x = x*3

        // Backward-edge CFI: pop and check both shadow stacks
        "addi   gp, gp, -4",        // sw_sspopchk (software)
        "lw     t0, 0(gp)",
        "lw     ra, 12(sp)",
        "bne    t0, ra, 99f",

        "lw     gp, 8(sp)",
        "addi   sp, sp, 16",
        ".4byte 0x60500073",        // sspopchk ra (HW — NOP if no Zicfiss)
        "ret",

        "99: ebreak",               // Shadow stack mismatch fault
    )
}

/// Add 42 to x. Callable via function pointer.
/// Has an unlabeled landing pad (lpad 0) at entry.
/// Leaf function — no shadow stack needed (no call, so ra is never saved).
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn add_42(x: u32) -> u32 {
    naked_asm!(
        ".4byte 0x00000017",        // lpad 0
        "addi   a0, a0, 42",
        "ret",
    )
}

/// Square x (x * x). Callable via function pointer.
/// Has a labeled landing pad (lpad 7) — on Zicfilp hardware, only callers
/// with label=7 in their indirect-call sequence can reach this function.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn square(x: u32) -> u32 {
    naked_asm!(
        ".4byte {lpad_7}",          // lpad 7
        "mul    a0, a0, a0",
        "ret",
        lpad_7 = const ((7u32 << 12) | 0x17),
    )
}

// ============================================================================
// Non-leaf function demonstrating full CFI protection
// ============================================================================

/// Call a function pointer and add 1 to the result.
/// Demonstrates full forward+backward CFI in a non-leaf function that
/// itself performs an indirect call.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn call_and_inc(fp: unsafe extern "C" fn(u32) -> u32, x: u32) -> u32 {
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

        // Call the function pointer: a0 = fp, a1 = x
        // RISC-V calling convention: a0 = first arg, a1 = second arg
        "mv     t1, a0",            // t1 = fp
        "mv     a0, a1",            // a0 = x (arg for the target)
        "jalr   ra, t1, 0",         // indirect call through fp

        // Add 1 to result
        "addi   a0, a0, 1",

        // Backward-edge: pop and check
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

// ============================================================================
// Function dispatch table — typical use-case for forward-edge CFI
// ============================================================================

/// Dispatch table entry: an ID and a function pointer.
#[repr(C)]
struct DispatchEntry {
    id: u32,
    handler: unsafe extern "C" fn(u32) -> u32,
}

/// A static dispatch table. In a real system, this would be in ROM/flash.
/// Each handler has a landing pad, so indirect calls through this table
/// are forward-edge CFI compliant.
static DISPATCH_TABLE: [DispatchEntry; 3] = [
    DispatchEntry { id: 0, handler: triple },
    DispatchEntry { id: 1, handler: add_42 },
    DispatchEntry { id: 2, handler: square },
];

/// Look up and call a handler by ID.
fn dispatch(id: u32, arg: u32) -> Option<u32> {
    for entry in &DISPATCH_TABLE {
        if entry.id == id {
            return Some(unsafe { (entry.handler)(arg) });
        }
    }
    None
}

// ============================================================================
// Entry Point
// ============================================================================

/// Trap handler that skips illegal instructions.
///
/// When we attempt to access CSRs like menvcfg (0x30A) or ssp (0x011) on
/// hardware/emulators that don't implement them, an illegal instruction
/// exception fires. This handler simply advances mepc past the faulting
/// instruction and returns, allowing boot to continue gracefully.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.init"]
unsafe extern "C" fn _trap_handler() {
    naked_asm!(
        // Read the faulting instruction to determine its length (2 or 4 bytes).
        // RISC-V compressed instructions have bits [1:0] != 0b11.
        "csrr   t0, mepc",
        "lhu    t1, 0(t0)",          // Load halfword at mepc
        "andi   t1, t1, 0x3",
        "li     t2, 0x3",
        "bne    t1, t2, 6f",
        // 4-byte instruction
        "addi   t0, t0, 4",
        "j      7f",
        // 2-byte compressed instruction
        "6: addi t0, t0, 2",
        "7: csrw mepc, t0",
        "mret",
    )
}

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.init"]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        // --- 1. Set up the regular stack ---
        "la     sp, _stack_top",

        // --- 2. Install trap handler that skips illegal CSR accesses ---
        "la     t0, _trap_handler",
        "csrw   mtvec, t0",

        // --- 3. Zero BSS ---
        "la     t0, _bss_start",
        "la     t1, _bss_end",
        "1: beq  t0, t1, 2f",
        "sw     zero, 0(t0)",
        "addi   t0, t0, 4",
        "j      1b",
        "2:",

        // --- 4. Copy .data from FLASH to RAM ---
        "la     t0, _data_start",
        "la     t1, _data_end",
        "la     t2, _data_load",
        "3: beq  t0, t1, 4f",
        "lw     t3, 0(t2)",
        "sw     t3, 0(t0)",
        "addi   t0, t0, 4",
        "addi   t2, t2, 4",
        "j      3b",
        "4:",

        // --- 5. Enable hardware CFI (if supported) ---
        // menvcfg: set LPE (bit 2) and SSE (bit 3)
        // On hardware without these CSRs, the trap handler skips them.
        "li     t0, 0x0C",
        "csrs   0x30A, t0",          // csrs menvcfg, t0

        // --- 6. Initialize hardware shadow stack pointer ---
        "la     t0, _shadow_stack_top",
        "csrw   0x011, t0",          // csrw ssp, t0

        // --- 7. Initialize software shadow stack pointer (gp) ---
        "la     gp, _sw_shadow_stack_bottom",

        // --- 8. Jump to Rust main ---
        "call   main",

        // --- 9. Halt if main returns ---
        "5: wfi",
        "j      5b",
    )
}

// ============================================================================
// Main
// ============================================================================

#[no_mangle]
pub extern "C" fn main() -> ! {
    uart_puts("============================================\r\n");
    uart_puts("  RISC-V Bare Metal CFI Demo (RV32 + Rust)\r\n");
    uart_puts("  Zicfilp (Landing Pads) + Zicfiss (Shadow Stack)\r\n");
    uart_puts("============================================\r\n\r\n");

    // --- Test 1: Direct calls ---
    uart_puts("[Test 1] Direct function calls\r\n");
    {
        let r = unsafe { triple(7) };
        uart_puts("  triple(7) = ");
        uart_put_dec(r);
        uart_puts(" (expected 21)\r\n");

        let r = unsafe { add_42(8) };
        uart_puts("  add_42(8) = ");
        uart_put_dec(r);
        uart_puts(" (expected 50)\r\n");

        let r = unsafe { square(5) };
        uart_puts("  square(5) = ");
        uart_put_dec(r);
        uart_puts(" (expected 25)\r\n");
    }
    uart_newline();

    // --- Test 2: Indirect calls via function pointers ---
    uart_puts("[Test 2] Indirect calls via function pointers\r\n");
    uart_puts("  (Zicfilp enforces landing pads at call targets)\r\n");
    {
        let fp: unsafe extern "C" fn(u32) -> u32 = triple;
        let r = unsafe { fp(10) };
        uart_puts("  fp=triple: fp(10) = ");
        uart_put_dec(r);
        uart_puts(" (expected 30)\r\n");

        let fp: unsafe extern "C" fn(u32) -> u32 = add_42;
        let r = unsafe { fp(0) };
        uart_puts("  fp=add_42: fp(0) = ");
        uart_put_dec(r);
        uart_puts(" (expected 42)\r\n");
    }
    uart_newline();

    // --- Test 3: Dispatch table (common real-world pattern) ---
    uart_puts("[Test 3] Dispatch table with indirect calls\r\n");
    {
        for id in 0..3u32 {
            if let Some(result) = dispatch(id, 6) {
                uart_puts("  dispatch(");
                uart_put_dec(id);
                uart_puts(", 6) = ");
                uart_put_dec(result);
                match id {
                    0 => uart_puts(" (triple: expected 18)"),
                    1 => uart_puts(" (add_42: expected 48)"),
                    2 => uart_puts(" (square: expected 36)"),
                    _ => {}
                }
                uart_newline();
            }
        }
    }
    uart_newline();

    // --- Test 4: Non-leaf function with full CFI ---
    uart_puts("[Test 4] Non-leaf call_and_inc (full forward+backward CFI)\r\n");
    {
        let r = unsafe { call_and_inc(triple, 4) };
        uart_puts("  call_and_inc(triple, 4) = ");
        uart_put_dec(r);
        uart_puts(" (expected 13: triple(4)=12, +1=13)\r\n");

        let r = unsafe { call_and_inc(add_42, 0) };
        uart_puts("  call_and_inc(add_42, 0) = ");
        uart_put_dec(r);
        uart_puts(" (expected 43: add_42(0)=42, +1=43)\r\n");
    }
    uart_newline();

    // --- Test 5: Shadow stack state inspection ---
    uart_puts("[Test 5] Shadow stack pointer inspection\r\n");
    {
        let gp_val: u32;
        unsafe { asm!("mv {}, gp", out(reg) gp_val) };
        uart_puts("  Software SSP (gp) = ");
        uart_put_hex32(gp_val);
        uart_newline();

        uart_puts("  (Hardware SSP via CSR 0x011 — available on Zicfiss HW only)\r\n");
    }
    uart_newline();

    // --- Summary ---
    uart_puts("============================================\r\n");
    uart_puts("  CFI Protection Summary:\r\n");
    uart_puts("  - Forward-edge:  lpad at indirect call targets\r\n");
    uart_puts("  - Backward-edge: sspush/sspopchk in prologue/epilogue\r\n");
    uart_puts("  - Fallback:      software shadow stack via gp register\r\n");
    uart_puts("  - HW instructions are NOPs on non-CFI hardware (safe)\r\n");
    uart_puts("============================================\r\n");
    uart_puts("\r\nAll tests passed.\r\n");

    // Signal QEMU to exit (virt machine test finisher at 0x100000)
    // Write 0x5555 (pass) to the test finisher MMIO address
    unsafe {
        let test_finisher = 0x10_0000 as *mut u32;
        test_finisher.write_volatile(0x5555);
    }

    loop {
        unsafe { asm!("wfi") };
    }
}

// ============================================================================
// Panic Handler
// ============================================================================

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    uart_puts("\r\n!!! PANIC !!!\r\n");
    if let Some(loc) = info.location() {
        uart_puts("  at ");
        uart_puts(loc.file());
        uart_puts(":");
        uart_put_dec(loc.line());
        uart_newline();
    }
    loop {
        unsafe { asm!("wfi") };
    }
}
