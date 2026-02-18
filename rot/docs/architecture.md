# RISC-V Root of Trust: CFI + PMP Architecture

> **Hardware CFI enforcement with PMP privilege separation in a bare-metal Rust kernel**

---

## Architecture Overview

This Root of Trust (RoT) design combines three hardware security features of
RISC-V into a unified trust anchor:

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Hardware Enforcement                         │
│  ┌──────────────┐  ┌──────────────────┐  ┌──────────────────────┐  │
│  │   Zicfilp    │  │     Zicfiss      │  │        PMP           │  │
│  │ Landing Pads │  │  Shadow Stack    │  │ Memory Protection    │  │
│  │ (fwd-edge)   │  │  (bwd-edge)     │  │ (privilege isolation)│  │
│  └──────────────┘  └──────────────────┘  └──────────────────────┘  │
└─────────────────────────────────────────────────────────────────────┘
                              │
          ┌───────────────────┼───────────────────┐
          ▼                   ▼                   ▼
┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐
│    M-Mode (RoT)  │ │  Privilege Drop  │ │   U-Mode (App)   │
│  - Boot + Init   │ │  - PMP config    │ │  - Firmware code  │
│  - Crypto keys   │ │  - CFI enable    │ │  - Sandboxed exec │
│  - Measurement   │ │  - mret to U     │ │  - ecall services │
│  - Ecall handler │ │                  │ │  - CFI enforced   │
└──────────────────┘ └──────────────────┘ └──────────────────┘
```

---

## Memory Map

All regions are power-of-2 sized and naturally aligned for efficient PMP NAPOT
encoding (each region = one PMP entry).

| Region | Base | Size | M-mode | U-mode | Purpose |
|---|---|---|---|---|---|
| ROM | `0x8000_0000` | 64K | RX (Locked) | none | M-mode code (immutable) |
| M_RAM | `0x8001_0000` | 32K | RW | none | M-mode data, stack, secrets |
| M_SHADOW | `0x8001_8000` | 4K | RW | none | M-mode HW shadow stack |
| M_SW_SHADOW | `0x8001_9000` | 4K | RW | none | M-mode SW shadow stack |
| U_CODE | `0x8002_0000` | 128K | RWX | **RX** | U-mode firmware code |
| U_RODATA | `0x8004_0000` | 32K | RW | **R** | U-mode read-only data |
| U_RAM | `0x8004_8000` | 64K | RW | **RW** | U-mode data + stack |
| U_SHADOW | `0x8005_8000` | 4K | RW | **RW** | U-mode HW shadow stack |
| U_SW_SHADOW | `0x8005_9000` | 4K | RW | **RW** | U-mode SW shadow stack |
| UART | `0x1000_0000` | 4K | RW | **RW** | 16550 UART MMIO |

**Key security invariants:**
- **W^X enforcement**: U-mode code is RX (no write), U-mode data is RW (no execute)
- **M-mode isolation**: All M-mode memory is invisible to U-mode
- **Shadow stack isolation**: Shadow stacks are in dedicated regions, separate from data stacks
- **Locked code**: M-mode ROM is locked (even M-mode cannot self-modify at runtime)

---

## PMP Configuration

8 PMP entries enforce the memory map. PMP entries use NAPOT (Naturally Aligned
Power-Of-Two) addressing for single-entry-per-region efficiency.

```
Entry  Region       Locked  M-mode   U-mode    NAPOT addr
─────  ───────────  ──────  ───────  ────────  ──────────
  0    ROM (64K)    YES     R-X      none      napot(0x80000000, 64K)
  1    M_RAM (32K)  no      RW-      none      napot(0x80010000, 32K)
  2    M_SHADOW(8K) no      RW-      none      napot(0x80018000, 8K)
  3    U_CODE(128K) no      RWX      R-X       napot(0x80020000, 128K)
  4    U_RODATA(32K)no      RW-      R--       napot(0x80040000, 32K)
  5    U_RAM (64K)  no      RW-      RW-       napot(0x80048000, 64K)
  6    U_SHADOW(8K) no      RW-      RW-       napot(0x80058000, 8K)
  7    UART (4K)    no      RW-      RW-       napot(0x10000000, 4K)
```

**PMP semantics:**
- **Locked entries** (L=1): Apply to M-mode too. M-mode ROM is RX-only even for M-mode.
- **Unlocked entries** with no permissions: M-mode bypasses PMP (has full access), but
  U-mode sees no-access (deny by default).
- U-mode accesses without a matching PMP entry are **denied** (RISC-V spec).

---

## CFI Integration

### Forward Edge: Zicfilp Landing Pads

Every function that is a target of an indirect call/jump **must** begin with
an `lpad` instruction. On hardware with Zicfilp enabled:

- CPU checks that the instruction at the indirect jump/call target is `lpad`
- If not, a **software-check exception** (mcause=18) fires
- `lpad N` carries a label; the calling sequence can verify the label matches

```
Indirect call:                     Target function:
    la t1, target                      .4byte 0x00000017    // lpad 0
    jalr ra, t1, 0  ──────────────►    ... function body ...
                                       ret
         ┌─── Without lpad? CPU traps! (mcause=18)
```

### Backward Edge: Zicfiss Shadow Stack

Non-leaf functions push `ra` onto a **hardware shadow stack** at entry and
verify it at return. On mismatch, the CPU faults.

```
Function prologue:                 Function epilogue:
    .4byte 0x60100073  // sspush ra    .4byte 0x60500073  // sspopchk ra
    sw ra, 12(sp)      // regular      lw ra, 12(sp)
    ...                                ret
                                       │
    ┌─── If ra was corrupted (ROP), sspopchk detects mismatch → trap!
```

### Software Shadow Stack (Fallback for non-Zicfiss cores)

The same binary includes **both** Zicfiss instructions and a software shadow
stack (via the `gp` register). Since Zicfiss instructions encode in the
Zimop/Zcmop space, they execute as **guaranteed NOPs** on cores without CFI
extensions. This gives graceful degradation — one binary, two behaviors:

| Core has Zicfiss? | HW `sspush`/`sspopchk` | SW `gp`-based check | Effective protection |
|---|---|---|---|
| **Yes** | Enforced by CPU | Redundant (harmless overhead) | Hardware-grade |
| **No** | Executes as NOP | Active — sole defense | Software-grade |

```
Entry:
    .4byte 0x60100073       // HW sspush ra  (NOP if no Zicfiss)
    sw     ra, 0(gp)        // SW shadow stack push
    addi   gp, gp, 4

Return:
    addi   gp, gp, -4       // SW shadow stack pop
    lw     t0, 0(gp)
    lw     ra, 12(sp)
    bne    t0, ra, fault     // SW check
    .4byte 0x60500073       // HW sspopchk ra (NOP if no Zicfiss)
    ret
```

**Why not run both on Zicfiss cores?** On a Zicfiss-capable core the HW
shadow stack is strictly stronger — only `sspush`/`sspopchk` can write to
SS-attributed pages, so no software exploit can corrupt it. The SW check
uses regular RW memory reachable via `gp`, which an attacker who can
corrupt a register or write to that region can defeat. Running both
wastes cycles and burns the `gp` register for no additional security.

A production deployment would detect Zicfiss at boot and skip the SW
shadow stack path entirely, freeing `gp` for the global pointer or
thread-local storage. The current code runs both unconditionally because
it targets QEMU, where `sspush`/`sspopchk` are NOPs and only the SW
path provides real protection.

---

## Boot Sequence

```
 _start (M-mode, .text.init)
    │
    ├─ Set M-mode stack pointer
    ├─ Install trap handler (skips illegal CSR accesses)
    ├─ Zero BSS, copy .data
    ├─ Initialize M-mode software shadow stack (gp)
    │
    └─► rot_main() (M-mode Rust)
         │
         ├─ Phase 1: Enable CFI
         │   ├─ csrs menvcfg, LPE|SSE     (Zicfilp + Zicfiss for U-mode)
         │   ├─ csrs senvcfg, LPE|SSE     (forward-compat with S-mode)
         │   └─ csrw ssp, _m_shadow_stack_top
         │
         ├─ Phase 2: Configure PMP
         │   ├─ Write pmpaddr0..7
         │   └─ Write pmpcfg0, pmpcfg1
         │
         ├─ Phase 3: Measure firmware
         │   └─ rot_measure_firmware(U_CODE, 128K)  [CFI-protected]
         │
         ├─ Phase 4: Seal secrets
         │   └─ rot_seal_secret(data, key_id)       [CFI-protected, labeled lpad]
         │
         └─ Phase 5: Launch U-mode
              ├─ mstatus.MPP = 0b00 (User)
              ├─ mepc = _u_entry
              ├─ sp = _u_stack_top
              ├─ csrw ssp, _u_shadow_stack_top
              ├─ gp = _u_sw_shadow_stack_bottom
              └─ mret  ──────────────────►  _u_entry() (U-mode)
                                                │
                                                ├─ Indirect calls (Zicfilp enforced)
                                                ├─ Shadow stack active (Zicfiss)
                                                ├─ PMP enforced (cannot touch M-mode)
                                                └─ ecall for services
```

---

## Ecall Interface (U → M)

U-mode code requests M-mode services via the `ecall` instruction. The trap
handler dispatches on `a7` (syscall number):

| a7 | Name | Arguments | Description |
|---|---|---|---|
| 0 | `uart_putc` | a0 = char | Print one character |
| 1 | `uart_puts` | a0 = ptr, a1 = len | Print a string |
| 2 | `exit` | a0 = code | Halt system (QEMU test finisher) |
| 3 | `get_random` | a0 = &buf, a1 = len | Fill buffer with random bytes (stub) |

This is deliberately minimal. A production RoT would add:
- Key derivation / sealing / attestation
- Firmware update verification
- Monotonic counter access
- Secure storage read/write

---

## Attack Resistance

| Attack | Protection Mechanism |
|---|---|
| **ROP (Return-Oriented Programming)** | Zicfiss shadow stack detects corrupted return addresses |
| **JOP (Jump-Oriented Programming)** | Zicfilp landing pads prevent jumping to arbitrary gadgets |
| **Buffer overflow → code injection** | W^X via PMP: U-mode data is RW (no execute), code is RX (no write) |
| **Privilege escalation** | PMP denies U-mode access to M-mode memory; mret enforces privilege level |
| **M-mode code tampering** | PMP entry 0 is Locked RX — even M-mode cannot write its own code |
| **Shadow stack corruption (SW)** | Shadow stack in dedicated PMP region, spatially isolated from data |
| **Shadow stack corruption (HW)** | Zicfiss shadow stack pages have special attributes; normal stores rejected |
| **Key/secret exfiltration** | M_RAM region denied to U-mode; secrets only accessible via M-mode ecall |
| **Indirect call type confusion** | Labeled landing pads (`lpad N`) restrict which callers can reach a target |
| **Stack pivot** | Separate shadow stack means pivoting the main stack doesn't affect return addresses |

---

## Building & Running

```bash
cd rot/

# Build (uses custom target from .cargo/config.toml)
cargo build --release

# Inspect PMP + CFI instructions in the binary
llvm-objdump -d target/rv32imac-cfi-none-elf/release/riscv-rot-cfi | grep -E "csrw|lpad|sspush|sspop|0x00000017|0x60100073|0x60500073|0x3B0|0x3A0"

# Run in QEMU
qemu-system-riscv32 -machine virt -nographic -bios none \
    -kernel target/rv32imac-cfi-none-elf/release/riscv-rot-cfi

# On QEMU 9.x+ with CFI support:
qemu-system-riscv32 -machine virt \
    -cpu rv32,zicfilp=true,zicfiss=true,zimop=true,zcmop=true \
    -nographic -bios none \
    -kernel target/rv32imac-cfi-none-elf/release/riscv-rot-cfi
```

---

## File Structure

```
rot/
├── .cargo/config.toml      # Build target, linker flags, build-std
├── Cargo.toml               # Package manifest
├── build.rs                 # Linker script path setup
├── memory.x                 # Memory map (PMP-aligned regions)
├── link.x                   # Linker script (M-mode + U-mode sections)
└── src/
    └── main.rs              # M-mode RoT kernel + U-mode app
```

---

## Limitations & Future Work

| Limitation | Path Forward |
|---|---|
| `rustc` won't auto-emit `lpad`/`sspush` for RISC-V | Awaiting LLVM feature wiring in rustc |
| SW shadow stack always runs alongside HW on Zicfiss cores | Detect Zicfiss at boot; skip SW path when HW is available, freeing `gp` |
| Software shadow stack bypassable if attacker leaks `gp` | Hardware Zicfiss provides true protection; SW is fallback only |
| No MMU (PMP only) — coarser isolation granularity | Use Sv32 MMU for page-level protection if available |
| Measurement is XOR hash (stub) | Replace with SHA-256/384 (e.g., `sha2` crate or HW accelerator) |
| Crypto sealing is XOR (stub) | Replace with AES-GCM using device identity key |
| Single U-mode app | Extend with multiple PMP domains for multi-tenant firmware |
| No secure boot chain verification | Add signature verification of U-mode firmware before launch |
| PMP entry count limited (16 on most cores) | Use Smepmp or ePMP for more entries; combine small regions |
