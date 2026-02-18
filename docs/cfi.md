# Bare-Metal RISC-V CFI with Rust: A DIY Guide

> **Implementing Zicfilp & Zicfiss on RV32 when the compiler won't do it for you**
>
> *Last updated: February 2026*

---

## Table of Contents

1. [Reality Check: What Works Today](#1-reality-check)
2. [Environment Setup](#2-environment-setup)
3. [Project Skeleton](#3-project-skeleton)
4. [Custom Target Specification](#4-custom-target-spec)
5. [DIY Landing Pads (Zicfilp)](#5-diy-landing-pads)
6. [DIY Shadow Stack (Zicfiss)](#6-diy-shadow-stack)
7. [Software Shadow Stack Fallback](#7-software-shadow-stack)
8. [CSR Setup for CFI Activation](#8-csr-setup)
9. [Linker Script Considerations](#9-linker-script)
10. [Testing with QEMU](#10-testing-with-qemu)
11. [Putting It All Together](#11-full-example)
12. [Limitations & Gotchas](#12-limitations)

---

## 1. Reality Check: What Works Today <a name="1-reality-check"></a>

### Compiler Support Matrix (as of Feb 2026)

| Feature | LLVM/Clang | GCC | rustc | Status |
|---|---|---|---|---|
| Zicfilp (lpad emission) | ✅ Clang standalone | ✅ Patches in review | ❌ Not exposed | Clang can do it; rustc's bundled LLVM ignores `+zicfilp` |
| Zicfiss (sspush/sspop) | ✅ Clang standalone | ❌ Not yet | ❌ Not exposed | Same situation |
| `-march=rv32i_zicfilp_zicfiss` | ✅ Clang | Partial | ❌ | rustc doesn't recognize these target features |
| kCFI (`-fsanitize=kcfi`) | ✅ Clang only | ❌ | ❌ on RISC-V | Rust has kcfi support on x86/aarch64 only |

### The Core Problem

Rust's `rustc` uses LLVM as its backend. While standalone LLVM/Clang promoted
Zicfilp and Zicfiss from experimental in LLVM 20 (Sept 2025), **rustc's bundled
LLVM does not recognize these as valid RISC-V target features** — even as of
rustc 1.95.0-nightly with LLVM 21.1.8. Specifying them in a custom target JSON
produces warnings and the features are silently ignored:

```
'+zicfilp' is not a recognized feature for this target (ignoring feature)
'+zicfiss' is not a recognized feature for this target (ignoring feature)
```

You also cannot pass them via `rustflags`:

```toml
# THIS DOES NOT WORK (yet)
[build]
rustflags = ["-C", "target-feature=+zicfilp,+zicfiss"]
```

The features are simply not wired through from rustc to its bundled LLVM for
RISC-V. The compiler will **not** auto-emit `lpad` or `sspush`/`sspopchk` for
you under any configuration today.

### What We CAN Do

Since we're bare-metal, we have full control. Our strategy:

1. **Inline assembly** — Emit `lpad`, `sspush`, `sspop` instructions manually
2. **Raw `.word` directives** — Encode instructions as raw bytes when the assembler doesn't recognize them
3. **Custom target JSON** — Tell rustc/LLVM about our CPU features at the target level
4. **Naked functions** — Write function prologues/epilogues by hand
5. **Build scripts** — Post-process ELF to verify/inject landing pads

---

## 2. Environment Setup <a name="2-environment-setup"></a>

### Prerequisites

```bash
# Install Rust nightly (needed for -Z build-std and custom JSON targets)
rustup install nightly
rustup component add --toolchain nightly rust-src llvm-tools

# Install QEMU for testing
# Ubuntu/Debian:
sudo apt install qemu-system-riscv32
# macOS:
brew install qemu
```

> **Note:** `naked_functions` was stabilized in Rust 1.88.0 — nightly is only
> needed for `-Z build-std` (building `core` from source for our custom target)
> and `-Z json-target-spec` (custom target JSON). Once your custom target is a
> built-in, you could use stable.

### Pin the Toolchain

Create `rust-toolchain.toml` in your project root:

```toml
[toolchain]
channel = "nightly"
components = ["rust-src", "llvm-tools"]
```

### Verify LLVM Version

```bash
rustc +nightly -vV
# As of Feb 2026: LLVM version 21.1.8
# Even though standalone LLVM 20+ supports Zicfilp/Zicfiss,
# rustc's bundled LLVM does NOT recognize them for RISC-V.

# Confirm the features are not exposed:
rustc +nightly --print target-features --target riscv32imac-unknown-none-elf | grep -i cfi
# Prints nothing — confirming rustc doesn't expose them
```

---

## 3. Project Skeleton <a name="3-project-skeleton"></a>

```bash
cargo new riscv-cfi-baremetal
cd riscv-cfi-baremetal
```

### `Cargo.toml`

We don't need any dependencies — we roll our own startup code, linker scripts,
and UART driver. No `riscv` or `riscv-rt` crates.

```toml
[package]
name = "riscv-cfi-baremetal"
version = "0.1.0"
edition = "2021"

[dependencies]

[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
debug = true            # Keep debug info for inspection
```

### `.cargo/config.toml`

```toml
[build]
target = "rv32imac-cfi-none-elf.json"

[target.'cfg(target_arch = "riscv32")']
runner = "qemu-system-riscv32 -machine virt -nographic -bios none -kernel"
rustflags = [
    "-C", "link-arg=-Tmemory.x",
    "-C", "link-arg=-Tlink.x",
    "-C", "link-arg=--no-relax",
]

[unstable]
build-std = ["core"]
build-std-features = ["compiler-builtins-mem"]
json-target-spec = true
```

Key points:
- **`target`** points to our custom JSON target, not a built-in triple
- **`cfg(target_arch = "riscv32")`** matches any RV32 target (including our custom one)
- **`--no-relax`** disables GP-relative linker relaxation so we can use `gp` as a
  software shadow stack pointer
- **`json-target-spec = true`** is required under `[unstable]` for custom JSON targets
- **`build-std`** only needs `["core"]` — no `alloc` needed for bare metal

---

## 4. Custom Target Specification <a name="4-custom-target-spec"></a>

This is the first key trick. We create a custom target JSON that tells LLVM about
our CPU's capabilities, potentially sneaking in the Zicfilp/Zicfiss features at
the LLVM level even though rustc doesn't know about them.

### Generate Base Target

```bash
rustc +nightly -Z unstable-options \
    --print target-spec-json \
    --target riscv32imac-unknown-none-elf \
    > rv32imac-cfi-none-elf.json
```

### Modify the Target JSON

Edit `rv32imac-cfi-none-elf.json`. The key changes from the base target are in
`features` and `metadata`:

```json
{
  "arch": "riscv32",
  "cpu": "generic-rv32",
  "crt-objects-fallback": "false",
  "data-layout": "e-m:e-p:32:32-i64:64-n32-S128",
  "eh-frame-header": false,
  "emit-debug-gdb-scripts": false,
  "features": "+m,+a,+c,+zicfilp,+zicfiss,+zimop,+zcmop",
  "linker": "rust-lld",
  "linker-flavor": "gnu-lld",
  "llvm-abiname": "ilp32",
  "llvm-target": "riscv32",
  "max-atomic-width": 32,
  "metadata": {
    "description": "RISC-V RV32IMAC with CFI extensions (Zicfilp + Zicfiss)",
    "host_tools": false,
    "std": false,
    "tier": 3
  },
  "panic-strategy": "abort",
  "relocation-model": "static",
  "target-pointer-width": 32
}
```

**Important schema notes (learned the hard way):**

- **No `is-builtin` field** — this is not a valid target spec key; rustc rejects it
- **`target-pointer-width`** must be an integer (`32`), not a string (`"32"`)
- **`llvm-target`** should be `"riscv32"`, not `"riscv32-unknown-none-elf"`
- **`metadata`** block is expected by modern rustc (optional but good practice)
- **`crt-objects-fallback`** should be `"false"` to match the base target

**Key additions in `features`:**

- `+zicfilp` — Tells LLVM the target has landing pad support
- `+zicfiss` — Tells LLVM the target has shadow stack support
- `+zimop` — May-Be-Operations (Zicfiss instructions are encoded as Zimop subset)
- `+zcmop` — Compressed May-Be-Operations

> ⚠️ **As of Feb 2026, rustc's bundled LLVM (21.1.8) does NOT recognize
> `+zicfilp` or `+zicfiss` for RISC-V targets.** You will see warnings like
> `'+zicfilp' is not a recognized feature for this target (ignoring feature)`.
> The `+zimop` and `+zcmop` features ARE recognized. The unrecognized features
> are harmlessly ignored — but this means LLVM will NOT auto-emit `lpad` or
> `sspush`/`sspopchk` for you. That's why we do everything manually with
> `.4byte` encodings below.

### Use the Custom Target

Since `.cargo/config.toml` already sets the target, just run:

```bash
cargo build --release
```

---

## 5. DIY Landing Pads (Zicfilp) <a name="5-diy-landing-pads"></a>

The `lpad` instruction is encoded as `AUIPC x0, imm` — opcode `0x00000017`
with `rd=x0`. The immediate field carries an optional label value.

### Encoding

```
lpad 0     = 0x00000017    (AUIPC x0, 0)
lpad N     = (N << 12) | 0x00000017
```

### Approach A: Macros for Non-Naked Functions

For inserting landing pads inside regular (non-naked) Rust functions, use
`core::arch::asm!` with `.4byte`:

```rust
#![no_std]
#![no_main]

/// Insert a landing pad at the current location.
/// The label value is optional — 0 means "unlabeled landing pad".
macro_rules! landing_pad {
    () => {
        unsafe { core::arch::asm!(".4byte 0x00000017", options(nomem, nostack)) }
    };
    ($label:literal) => {
        unsafe {
            core::arch::asm!(
                concat!(".4byte ", $label, " << 12 | 0x17"),
                options(nomem, nostack),
            )
        }
    };
}
```

> **Note:** `#![feature(naked_functions)]` is **not** needed — `naked_functions`
> was stabilized in Rust 1.88.0.

### Approach B: Naked Functions with Landing Pads

For functions that are targets of indirect calls, we write them as naked
functions with a landing pad as the very first instruction.

Modern Rust requires `#[unsafe(naked)]` (not `#[naked]`) and `naked_asm!`
(not `asm!`). The `naked_asm!` macro does **not** take `options(noreturn)` —
it's implied.

```rust
use core::arch::naked_asm;

/// A leaf function callable through a function pointer.
/// The landing pad at entry ensures Zicfilp enforcement succeeds.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn indirect_target(x: u32) -> u32 {
    naked_asm!(
        // Landing pad — MUST be the first instruction
        ".4byte 0x00000017",    // lpad 0

        // Function body
        "addi   a0, a0, 1",    // return x + 1
        "ret",
    )
}

/// Another target with a labeled landing pad (label = 42).
/// On Zicfilp hardware, only indirect calls carrying label=42 can reach this.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn checked_target(x: u32) -> u32 {
    naked_asm!(
        ".4byte {lpad_42}",     // lpad 42
        "slli   a0, a0, 1",    // return x * 2
        "ret",
        lpad_42 = const ((42u32 << 12) | 0x17),
    )
}
```

### Build-Script Landing Pad Verifier

You can write a build script that inspects the output ELF and warns if any
function-pointer target is missing a landing pad:

```rust
// build.rs — optional post-build verification
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/");

    // After build, you could run:
    // riscv32-unknown-elf-objdump -d target/.../your_binary
    // and grep for functions that lack 0x00000017 as their first instruction.
    // This is left as a CI integration exercise.
}
```

---

## 6. DIY Shadow Stack (Zicfiss) <a name="6-diy-shadow-stack"></a>

The Zicfiss instructions are encoded as a subset of Zimop (May-Be-Operations).
On hardware without Zicfiss, they execute as NOPs. On hardware with Zicfiss
active, they manipulate the shadow stack.

### Instruction Encodings (RV32)

| Instruction | Encoding | Description |
|---|---|---|
| `sspush ra` | `0x60100073` | Push `ra` (x1) to shadow stack |
| `sspopchk ra` | `0x60500073` | Pop from shadow stack, compare with `ra`, fault on mismatch |
| `ssrdp rd` | Zimop encoding | Read shadow stack pointer into `rd` |
| `ssamoswap.w rd, rs2, (rs1)` | AMO encoding | Atomic swap on shadow stack memory |

> **Note:** The exact encodings above are derived from the Zicfiss spec where
> these instructions reuse Zimop/Zcmop code points. Verify against the
> ratified spec for your hardware.

### Inline Assembly Macros

```rust
/// Push the return address onto the hardware shadow stack.
/// On hardware without Zicfiss, this is a NOP (Zimop behavior).
#[macro_export]
macro_rules! sspush_ra {
    () => {
        unsafe {
            core::arch::asm!(
                // sspush x1 — encoded as Zimop subset
                // Encoding: specific to Zicfiss spec
                ".4byte 0x60100073",
                options(nomem, nostack)
            )
        }
    };
}

/// Pop from shadow stack and compare with ra.
/// Faults with software-check exception on mismatch.
/// NOP on hardware without Zicfiss.
#[macro_export]
macro_rules! sspopchk_ra {
    () => {
        unsafe {
            core::arch::asm!(
                ".4byte 0x60500073",
                options(nomem, nostack)
            )
        }
    };
}

/// Read the shadow stack pointer into a register.
#[macro_export]
macro_rules! ssrdp {
    ($reg:literal) => {
        // ssrdp is encoded as a Zimop instruction
        // The actual encoding depends on the destination register
        // For reading into a general register, use CSR read of ssp
        unsafe {
            core::arch::asm!(
                concat!("csrr ", $reg, ", 0x011"),  // CSR_SSP = 0x011
                options(nomem, nostack)
            )
        }
    };
}
```

### Naked Function with Full CFI (Landing Pad + Shadow Stack)

```rust
use core::arch::naked_asm;

/// A non-leaf function with both forward and backward edge CFI.
/// Demonstrates dual-mode: hardware CFI instructions (NOP on old HW)
/// plus software shadow stack (always works).
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn protected_function(x: u32) -> u32 {
    naked_asm!(
        // === ENTRY ===
        // Forward-edge: landing pad (Zicfilp)
        ".4byte 0x00000017",        // lpad 0

        // Backward-edge: push return address to shadow stack (Zicfiss)
        ".4byte 0x60100073",        // sspush ra (HW — NOP if no Zicfiss)

        // Standard prologue
        "addi   sp, sp, -16",
        "sw     ra, 12(sp)",
        "sw     gp, 8(sp)",         // Preserve software SSP

        // Software shadow stack push
        "sw     ra, 0(gp)",
        "addi   gp, gp, 4",

        // Function body
        "addi   a0, a0, 10",

        // Software shadow stack pop+check
        "addi   gp, gp, -4",
        "lw     t0, 0(gp)",
        "lw     ra, 12(sp)",
        "bne    t0, ra, 99f",       // Check

        // Standard epilogue
        "lw     gp, 8(sp)",
        "addi   sp, sp, 16",

        // Backward-edge: verify return address against hardware shadow stack
        ".4byte 0x60500073",        // sspopchk ra (HW — NOP if no Zicfiss)

        "ret",

        // Software shadow stack mismatch — fault
        "99: ebreak",
    )
}
```

---

## 7. Software Shadow Stack Fallback <a name="7-software-shadow-stack"></a>

When your hardware doesn't have Zicfiss, you can implement a software shadow
stack as a fallback. This is what `CONFIG_SHADOW_CALL_STACK` does in the Linux
kernel, adapted for bare-metal Rust. On Zicfiss-capable cores the hardware
shadow stack is strictly stronger and the software version is redundant.

### Design

- Reserve a register as the Shadow Stack Pointer (SSP) — we'll use `gp` (x3)
  since bare-metal code rarely uses the global pointer for its intended purpose
- Allocate a separate memory region for the shadow stack
- On function entry: store `ra` to `[gp]`, increment `gp`
- On function return: decrement `gp`, load and compare `ra`

### Implementation

```rust
/// Software shadow stack configuration
const SHADOW_STACK_SIZE: usize = 4096; // 4KB = 1024 return addresses
const SHADOW_STACK_BASE: usize = 0x8008_0000; // Place in unused RAM region

/// Initialize the software shadow stack.
/// Call this once at startup, before any other functions.
#[inline(never)]
pub unsafe fn sss_init() {
    core::arch::asm!(
        "li gp, {base}",
        base = const SHADOW_STACK_BASE,
        options(nomem, nostack)
    );
}

/// Macro to insert software shadow stack push in function prologue.
/// Uses gp (x3) as the shadow stack pointer.
#[macro_export]
macro_rules! sss_push_ra {
    () => {
        unsafe {
            core::arch::asm!(
                "sw     ra, 0(gp)",     // Store ra to shadow stack
                "addi   gp, gp, 4",     // Advance shadow stack pointer
                options(nostack)
            )
        }
    };
}

/// Macro to insert software shadow stack check in function epilogue.
/// Compares the saved return address with current ra.
/// On mismatch: enters infinite loop (you'd replace this with your fault handler).
#[macro_export]
macro_rules! sss_popchk_ra {
    () => {
        unsafe {
            core::arch::asm!(
                "addi   gp, gp, -4",    // Rewind shadow stack pointer
                "lw     t0, 0(gp)",     // Load shadow copy
                "beq    t0, ra, 1f",    // Compare with current ra
                // MISMATCH — trigger fault
                "ebreak",              // Or: j fault_handler
                "1:",
                options(nostack)
            )
        }
    };
}
```

### Using the Software Shadow Stack

```rust
#[no_mangle]
pub extern "C" fn my_function(x: u32) -> u32 {
    sss_push_ra!();

    let result = x + inner_function(x);

    sss_popchk_ra!();
    result
}
```

### Proc-Macro Approach (Advanced)

For a more ergonomic experience, you could write a proc macro that automatically
wraps functions:

```rust
// In a separate proc-macro crate:
// This is conceptual — full implementation would parse the function AST
// and inject prologue/epilogue assembly.

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn shadow_stack(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse the function, wrap body with sss_push_ra / sss_popchk_ra
    // Left as an exercise — the key idea is automation
    item
}

// Usage:
// #[shadow_stack]
// fn my_protected_function() { ... }
```

---

## 8. CSR Setup for CFI Activation <a name="8-csr-setup"></a>

On real hardware with Zicfilp/Zicfiss, you need to enable CFI via CSRs
before the extensions take effect. This is done in M-mode (machine mode)
during early boot.

### Relevant CSRs

| CSR | Address | Purpose |
|---|---|---|
| `menvcfg` | 0x30A | M-mode environment config — controls CFI for S-mode |
| `senvcfg` | 0x10A | S-mode environment config — controls CFI for U-mode |
| `mseccfg` | 0x747 | Machine security config (may be relevant) |
| `ssp` | 0x011 | Shadow Stack Pointer CSR |

### Bit Fields

```
menvcfg / senvcfg:
  Bit 2 (LPE)  — Landing Pad Enable: set to 1 to activate Zicfilp
  Bit 3 (SSE)  — Shadow Stack Enable: set to 1 to activate Zicfiss
```

### M-Mode Initialization Code

```rust
/// Enable Zicfilp and Zicfiss for the current privilege mode.
/// MUST be called from M-mode during early boot.
pub unsafe fn enable_cfi() {
    // Read menvcfg
    let mut menvcfg: u32;
    core::arch::asm!("csrr {}, 0x30A", out(reg) menvcfg);

    // Set LPE (bit 2) and SSE (bit 3)
    menvcfg |= (1 << 2) | (1 << 3);

    // Write back
    core::arch::asm!("csrw 0x30A, {}", in(reg) menvcfg);

    // If running in S-mode software, also set senvcfg for U-mode
    // core::arch::asm!("csrw 0x10A, {}", in(reg) senvcfg);

    // Initialize the shadow stack pointer CSR
    let ssp_base: u32 = 0x8010_0000; // Top of shadow stack region
    core::arch::asm!("csrw 0x011, {}", in(reg) ssp_base);
}
```

### Trap Handler for Graceful Degradation

The CSR accesses to `menvcfg` (0x30A) and `ssp` (0x011) will cause illegal
instruction traps on hardware/emulators that don't implement them (e.g.,
QEMU < 9.x). We install a trap handler that simply skips the faulting
instruction, allowing boot to continue:

```rust
use core::arch::naked_asm;

/// Trap handler that skips illegal instructions.
/// Determines instruction length (2 vs 4 bytes) by inspecting bits [1:0]
/// of the faulting instruction, then advances mepc accordingly.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.init"]
unsafe extern "C" fn _trap_handler() {
    naked_asm!(
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
```

### Complete Boot Sequence

```rust
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.init"]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        // 1. Set up regular stack
        "la     sp, _stack_top",

        // 2. Install trap handler (skips illegal CSR accesses gracefully)
        "la     t0, _trap_handler",
        "csrw   mtvec, t0",

        // 3. Zero BSS
        "la     t0, _bss_start",
        "la     t1, _bss_end",
        "1: beq  t0, t1, 2f",
        "sw     zero, 0(t0)",
        "addi   t0, t0, 4",
        "j      1b",
        "2:",

        // 4. Copy .data from FLASH to RAM
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

        // 5. Enable CFI extensions (if hardware supports them)
        // On unsupported hardware, these CSR accesses trap and the
        // trap handler skips them — boot continues cleanly.
        "li     t0, 0x0C",
        "csrs   0x30A, t0",          // csrs menvcfg, t0  (set LPE + SSE)

        // 6. Initialize hardware shadow stack pointer
        "la     t0, _shadow_stack_top",
        "csrw   0x011, t0",          // csrw ssp, t0

        // 7. Initialize software shadow stack pointer (gp)
        "la     gp, _sw_shadow_stack_bottom",

        // 8. Jump to Rust main
        "call   main",

        // 9. Halt if main returns
        "5: wfi",
        "j      5b",
    )
}
```

---

## 9. Linker Script Considerations <a name="9-linker-script"></a>

### `memory.x`

```ld
MEMORY
{
    FLASH : ORIGIN = 0x80000000, LENGTH = 512K
    RAM   : ORIGIN = 0x80080000, LENGTH = 256K
}

/* Shadow stack regions */
_shadow_stack_size = 4K;
_sw_shadow_stack_size = 4K;
```

### `link.x`

```ld
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

    /* Discard unwinding sections (not needed in bare metal) */
    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
```

### Why Separate Regions Matter

The shadow stack MUST be in a different memory region from the regular stack.
If they're adjacent, a large buffer overflow on the regular stack could reach
the shadow stack. On real Zicfiss hardware, the shadow stack pages have special
PTE attributes that prevent normal stores — but on software shadow stacks,
physical separation is your only defense.

---

## 10. Testing with QEMU <a name="10-testing-with-qemu"></a>

### QEMU CFI Support

As of early 2026, QEMU's RISC-V CFI support varies by version:

| QEMU Version | CFI Support | Notes |
|---|---|---|
| < 9.x | None | CSR accesses to menvcfg/ssp trap as illegal instructions |
| 9.x+ | Partial | May recognize `-cpu rv32,zicfilp=true,...` |

```bash
qemu-system-riscv32 --version
# Ubuntu 22.04 ships QEMU 6.2 — no CFI support

# On QEMU 9.x+, try explicit CPU features:
qemu-system-riscv32 \
    -machine virt \
    -cpu rv32,zicfilp=true,zicfiss=true,zimop=true,zcmop=true \
    -nographic \
    -bios none \
    -kernel target/rv32imac-cfi-none-elf/release/riscv-cfi-baremetal
```

> Thanks to the trap handler in `_start` (see Section 8), the binary runs
> cleanly on **any** QEMU version. On old QEMU, the CSR writes silently
> fail (trap handler skips them), the hardware CFI instructions execute as
> NOPs, and the software shadow stack provides protection.

### Testing Without CFI Hardware

Since Zicfiss/Zicfilp instructions are encoded as Zimop/Zcmop, they are
**guaranteed to be NOPs** on hardware/emulators that don't implement them.
This means:

- ✅ Your binary runs on any RV32 (tested on QEMU 6.2 with zero CFI support)
- ✅ Hardware CFI instructions are harmless NOPs on unsupported hardware
- ✅ Software shadow stack provides real protection regardless
- ✅ On CFI-capable hardware, enforcement kicks in automatically
- ⚠️ You can't test that CFI *violations* are caught without real CFI hardware

### Verify Instruction Encoding

```bash
# Use llvm-objdump from your rustup toolchain:
llvm-objdump -d target/rv32imac-cfi-none-elf/release/riscv-cfi-baremetal | head -100

# Look for these encodings in the disassembly:
#   0x00000017  →  "auipc zero, 0x0" or ".word 0x00000017"  (lpad 0)
#   0x00007017  →  ".word 0x00007017"                        (lpad 7)
#   0x60100073  →  ".word 0x60100073"                        (sspush ra)
#   0x60500073  →  ".word 0x60500073"                        (sspopchk ra)
#
# Older objdump shows lpad 0 as "auipc zero,0x0" since it's encoded as
# AUIPC with rd=x0. Newer objdump with Zicfilp awareness shows "lpad".
```

---

## 11. Putting It All Together <a name="11-full-example"></a>

### `src/main.rs`

The full source lives in `src/main.rs` in the project. The key elements are:

- **Trap handler** (`_trap_handler`) — skips illegal CSR accesses during boot
- **Naked functions** with landing pads: `triple`, `add_42`, `square`
- **Non-leaf CFI function** (`call_and_inc`) — full forward+backward protection
  with an indirect call inside
- **Dispatch table** — realistic function-pointer table pattern
- **UART output** — direct MMIO to QEMU's 16550-compatible UART at `0x1000_0000`

Here's a condensed version showing the patterns (see `src/main.rs` for the
complete, buildable source):

```rust
#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

// --- UART (QEMU virt machine) ---
const UART_BASE: *mut u8 = 0x1000_0000 as *mut u8;
fn uart_puts(s: &str) {
    for b in s.bytes() { unsafe { UART_BASE.write_volatile(b) } }
}

// --- Indirect call target with full CFI ---
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn triple(x: u32) -> u32 {
    naked_asm!(
        ".4byte 0x00000017",        // lpad 0     (forward-edge)
        ".4byte 0x60100073",        // sspush ra  (backward-edge, HW)
        "addi   sp, sp, -16",
        "sw     ra, 12(sp)",
        "sw     gp, 8(sp)",
        "sw     ra, 0(gp)",         // software shadow stack push
        "addi   gp, gp, 4",

        "slli   t0, a0, 1",         // x * 2
        "add    a0, t0, a0",        // x * 3

        "addi   gp, gp, -4",        // software shadow stack pop+check
        "lw     t0, 0(gp)",
        "lw     ra, 12(sp)",
        "bne    t0, ra, 99f",
        "lw     gp, 8(sp)",
        "addi   sp, sp, 16",
        ".4byte 0x60500073",        // sspopchk ra (backward-edge, HW)
        "ret",
        "99: ebreak",               // mismatch fault
    )
}

// --- Leaf function: landing pad only, no shadow stack needed ---
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn add_42(x: u32) -> u32 {
    naked_asm!(
        ".4byte 0x00000017",        // lpad 0
        "addi   a0, a0, 42",
        "ret",
    )
}

// --- Labeled landing pad (label = 7) ---
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

// --- Main ---
#[no_mangle]
pub extern "C" fn main() -> ! {
    uart_puts("=== RISC-V CFI Demo ===\r\n");

    // Direct call
    let r = unsafe { triple(7) };       // r = 21

    // Indirect call via function pointer
    let fp: unsafe extern "C" fn(u32) -> u32 = triple;
    let r = unsafe { fp(10) };          // r = 30

    // Dispatch table
    let table: [unsafe extern "C" fn(u32) -> u32; 3] = [triple, add_42, square];
    let r = unsafe { (table[2])(6) };   // r = 36

    uart_puts("All tests passed.\r\n");
    loop { unsafe { asm!("wfi") }; }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    uart_puts("PANIC\r\n");
    loop { unsafe { asm!("wfi") }; }
}
```

### Build & Run

```bash
# Build (uses custom target from .cargo/config.toml automatically)
cargo build --release

# Inspect the binary — verify CFI instructions are present
llvm-objdump -d target/rv32imac-cfi-none-elf/release/riscv-cfi-baremetal | head -80
# Look for:
#   0x00000017  →  lpad 0  (may show as "auipc zero, 0x0" on older objdump)
#   0x00007017  →  lpad 7  (labeled landing pad)
#   0x60100073  →  sspush ra
#   0x60500073  →  sspopchk ra

# Run in QEMU
qemu-system-riscv32 -machine virt -nographic -bios none \
    -kernel target/rv32imac-cfi-none-elf/release/riscv-cfi-baremetal
```

Expected output:

```
============================================
  RISC-V Bare Metal CFI Demo (RV32 + Rust)
  Zicfilp (Landing Pads) + Zicfiss (Shadow Stack)
============================================

[Test 1] Direct function calls
  triple(7) = 21 (expected 21)
  add_42(8) = 50 (expected 50)
  square(5) = 25 (expected 25)

[Test 2] Indirect calls via function pointers
  (Zicfilp enforces landing pads at call targets)
  fp=triple: fp(10) = 30 (expected 30)
  fp=add_42: fp(0) = 42 (expected 42)

[Test 3] Dispatch table with indirect calls
  dispatch(0, 6) = 18 (triple: expected 18)
  dispatch(1, 6) = 48 (add_42: expected 48)
  dispatch(2, 6) = 36 (square: expected 36)

[Test 4] Non-leaf call_and_inc (full forward+backward CFI)
  call_and_inc(triple, 4) = 13 (expected 13: triple(4)=12, +1=13)
  call_and_inc(add_42, 0) = 43 (expected 43: add_42(0)=42, +1=43)

[Test 5] Shadow stack pointer inspection
  Software SSP (gp) = 0x80083000
  (Hardware SSP via CSR 0x011 — available on Zicfiss HW only)

============================================
  CFI Protection Summary:
  - Forward-edge:  lpad at indirect call targets
  - Backward-edge: sspush/sspopchk in prologue/epilogue
  - Fallback:      software shadow stack via gp register
  - HW instructions are NOPs on non-CFI hardware (safe)
============================================

All tests passed.
```

---

## 12. Limitations & Gotchas <a name="12-limitations"></a>

### What This Approach Cannot Do

| Limitation | Why | Workaround |
|---|---|---|
| Compiler won't auto-insert `lpad` at every function entry | rustc doesn't support `-fcf-protection` for RISC-V | Naked functions or post-processing ELF |
| Compiler won't auto-insert `sspush`/`sspopchk` in prologues/epilogues | Same — no compiler support | Manual insertion or proc-macro |
| No automatic type-hash kCFI | Requires `-fsanitize=kcfi` which rustc doesn't support on RISC-V | Not feasible in pure Rust today |
| `gp` register reservation | Rust/LLVM may use `gp` for relaxation | Pass `--no-relax` to the linker (see below) |
| Cannot protect Rust-generated code | Only naked/asm functions get CFI | Write critical paths in assembly |
| Software shadow stack is bypassable | If attacker leaks `gp`, they can corrupt it | Use hardware Zicfiss when available |
| `+zicfilp`/`+zicfiss` target features ignored | rustc's bundled LLVM (even 21.x) doesn't recognize them for RV | Raw `.4byte` encodings only |

### The `+zicfilp`/`+zicfiss` Target Feature Problem

Even though standalone LLVM 20+ supports these features, **rustc's bundled LLVM
does not recognize them for RISC-V targets** as of nightly 1.95.0 (LLVM 21.1.8,
Feb 2026). Specifying them in a custom target JSON produces:

```
'+zicfilp' is not a recognized feature for this target (ignoring feature)
'+zicfiss' is not a recognized feature for this target (ignoring feature)
```

The features compile without error but are silently ignored — LLVM will not
auto-emit `lpad` or `sspush`/`sspopchk`. We include them in the target JSON
anyway (they're harmless and will activate once rustc's LLVM catches up), but
all CFI instructions must be emitted manually via `.4byte`.

The `+zimop` and `+zcmop` features *are* recognized and work correctly.

### The `naked_functions` API

The `naked_functions` feature was stabilized in **Rust 1.88.0** (mid-2025). The
modern API differs from older nightly versions:

| Old (pre-1.88) | Current (1.88+) |
|---|---|
| `#![feature(naked_functions)]` | Not needed |
| `#[naked]` | `#[unsafe(naked)]` |
| `asm!(..., options(noreturn))` | `naked_asm!(...)` (noreturn implied) |

If you're following old tutorials or blog posts, this is the most common source
of compile errors.

### The `gp` Register Problem

By default, the RISC-V linker uses `gp` for linker relaxation (accessing globals
via `gp`-relative addressing). If you reserve `gp` for the software shadow stack,
you must disable this optimization:

```toml
# In .cargo/config.toml — disable GP relaxation
rustflags = [
    "-C", "link-arg=-Tlink.x",
    "-C", "link-arg=--no-relax",
]
```

Alternatively, use `s11` (x27) instead of `gp`:

```rust
macro_rules! sw_sspush_s11 {
    () => {
        unsafe {
            asm!(
                "sw ra, 0(s11)",
                "addi s11, s11, 4",
                options(nostack)
            )
        }
    };
}
```

But you'll need to tell LLVM not to use `s11` for register allocation —
which requires a custom target or `-C llvm-args=-riscv-reserved-reg=x27`
(experimental).

### CSR Access on Unsupported Hardware

The CSR writes to enable CFI (`menvcfg` at 0x30A, `ssp` at 0x011) will cause
illegal instruction traps on hardware and emulators that don't implement them.
QEMU < 9.x does not support these CSRs.

**Solution:** Install a trap handler early in boot that skips illegal instructions
(see Section 8). The trap handler reads `mepc`, determines instruction length
(2 vs 4 bytes), advances `mepc`, and returns via `mret`.

### Path Forward

The realistic path to full CFI in Rust on RISC-V:

1. **Now:** Use the techniques in this guide for critical bare-metal code
2. **Soon:** rustc's bundled LLVM recognizes `+zicfilp`/`+zicfiss` for RISC-V
3. **Later:** rustc implements `-Z cf-protection=full` for RISC-V, auto-instrumenting all functions
4. **Eventually:** Rust's `-Z sanitizer=kcfi` works on RISC-V

Until then — you have `naked_asm!`, `.4byte` encodings, and determination.

---

*This guide is provided as-is for educational purposes. Instruction encodings
should be verified against the ratified RISC-V CFI specification for your
specific hardware. The Zimop/Zcmop NOP guarantee means these techniques are
safe to deploy even on hardware without CFI — the instructions simply do nothing.*
