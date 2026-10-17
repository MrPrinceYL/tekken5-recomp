# Tekken5Decomp: Tekken 5 (SLUS-21059, PlayStation 2), Spreadsheet Method

The CSVs in `sheets/` are the **single source of truth** for all of the game's code: the seven
Emotion Engine programs on the disc and the seven IOP modules the game runs on the I/O processor.
Everything else is a projection: `src/` (Ghidra C, for reading) and `rust/` (compilable Rust, for
running) are generated from the sheets and never edited by hand.

```
Tekken 5 (USA).iso
   │ tools/ps2_extract.py   (ISO9660 → files + disc_files.csv)
   │ tools/tk5data.py       (TK5DATA3.BIN table of contents, LZ decompression of entries 0..6)
   │ tools/make_elfs.py     (one ELF per program: main = SLUS_210.59 + the two images its boot
   │                         loader unpacks; the Arcade History titles, Devil Within, the loader)
   │ tools/iop_modules.py   (the IRX modules in TK5DATA3 entry 6, relocated to fixed IOP addresses)
   ▼
tools/run_ghidra.sh ── ghidra-cli ──► ../Tekken5/ghidra/TEKKEN5.gpr (one program per ELF / module)
   │ SetGp → Analyze → FixNoReturn → CreateGapFunctions → FixDelaySlotTargets →
   │ DisassemblePointerTargets → ExportPcode → ExportSheets   (IOP=1: the IOP modules)
   ▼
../Tekken5/export/<program>/, export_iop/<module>/
   │ tools/build_sheets.py [--iop]
   ▼
sheets/*.csv, sheets/instructions/<program>.csv, sheets/memory/<program>.csv, sheets/iop/...
   │ tools/gen_rust.py (+ tools/gen_iop.py)
   ▼
rust/ (Bevy app)                                     src/raw_c, src/units (C)
```

## Playing

Run **`play.bat`**. The first run builds everything (that takes a while: about 1.6 million
recompiled instructions), later runs start in seconds. The disc image is read from `T5_DISC`,
which defaults to `E:\Decompilation\Game\Tekken 5 (USA).iso`.

- Keyboard and gamepads work at once. F2 rebinds controls (`%APPDATA%\Tekken5Decomp\controls.json`).
  Default keyboard: arrows / WASD = D-pad, X or Space = Cross, C = Circle, Z = Square,
  V = Triangle, Q / E = L1 / R1, 1 / 3 = L2 / R2, Enter = Start, Tab / Right Shift = Select.
  A second gamepad becomes the controller in port 2.
- P opens the settings: volume, window mode, gamma, VSync, sharp or smooth picture, FPS counter
  and optional faster disc loading. F1 shows the debug overlay. Holding F3 fast-forwards (skips the
  idle time between frames). F11 or Alt+Enter toggles
  borderless fullscreen.
- Memory cards are folders (`memcard\port0`, `memcard\port1`; `T5_MEMCARD` moves them): every
  save file is an ordinary file there.
- `rust\t5_trace.log` is the diagnostics log (programs started, IOP modules, RPC calls, the main
  thread's position every 5 s). If something goes wrong, send it.

## Rebuilding from the disc

Python 3, Ghidra 12.1.4 with ghidra-emotionengine-reloaded 2.1.38 (**patched by
`tools/fix_ee_sleigh.py`**, see below), ghidra-cli and a Rust toolchain.

```bat
set ISO=E:\Decompilation\Game\Tekken 5 (USA).iso
set W=E:\Decompilation\Decomps\Tekken5
cd tools
python fix_ee_sleigh.py
python ps2_extract.py "%ISO%" %W%\extracted
python tk5data.py "%ISO%" %W%\extracted
python make_elfs.py %W%\extracted
python iop_modules.py %W%\extracted
set GHIDRA=E:\Decompilation\Tools\ghidra-cli\target\release\ghidra.exe
set T5_WORK=%W%
bash run_ghidra.sh
set IOP=1& bash run_ghidra.sh
python build_sheets.py && python build_sheets.py --iop && python gen_rust.py
```

## The disc

| File | What |
|---|---|
| `SLUS_210.59` | The boot ELF: crt0, Sony libraries, and in `.y4` the boot loader (0x00D060D0) |
| `TK5DATA3.BIN` | u32 count + {offset, size} table: entries 0..4 the extra programs (LZ-compressed ELFs), 5 the game's code + data (unpacked to 0x001F0180), 6 sound system + IOP module images (to 0x00488000), 7..9 data archives |
| `TK5DATA0/1/2/5.BIN` | Game data archives (read through Namco's NU file manager on the IOP) |
| `TK5DATA4.BIN` | The program loader (ELF at 0x01E00000) |
| `IOPRP300.IMG`, `IRXARC.BIN` | IOP kernel replacement image, IRX archive |

LZ format (TK5DATA3): a flag byte, its bits LSB first above a sentinel bit; 1 = literal byte,
0 = big-endian u16 back-reference (11-bit distance, 0 = 2048; 5-bit length, 0 = 32); flag 0 ends.

## Programs (`sheets/variants.csv`)

| Program | Source | |
|---|---|---|
| `main` | SLUS_210.59 + TK5DATA3 5, 6 | Tekken 5 |
| `tekken1`, `tekken2`, `tekken3` | TK5DATA3 0, 1, 2 | Arcade History |
| `starblade` | TK5DATA3 3 | StarBlade |
| `devil` | TK5DATA3 4 | Devil Within |
| `loader` | TK5DATA4.BIN | program loader |

Each is a whole program (LoadExecPS2 / ExecPS2 replace all of RAM), analysed as its own Ghidra
program. Every row of functions / calls is keyed by (variant, addr).

## Sheets

| Sheet | Key | Contents |
|---|---|---|
| `meta.csv` | `key` | Game ID, boot path, memory map, TK5DATA3 format, toolchain |
| `variants.csv` | `variant` | The programs: source, entry, gp, range, counts |
| `functions.csv` | `variant,addr` | name, size, section, module (game / sce), unit, status, owner, fan-in/out, signature, C path, notes |
| `units.csv` | `variant,unit` | Work partitions (16 KiB address ranges of game code, Sony libraries) with status roll-ups |
| `calls.csv` | `variant,caller,callee` | Static call graph |
| `instructions/<variant>.csv` | `addr` | Every instruction: function, bytes, disassembly, delay-slot flag, raw p-code |
| `memory/<variant>.csv` | `addr` | The program image, 64 bytes per row |
| `strings.csv`, `globals.csv`, `sections.csv`, `registers.csv` | | Ghidra's data |
| `disc_files.csv`, `tk5data3_toc.csv` | | The disc and the code archive |
| `iop/modules.csv` | `module` | IOP modules: IRX name, SHA-1, fixed address, entry, gp |
| `iop/functions.csv`, `iop/calls.csv`, `iop/strings.csv` | `module,addr` | As above, for the IOP modules |
| `iop/imports.csv`, `iop/exports.csv` | | Import stubs (library, index, name) and export tables |
| `iop/instructions/<module>.csv`, `iop/memory/<module>.csv` | | IOP code and images |

Status lifecycle (`functions.csv:status`): `raw` → `named` → `typed` → `clean` → `verified`.

## Rust / Bevy (`rust/`)

| Crate | What |
|---|---|
| `ee-core` | ABI of generated EE code: the R5900 context (128-bit GPRs, HI/LO pairs, SA, FPU with the EE's non-IEEE rules, COP0, VU0 registers), p-code ops (64 and 128 bit), MMI and VU0 macro instructions |
| `iop-core` | ABI of generated IOP code (R3000A) |
| `ps2-runtime` | The PS2: EE kernel (threads as coroutines, semaphores, interrupt handlers, alarms, program loading), DMAC, INTC, timers, GIF, GS with a software rasterizer, VIF0/1, VU0/1 micro mode, IPU (MPEG-2), SIF; the IOP: kernel libraries for the recompiled modules, RPC, cdvdman / ioman, padman, mcserv, SPU2 |
| `recomp/<variant>/part_NN` | **Generated.** `f_<ADDR>_<variant>` per function |
| `recomp_iop/<module>` | **Generated.** IOP module code `i_<ADDR>` |
| `t5-recomp` | Program and module tables, boot, `t5-headless` (automated boot checks) |
| `t5-bevy` | Bevy app: picture on a quad under a 3D orthographic camera, SPU2 audio, input |

### How the console is reproduced

- **CPU**: static recompilation of every instruction's p-code (one Rust function per guest
  function, one match arm per basic block; jumps out of a function are tail jumps). Opaque
  CALLOTHER ops (MMI, VU0 macro mode) are executed from the instruction word.
- **Threads**: EE kernel threads are host coroutines with their own stacks; the scheduler
  follows the kernel's priority rules. Interrupt handlers run at the next service point
  (function entry or loop back-edge).
- **IOP**: the game's own IOP modules (libsd, SPU2 synthesizer, NU file manager, NU sound,
  NU stream, sbse, sdr driver) are recompiled and run on coroutines too; Sony's kernel libraries
  they import are emulated at their import stubs. SIF RPC in both directions follows the
  protocol of the game's own libkernl.
- **Time**: device time is host time in EE cycles (294.912 MHz); vertical blank at 59.94 Hz,
  timers, DVD read speed, SPU2 output at 48 kHz follow it.
