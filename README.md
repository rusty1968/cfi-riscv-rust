# RISC-V Bare Metal CFI Demo

A `#![no_std]` Rust project demonstrating **DIY Control Flow Integrity** on RV32 using the RISC-V CFI extensions:

- **Zicfilp** — forward-edge CFI via landing pads (`lpad`) at indirect call/jump targets
- **Zicfiss** — backward-edge CFI via hardware shadow stack (`sspush`/`sspopchk`)
- **Software shadow stack** — always-available fallback using the `gp` register
- **DIY KCFI** — type-based indirect call verification (type hash at `[fn-4]`, checked before every indirect call)

All hardware CFI instructions are encoded in the **Zimop/Zcmop** (May-Be-Operations) space, so they execute as NOPs on hardware that lacks CFI support. This gives you graceful degradation: hardware enforcement when available, software fallback when not.

## What it demonstrates

| Test | Description |
|------|-------------|
| 1 | Direct calls to CFI-protected naked functions |
| 2 | Indirect calls via function pointers (landing pad enforcement) |
| 3 | Dispatch table pattern — typical real-world use case for forward-edge CFI |
| 4 | Non-leaf `call_and_inc` with full forward + backward CFI |
| 5 | Shadow stack pointer inspection |
| 6 | KCFI type hash verification — reads hashes from memory, verifies checks pass |

## Building

Requires Rust **nightly** (for `build-std` and `json-target-spec`):

```
cargo build --release
```

The project uses a custom target spec ([rv32imac-cfi-none-elf.json](rv32imac-cfi-none-elf.json)) with `+zicfilp`, `+zicfiss`, `+zimop`, and `+zcmop` features. The nightly toolchain is pinned via [rust-toolchain.toml](rust-toolchain.toml).

> **Note:** As of LLVM 21, `+zicfilp` and `+zicfiss` are not recognized for RISC-V targets (silently ignored). All CFI instructions are emitted as raw `.4byte` encodings.

## Running on QEMU

```
cargo run --release
```

This launches `qemu-system-riscv32 -machine virt`. QEMU < 9.0 doesn't support the CFI CSRs (`menvcfg`, `ssp`), but the included trap handler skips illegal instructions gracefully.

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

[Test 6] KCFI type hash verification
  (Type hash at [fn-4] checked before every indirect call)
  hash at triple-4:       0x4b434649 (expected 0x4b434649)
  hash at add_42-4:       0x4b434649 (expected 0x4b434649)
  hash at square-4:       0x4b434649 (expected 0x4b434649)
  hash at call_and_inc-4: 0x4b434650 (expected 0x4b434650)
  kcfi_check triple:  PASS
  kcfi_check add_42:  PASS
  kcfi_check square:  PASS
  KCFI dispatch(0, 5) = 15 (expected 15)
  KCFI call_and_inc(add_42, 10) = 53 (expected 53)

============================================
  CFI Protection Summary:
  - Forward-edge:  lpad at indirect call targets
  - Backward-edge: sspush/sspopchk in prologue/epilogue
  - Fallback:      software shadow stack via gp register
  - Type-based:    KCFI hash at [fn-4] checked before indirect calls
  - HW instructions are NOPs on non-CFI hardware (safe)
============================================

All tests passed.
```

## Project structure

```
.
├── src/main.rs                  # Demo: CFI macros, naked functions, tests
├── rv32imac-cfi-none-elf.json   # Custom target spec with CFI features
├── memory.x                     # Memory layout (QEMU virt: 512K FLASH + 256K RAM)
├── link.x                       # Linker script (shadow stack sections)
├── build.rs                     # Linker search path setup
├── rust-toolchain.toml          # Pins nightly + rust-src
├── .cargo/config.toml           # Target, runner, rustflags
└── docs/
    └── cfi.md                   # Detailed implementation guide
```

## DIY KCFI

LLVM's `-Zsanitizer=kcfi` inserts type hashes before function entries and checks them at indirect call sites — but it only supports **x86_64** and **aarch64**, not RISC-V. So we implement it by hand:

- A 32-bit type hash is placed at `[fn_addr - 4]` (immediately before the landing pad)
- Before every indirect call, the caller loads the hash and compares it to the expected type
- On mismatch, `ebreak` traps — preventing calls through corrupted or type-confused function pointers
- Functions are defined via `global_asm!()` to control symbol placement relative to the hash

This mirrors exactly how LLVM's KCFI backend works (xxHash64 of the mangled type, truncated to 32 bits, placed at offset -4).

## Key design decisions

- **`gp` as software shadow stack pointer** — requires `--no-relax` to disable GP relaxation
- **`global_asm!()` for CFI functions** — allows placing KCFI hash at `[symbol - 4]` before the landing pad
- **Raw `.4byte` encodings** — necessary because LLVM doesn't yet emit `lpad`/`sspush`/`sspopchk` for RISC-V
- **Trap handler for CSR access** — graceful degradation on hardware/emulators without CFI CSRs

## References

- [RISC-V CFI Specification](https://github.com/riscv/riscv-cfi) (Zicfilp & Zicfiss)
- [docs/cfi.md](docs/cfi.md) — step-by-step implementation guide with lessons learned

## License

MIT
