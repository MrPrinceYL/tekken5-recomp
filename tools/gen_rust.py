"""Project the sheets into a Rust workspace.

sheets/instructions/<variant>.csv (p-code per instruction)
        -> rust/crates/recomp/<variant>/part_NN/src/lib.rs   (crates t5-<variant>-NN)
sheets/variants.csv + functions.csv + memory/<variant>.csv + hle.csv
        -> rust/crates/t5-recomp/src/table.rs                (programs, dispatch, HLE hooks)

Tekken 5 is several whole PS2 programs (sheets/variants.csv). Each runs alone (LoadExecPS2 /
ExecPS2 replace EE RAM), so a call is always linked directly to a function of the caller's own
program; only computed calls go through the runtime's dispatcher, which looks the target up in the
running program. Functions are `f_<ADDR>_<variant>`.

Every function becomes `pub fn f_<ADDR>_<variant>(c: &mut Ctx, entry: u32)`: a loop over a
`match` with one arm per basic block (dense indices -> jump table). Straight-line code inside a
block runs without dispatch; direct branches set the block index; computed jumps and mid-function
entries resolve through a per-function leader table. Jumps that leave the function are tail jumps
(`c.tail(target); return;`, continued by the caller: `Ctx::call` loops, direct calls are
followed by `c.settle()`), so they use no host stack. Block leaders: function entries, every
constant branch/call target, code addresses stored anywhere in the program image (jump tables,
callbacks, vtables), the instruction after any unconditional transfer. Branches back to an earlier
address (loops) poll for pending interrupts, so loops waiting on a flag set by an interrupt
handler terminate.

R5900 specifics:
- Ghidra folds each branch's delay slot into the branch's own p-code, so delay slot rows (column
  `delay` = 1) are skipped; one is only emitted on its own when it is a block leader.
- Ghidra's register offsets are remapped onto ee-core's compact register file (REG_BLOCKS).
  $zero reads as 0 and writes to it are dropped. Values of 16 bytes (128-bit GPRs, lq / sq,
  VU registers) are `u128`, everything else `u64`.
- COP0 registers go to the runtime (Status, Count, ...), as do `syscall`, `ei` / `di` and bc0x.
- CALLOTHER ops that Ghidra leaves opaque are implemented from the instruction word: COP2
  (VU0 macro mode, QMFC2 / QMTC2 / CFC2 / CTC2) by `ee_core::vu0::exec`, MMI by
  `ee_core::mmi::exec`. In a branch's p-code they belong to the delay slot.
Generated code is disposable: edit the sheets (or this generator), then rerun.
"""
import csv
import io
import os
import re
import shutil
import zlib
from collections import defaultdict
from pathlib import Path

csv.field_size_limit(1 << 30)
ROOT = Path(__file__).resolve().parent.parent
SHEETS, RUST = ROOT / "sheets", ROOT / "rust"
CRATES = RUST / "crates"
PREFIX = "t5"
PART_INSTR = int(os.environ.get("T5_PART_INSTR", "30000"))
IDENT_MAX = 0x1000  # program image bytes kept for identification

VN = re.compile(r"([CRUMX])([0-9a-f]+):(\d+)")
BIN = {
    "INT_ADD", "INT_SUB", "INT_MULT", "INT_DIV", "INT_SDIV", "INT_REM", "INT_SREM", "INT_AND", "INT_OR",
    "INT_XOR", "INT_LEFT", "INT_RIGHT", "INT_SRIGHT", "INT_EQUAL", "INT_NOTEQUAL", "INT_LESS",
    "INT_LESSEQUAL", "INT_SLESS", "INT_SLESSEQUAL", "INT_CARRY", "INT_SCARRY", "INT_SBORROW",
    "BOOL_AND", "BOOL_OR", "BOOL_XOR",
}
UN = {"INT_NEGATE", "INT_2COMP", "INT_ZEXT", "BOOL_NEGATE", "POPCOUNT", "LZCOUNT"}
FBIN = {"FLOAT_ADD", "FLOAT_SUB", "FLOAT_MULT", "FLOAT_DIV", "FLOAT_EQUAL", "FLOAT_NOTEQUAL",
        "FLOAT_LESS", "FLOAT_LESSEQUAL"}
FUN = {"FLOAT_NEG", "FLOAT_ABS", "FLOAT_SQRT", "FLOAT_CEIL", "FLOAT_FLOOR", "FLOAT_ROUND", "FLOAT_NAN"}

# Ghidra R5900 register offsets -> ee-core register file (see ee-core REG_*): (start, end, base).
REG_BLOCKS = [
    (0x0000, 0x0210, 0x000),  # zero_qw .. ra_qw, pc_qw (128-bit GPRs)
    (0x0300, 0x0328, 0x210),  # hi hi1 lo lo1 tsp
    (0x0400, 0x0408, 0x238),  # fsa (SA, in bits)
    (0x1000, 0x1080, 0x240),  # FPU f0..f31 (Ghidra's pair order)
    (0x1200, 0x1214, 0x2C0),  # fir fccr fexr fenr fcsr
    (0x1300, 0x1304, 0x2D4),  # FPU ACC
    (0x2000, 0x2080, 0x2E0),  # COP0 (accessed through the runtime, see COP0)
    (0x2100, 0x211C, 0x360),  # COP0 debug registers
    (0x2200, 0x220C, 0x37C),  # COP0 performance counters
    (0x4000, 0x4080, 0x390),  # VU0 vi0..vi15, vc0..vc15
    (0x4300, 0x4380, 0x410),  # vucr
    (0x4100, 0x4300, 0x500),  # VU0 vf0..vf31
    (0x4400, 0x4410, 0x700),  # VU0 ACC
]
COP0 = (0x2000, 0x2080)
ISA_MODE = 0x3F00

# CALLOTHER ops of a branch instruction itself (all others in a branch's p-code belong to its
# delay slot).
BRANCH_OWN = {"getCopCondition", "_bc2f", "_bc2fl", "_bc2t", "_bc2tl"}
NOP_OPS = {"SYNC", "prefetch", "cacheOp", "_vnop", "_vwaitq"}
COPY_OPS = {"_lqc2", "_sqc2"}


def read(path):
    """Rows of a CSV file as dicts."""
    with open(path, newline="", encoding="utf-8") as fh:
        return list(csv.DictReader(fh))


def parse_vn(tok):
    """A p-code varnode `<space><hexoffset>:<size>` as (space, offset, size); None for `_`."""
    if tok == "_":
        return None
    k, off, size = VN.fullmatch(tok).groups()
    return k, int(off, 16), int(size)


def parse_op(text):
    """One p-code op as (mnemonic, CALLOTHER name or None, output varnode, input varnodes)."""
    head, rest = text.split(" = ", 1) if " = " in text else (text, "")
    parts = head.split(" ")
    mnem = parts[0]
    name = None
    if mnem.startswith("CALLOTHER["):
        name = mnem[10:-1]
        mnem = "CALLOTHER"
    out = parse_vn(parts[1]) if len(parts) > 1 else None
    ins = [parse_vn(t) for t in rest.split()] if rest else []
    return mnem, name, out, ins


def write_if_changed(path, data):
    """Only touch files whose content changed, so cargo rebuilds just what changed."""
    if isinstance(data, str):
        data = data.encode("utf-8")
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and path.read_bytes() == data:
        return
    path.write_bytes(data)


class Out(io.StringIO):
    """`with Out(path) as fh:` collects text and writes it with write_if_changed on exit."""

    def __init__(self, path):
        super().__init__()
        self.path = path

    def __exit__(self, *exc):
        if exc[0] is None:
            write_if_changed(self.path, self.getvalue())
        return super().__exit__(*exc)


def mask(v, s):
    """`v` truncated to `s` bytes."""
    return v & ((1 << (8 * s)) - 1)


def ends_block(pcode):
    """True if control never falls through past this instruction unconditionally."""
    for text in pcode.split(" | "):
        m = text.split(" ", 1)[0]
        if m in ("BRANCH", "BRANCHIND", "RETURN"):
            return True
    return False


def word_of(row):
    """The instruction word of a row (bytes are in memory order, little endian)."""
    return int.from_bytes(bytes.fromhex(row["bytes"]), "little")


def find_leaders(instructions, entries, memory_rows):
    """Block-leader candidates of one program: function entries, constant branch/call targets
    and every aligned 32-bit word of the program image that is a code address. Over-inclusion
    only splits blocks; a missing leader would make a computed jump fail."""
    code = {int(r["addr"], 16) for r in instructions}
    leaders = set(entries)
    for r in instructions:
        for text in r["pcode"].split(" | "):
            if text.startswith(("BRANCH ", "CBRANCH ", "CALL ")):
                ins = parse_op(text)[3]
                if ins and ins[0] and ins[0][0] == "M":
                    leaders.add(ins[0][1])
        # Code addresses built as constants (lui / addiu of a handler, a callback).
        for m in VN.finditer(r["pcode"]):
            if m.group(1) == "C" and m.group(3) == "4":
                v = int(m.group(2), 16)
                if v in code and v & 3 == 0:
                    leaders.add(v)
    data_refs = 0
    for r in memory_rows:
        raw = bytes.fromhex(r["hex"])
        base = int(r["addr"], 16)
        for i in range(0, len(raw) - 3, 4):
            if (base + i) & 3:
                continue
            w = int.from_bytes(raw[i:i + 4], "little")
            if w in code and w not in leaders:
                leaders.add(w)
                data_refs += 1
    return leaders, data_refs


def reg_off(off):
    """ee-core register file offset of a Ghidra register offset."""
    for lo, hi, base in REG_BLOCKS:
        if lo <= off < hi:
            return base + off - lo
    raise ValueError(f"unmapped register offset {off:#x}")


class Gen:
    """Translates the p-code of one crate's functions (of one program) into Rust."""

    def __init__(self, variant, entries, all_entries, leaders, hooks):
        self.variant = variant
        self.suffix = f"_{variant}"
        self.entries = entries          # function entries of this part
        self.all_entries = all_entries  # every function entry of the program
        self.leaders = leaders
        self.hooks = set(hooks)
        self.blk = {}
        self.cur = 0
        self.asm = ""
        self.words = (0, 0)             # (own word, delay-slot word or own word)
        self.extern = set()
        self.uwide = set()              # unique temporaries that hold 16-byte values
        self.usub = {}                  # unique offset inside a wider unique -> (base, byte)

    # ---- values ----
    def rd(self, v, want_wide=False):
        """Rust expression reading a varnode: u64 (size <= 8) or u128 (size 16 or want_wide)."""
        k, off, s = v
        wide = s > 8
        if k == "C":
            e = f"0x{mask(off, s):x}u128" if wide else f"0x{mask(off, s):x}u64"
        elif k == "R":
            if off < 0x10:
                e = "0u128" if wide else "0u64"
            elif COP0[0] <= off < COP0[1]:
                e = f"mask(c.cop0_read({(off - COP0[0]) // 4}),{s})"
            else:
                o = reg_off(off)
                e = f"c.r128(0x{o:x})" if wide else f"c.r(0x{o:x},{s})"
        elif k == "U":
            if off in self.usub:
                # Bytes of a wider temporary (multu's HI is `Uaf04:4` of the product `Uaf00:8`).
                b, i = self.usub[off]
                e = f"mask((u{b:x}>>{8 * i}) as u64,{s})"
            elif off in self.uwide and not wide:
                e = f"(u{off:x} as u64)"
            else:
                e = f"u{off:x}"
        elif k == "M":
            e = f"c.ld128(0x{off:x})" if wide else f"c.ld(0x{off:x},{s})"
        else:
            raise ValueError(f"varnode space {k} at {self.cur:#x}")
        if want_wide and not wide:
            return f"({e} as u128)"
        return e

    def wr(self, v, expr, wide):
        """Rust statement writing `expr` (u128 if `wide`) to a varnode."""
        k, off, s = v
        dst_wide = s > 8
        if dst_wide and not wide:
            expr = f"({expr}) as u128"
        elif not dst_wide and wide:
            expr = f"(({expr}) as u64)"
        if k == "R" and (off < 0x10 or off == ISA_MODE):
            return f"let _={expr};" if "c." in expr else ""
        if k != "U" and "c." in expr:
            t = "u128" if dst_wide else "u64"
            return f"{{let v:{t}={expr};{self.wr(v, 'v', dst_wide)}}}"
        if k == "R":
            if COP0[0] <= off < COP0[1]:
                return f"c.cop0_write({(off - COP0[0]) // 4},{expr});"
            o = reg_off(off)
            return f"c.wr128(0x{o:x},{expr});" if dst_wide else f"c.wr(0x{o:x},{s},{expr});"
        if k == "U":
            if off in self.usub:
                raise ValueError(f"write to part of a temporary U{off:x}")
            if off in self.uwide and not dst_wide:
                return f"u{off:x}=({expr}) as u128;"
            return f"u{off:x}={expr};"
        if k == "M":
            return f"c.st128(0x{off:x},{expr});" if dst_wide else f"c.st(0x{off:x},{s},{expr});"
        raise ValueError(v)

    # ---- control flow ----
    def branch(self, target, idx, own, relative):
        """Rust code for a branch: p-code-relative, a block change, or a tail jump."""
        k, off, s = target
        if k == "C":
            rel = off - (1 << 32) if off >= 1 << 31 else off
            relative[0] = True
            return f"i={idx + rel};continue;"
        if off in own and off in self.blk:
            poll = f"if c.poll_at(0x{self.cur:x}){{return;}}" if off <= self.cur else ""
            return f"{poll}b={self.blk[off]};continue 'f;"
        return f"c.tail(0x{off:x});return;"

    def call(self, off):
        """Rust code for a direct call (the program's own function) or one through the dispatcher."""
        if off in self.entries:
            return f"f_{off:08X}{self.suffix}(c,0x{off:x});c.settle();if c.abort{{return;}}"
        if off in self.all_entries:
            self.extern.add(off)
            return f"unsafe{{f_{off:08X}{self.suffix}(c,0x{off:x})}};c.settle();if c.abort{{return;}}"
        return f"c.call(0x{off:x});if c.abort{{return;}}"

    def callother(self, name, out, ins):
        """Rust code for a CALLOTHER op."""
        word = self.words[0] if name in BRANCH_OWN else self.words[1]
        if name in NOP_OPS:
            return ""
        if name == "syscall":
            return f"c.syscall(0x{word:x});if c.abort{{return;}}"
        if name == "trap":
            return f"c.brk(0x{word:x});"
        if name == "EI":
            return "c.ei(true);"
        if name == "DI":
            return "c.ei(false);"
        if name in COPY_OPS:
            return self.wr(out, self.rd(ins[0], True), True)
        if name == "getCopCondition":
            cop = ins[0][1] if ins and ins[0] and ins[0][0] == "C" else 0
            return self.wr(out, f"(c.cop0_read(0x{0x100 + cop:x})&1)", False)
        if name.startswith("_bc2"):
            busy = "((c.host.vu0_busy)(c) as u64)"
            return self.wr(out, busy if name.startswith("_bc2t") else f"({busy}^1)", False) if out else ""
        if name.startswith("_v") or name in ("_qmfc2", "_qmtc2", "_cfc2", "_ctc2"):
            return f"vu0::exec(c,0x{word:x});"
        if name.startswith(("_p", "_q")) or name in ("countLeadingZeros", "countLeadingOnes"):
            return f"mmi::exec(c,0x{word:x});"
        call = f'c.intrinsic("{name}",0x{word:x})'
        return (self.wr(out, "0u64", False) + call + ";") if out else call + ";"

    def op(self, text, idx, own, relative):
        """Rust code for one p-code op."""
        mnem, name, out, ins = parse_op(text)
        if mnem == "COPY":
            w = out[2] > 8
            return self.wr(out, self.rd(ins[0], w), w)
        if mnem == "LOAD":
            a = self.rd(ins[0])
            if out[2] > 8:
                return self.wr(out, f"c.ld128(({a}) as u32)", True)
            return self.wr(out, f"c.ld(({a}) as u32,{out[2]})", False)
        if mnem == "STORE":
            a = self.rd(ins[0])
            s = ins[1][2]
            v = self.rd(ins[1])
            fn = "c.st128(a,v)" if s > 8 else f"c.st(a,{s},v)"
            t = "u128" if s > 8 else "u64"
            return f"{{let a=({a}) as u32;let v:{t}={v};{fn};}}"
        sizes = [v[2] for v in ins if v] + ([out[2]] if out else [])
        wide = any(s > 8 for s in sizes)
        p = "w_" if wide else ""
        if mnem in BIN:
            return self.wr(out, f"{p}{mnem.lower()}({self.rd(ins[0], wide)},{self.rd(ins[1], wide)},{ins[0][2]})", wide)
        if mnem in UN:
            return self.wr(out, f"{p}{mnem.lower()}({self.rd(ins[0], wide)},{ins[0][2]})", wide)
        if mnem == "INT_SEXT":
            if wide:
                return self.wr(out, f"wmask(w_int_sext({self.rd(ins[0], True)},{ins[0][2]}),{out[2]})", True)
            return self.wr(out, f"mask(int_sext({self.rd(ins[0])},{ins[0][2]}),{out[2]})", False)
        if mnem == "SUBPIECE":
            return self.wr(out, f"{p}subpiece({self.rd(ins[0], wide)},{ins[1][1]},{out[2]})", wide)
        if mnem == "PIECE":
            return self.wr(out, f"{p}piece({self.rd(ins[0], wide)},{self.rd(ins[1], wide)},{ins[1][2]})", wide)
        if mnem in FBIN:
            return self.wr(out, f"{mnem.lower()}({self.rd(ins[0])},{self.rd(ins[1])},{ins[0][2]})", False)
        if mnem in FUN:
            return self.wr(out, f"{mnem.lower()}({self.rd(ins[0])},{ins[0][2]})", False)
        if mnem in ("FLOAT2FLOAT", "INT2FLOAT", "TRUNC"):
            return self.wr(out, f"{mnem.lower()}({self.rd(ins[0])},{ins[0][2]},{out[2]})", False)
        if mnem == "BRANCH":
            return self.branch(ins[0], idx, own, relative)
        if mnem == "CBRANCH":
            return f"if {self.rd(ins[1])}!=0{{{self.branch(ins[0], idx, own, relative)}}}"
        if mnem == "BRANCHIND":
            return (f"{{let t=({self.rd(ins[0])}) as u32;let k=blk(t);"
                    f"if k!=0{{if c.poll(){{return;}}b=k;continue 'f;}}c.tail(t);return;}}")
        if mnem == "CALL":
            k, off, _ = ins[0]
            return self.call(off) if k == "M" else f"c.call(({self.rd(ins[0])}) as u32);if c.abort{{return;}}"
        if mnem == "CALLIND":
            return f"c.call(({self.rd(ins[0])}) as u32);if c.abort{{return;}}"
        if mnem == "RETURN":
            return "return;"
        if mnem == "CALLOTHER":
            return self.callother(name, out, ins)
        if mnem == "UNIMPLEMENTED":
            return f"c.brk(0xDEAD);return;"
        raise ValueError(f"unhandled p-code op {mnem} at {self.cur:#x}")

    def instruction(self, row, own, slot):
        """Rust block for one instruction's p-code."""
        self.cur = int(row["addr"], 16)
        self.asm = row["asm"]
        w = word_of(row)
        self.words = (w, word_of(slot) if slot is not None else w)
        ops = [p for p in row["pcode"].split(" | ") if p]
        uses = defaultdict(set)
        for m in VN.finditer(" | ".join(ops)):
            if m.group(1) == "U":
                uses[int(m.group(2), 16)].add(int(m.group(3)))
        self.uwide = {u for u, ss in uses.items() if max(ss) > 8}
        self.usub = {u: (b, u - b) for u in uses for b, ss in uses.items() if b < u < b + max(ss)}
        relative = [False]
        try:
            body = [self.op(o, i, own, relative) for i, o in enumerate(ops)]
        except Exception as e:
            raise ValueError(f"{self.variant} {self.cur:#010x} {self.asm}: {e}") from e
        decl = "".join(f"let mut u{u:x}:{'u128' if u in self.uwide else 'u64'}=0;" for u in sorted(uses) if u not in self.usub)
        if relative[0]:
            arms = "".join(f"{i}=>{{{b}i={i + 1};}}" for i, b in enumerate(body))
            return f"{{{decl}let mut i:i64=0;loop{{match i{{{arms}_=>break}}}}}}"
        return "{" + decl + "".join(body) + "}"

    def function(self, entry, name, rows):
        """The Rust function for one guest function: a loop over a match, one arm per block."""
        own = {int(r["addr"], 16) for r in rows}
        delay_of = {}
        for k, r in enumerate(rows):
            if r["delay"] == "1" and k > 0:
                delay_of[int(rows[k - 1]["addr"], 16)] = r
        emit = [r for r in rows if r["delay"] != "1" or int(r["addr"], 16) in self.leaders]
        lead, prev_nxt, after_jump = [], None, False
        for r in emit:
            a = int(r["addr"], 16)
            if a == entry or a in self.leaders or after_jump or prev_nxt != a:
                lead.append(a)
            after_jump = ends_block(r["pcode"]) or r["delay"] == "1"
            prev_nxt = a + (8 if a in delay_of else 4)
            if r["delay"] == "1":
                prev_nxt = a + 4
        self.blk = {a: i + 1 for i, a in enumerate(lead)}
        lead_set = set(lead)
        table = ",".join(f"0x{a:x}=>{i + 1}" for i, a in enumerate(lead))
        hook = "true" if entry in self.hooks else "false"
        lines = [f"/// {name} ({len(rows)} instructions, {len(lead)} blocks)",
                 f"#[unsafe(no_mangle)]\npub fn f_{entry:08X}{self.suffix}(c:&mut Ctx,entry:u32){{if c.enter(0x{entry:x},entry,{hook}){{return;}}",
                 f"fn blk(t:u32)->usize{{match t{{{table},_=>0}}}}",
                 (f"let mut b:usize=if entry==0x{entry:x}{{{self.blk[entry]}}}else{{blk(entry)}};"
                  if entry in self.blk else "let mut b:usize=blk(entry);"),
                 f"if b==0{{c.block_miss(0x{entry:x},entry);return;}}",
                 "'f:loop{match b{"]
        for k, r in enumerate(emit):
            a = int(r["addr"], 16)
            nxt = a + (8 if a in delay_of else 4)
            if r["delay"] == "1":
                nxt = a + 4
            if a in lead_set:
                lines.append(f"{self.blk[a]}=>{{")
            asm = r["asm"] + (f" ; {delay_of[a]['asm']}" if a in delay_of else "")
            lines.append(f"// {a:08X} {asm}\n{self.instruction(r, own, delay_of.get(a))}")
            last = k + 1 == len(emit) or int(emit[k + 1]["addr"], 16) in lead_set
            if last:
                if nxt in self.blk:
                    lines.append(f"b={self.blk[nxt]};continue 'f;}}")
                else:
                    lines.append(f"c.tail(0x{nxt:x});return;}}")
        lines.append("_=>return}}}")
        return "\n".join(lines)


PART_OPT = int(os.environ.get("T5_PART_OPT", "3"))


def unconditional(pcode):
    """True if control never falls through this instruction (an unconditional jump / return)."""
    ops = [t.split(" ", 1)[0] for t in pcode.split(" | ")]
    return "CBRANCH" not in ops and any(o in ("BRANCH", "BRANCHIND", "RETURN") for o in ops)


def group(instructions):
    """entry -> rows. Code outside any Ghidra function joins the function it falls through from
    (code after a call Ghidra believes never returns); other such code (reached only through
    pointers: handlers stored by lui / addiu, jump-table cases) becomes its own function, one per
    contiguous run."""
    by_addr = {int(r["addr"], 16): r for r in instructions}
    owner = {}
    by_func = defaultdict(list)
    merged = 0
    for r in instructions:
        a = int(r["addr"], 16)
        if r["func"]:
            f = int(r["func"], 16)
        else:
            prev = by_addr.get(a - 4)
            prev2 = by_addr.get(a - 8)
            falls = (prev is not None and (a - 4) in owner and not unconditional(prev["pcode"])
                     and not (prev["delay"] == "1" and prev2 is not None and unconditional(prev2["pcode"])))
            if falls:
                f = owner[a - 4]
                merged += 1
            else:
                f = a
        owner[a] = f
        by_func[f].append(r)
    for rows in by_func.values():
        rows.sort(key=lambda r: int(r["addr"], 16))
    return by_func, merged


def ranges(rows):
    """The contiguous address ranges [start, end) of a function's instructions."""
    out = []
    for r in rows:
        a = int(r["addr"], 16)
        if out and out[-1][1] == a:
            out[-1][1] = a + 4
        else:
            out.append([a, a + 4])
    return out


def crate_toml(name):
    return (f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n\n'
            f'[dependencies]\nee-core = {{ path = "../../../ee-core" }}\n')


def gen_variant(v, funcs, hle_rows):
    """Generate the crates of one program; returns (crate names, crate dirs, function names,
    entry list)."""
    name = v["variant"]
    print(f"{name}: reading instructions ...", flush=True)
    instructions = read(SHEETS / "instructions" / f"{name}.csv")
    memory_rows = read(SHEETS / "memory" / f"{name}.csv")
    by_func, merged = group(instructions)
    entries = sorted(by_func)
    leaders, data_refs = find_leaders(instructions, set(entries), memory_rows)
    owner = {}
    for f, rows in by_func.items():
        for r in rows:
            owner[int(r["addr"], 16)] = f
    for a, f in owner.items():
        prev = owner.get(a - 4)
        if prev is not None and prev != f:
            leaders.add(a)
    print(f"{name}: {len(entries)} functions ({merged} stray instructions merged), "
          f"{len(leaders)} block leaders ({data_refs} from data)", flush=True)
    hooks = [int(r["addr"], 16) for r in hle_rows if r["variant"] == name]

    # Parts of about PART_INSTR instructions.
    parts, cur, cnt = [], [], 0
    for e in entries:
        cur.append(e)
        cnt += len(by_func[e])
        if cnt >= PART_INSTR:
            parts.append(cur)
            cur, cnt = [], 0
    if cur:
        parts.append(cur)
    all_entries = set(entries)
    crates, dirs, names = [], [], []
    vdir = CRATES / "recomp" / name
    keep = {f"part_{n:02}" for n in range(len(parts))}
    if vdir.exists():
        for stale in vdir.iterdir():
            if stale.name not in keep:
                shutil.rmtree(stale)
    for n, part in enumerate(parts):
        crate = f"{PREFIX}-{name}-{n:02}"
        d = vdir / f"part_{n:02}"
        write_if_changed(d / "Cargo.toml", crate_toml(crate))
        g = Gen(name, set(part), all_entries, leaders, hooks)
        with Out(d / "src" / "lib.rs") as fh:
            fh.write("//! GENERATED by tools/gen_rust.py from sheets/instructions. Do not edit.\n"
                     f"//! Program: {name}, part {n}.\n"
                     "#![allow(unused, unreachable_code, non_snake_case, unused_parens, unused_braces, clippy::all)]\n"
                     "use ee_core::{Ctx, ops::*, vu0, mmi};\n\n")
            # Exact, non-overlapping address ranges -> function (the dispatcher's lookup).
            fh.write("pub static FUNCS: &[(u32, u32, fn(&mut Ctx, u32))] = &[\n")
            for e in part:
                for lo, hi in ranges(by_func[e]):
                    fh.write(f"    (0x{lo:x}, 0x{hi:x}, f_{e:08X}{g.suffix}),\n")
                f = funcs.get((name, e))
                names.append((e, f["name"] if f else f"orphan_{e:08X}"))
            fh.write("];\n\n")
            for e in part:
                f = funcs.get((name, e))
                fname = f["name"] if f else f"orphan_{e:08X} (code outside any function)"
                fh.write(g.function(e, fname, by_func[e]) + "\n\n")
            fh.write('unsafe extern "Rust" {\n')
            for e in sorted(g.extern):
                fh.write(f"    fn f_{e:08X}{g.suffix}(c: &mut Ctx, entry: u32);\n")
            fh.write("}\n")
        crates.append(crate)
        dirs.append(f"crates/recomp/{name}/part_{n:02}")
        print(f"  {crate}: {len(part)} functions, {sum(len(by_func[e]) for e in part)} instructions", flush=True)
    # The program image from its entry point on (identification, see `idents`).
    entry = int(v["entry"], 16)
    image = b""
    for r in memory_rows:
        a = int(r["addr"], 16)
        raw = bytes.fromhex(r["hex"])
        if a <= entry < a + len(raw) or image:
            image += raw[max(0, entry - a):] if not image else raw
        if len(image) >= IDENT_MAX:
            break
    return crates, dirs, names, entries, image[:IDENT_MAX], by_func


def idents(progs):
    """Per program, the image bytes the runtime matches in RAM at its entry point: at least 64,
    and as many as it takes to differ from every other program's image at that address
    (tekken1 and tekken2 share entry point and crt0)."""
    out = []
    for v, _, _, img in progs:
        e = int(v["entry"], 16)
        others = [w_img[e - int(w["entry"], 16):] for w, _, _, w_img in progs
                  if w is not v and 0 <= e - int(w["entry"], 16) < len(w_img)]
        n = 64
        while any(o[:n] == img[:n] for o in others if len(o) >= n):
            n += 4
            assert n <= len(img), f"{v['variant']}: no unique identification within {IDENT_MAX} bytes"
        out.append(img[:n])
    return out


def main():
    """Generate the whole Rust workspace from the sheets."""
    funcs = {(r["variant"], int(r["addr"], 16)): r for r in read(SHEETS / "functions.csv")}
    variants = read(SHEETS / "variants.csv")
    only = os.environ.get("T5_VARIANTS")
    hle_path = SHEETS / "hle.csv"
    hle_rows = [r for r in read(hle_path) if r["addr"]] if hle_path.exists() else []
    for r in hle_rows:
        assert (r["variant"], int(r["addr"], 16)) in funcs, f"hle.csv: no function {r['variant']} {r['addr']}"
    all_crates, all_dirs, progs = [], [], []
    for v in variants:
        if only and v["variant"] not in only.split(","):
            continue
        crates, dirs, names, entries, ident, by_func = gen_variant(v, funcs, hle_rows)
        all_crates += crates
        all_dirs += dirs
        progs.append((v, crates, names, ident))
    progs = [(v, c, n, i) for (v, c, n, _), i in zip(progs, idents(progs))]

    import gen_iop
    iop_crates, iop_dirs = gen_iop.generate(CRATES, PART_OPT)
    all_crates += iop_crates
    all_dirs += iop_dirs
    import gen_vu
    vu_crates, vu_dirs = gen_vu.generate()
    all_crates += vu_crates
    all_dirs += vu_dirs

    top = CRATES / f"{PREFIX}-recomp"

    def rs(c):
        return c.replace("-", "_")

    deps = "\n".join(f'{c} = {{ path = "../{d[len("crates/"):]}" }}' for c, d in zip(all_crates, all_dirs))
    write_if_changed(top / "Cargo.toml",
        f'[package]\nname = "{PREFIX}-recomp"\nversion = "0.1.0"\nedition = "2024"\n\n'
        f'[dependencies]\nee-core = {{ path = "../ee-core" }}\nbacktrace = "0.3"\nps2-runtime = {{ path = "../ps2-runtime" }}\n{deps}\n')
    with Out(top / "src" / "table.rs") as fh:
        fh.write("//! GENERATED by tools/gen_rust.py. Do not edit.\nuse ps2_runtime::Program;\n\n")
        fh.write("/// The game's programs (sheets/variants.csv).\npub static PROGRAMS: &[Program] = &[\n")
        for v, crates, names, ident in progs:
            parts = ", ".join(f"{rs(c)}::FUNCS" for c in crates)
            hooks = ", ".join(f'(0x{int(r["addr"], 16):08x}, "{r["name"]}")' for r in hle_rows if r["variant"] == v["variant"])
            fh.write(f'    Program {{\n        name: "{v["variant"]}",\n        title: "{v["title"]}",\n'
                     f'        entry: {v["entry"]},\n        gp: {v["gp"]},\n'
                     f'        ident: &{list(ident)},\n'
                     f'        parts: &[{parts}],\n        hle: &[{hooks}],\n        names: NAMES_{rs(v["variant"]).upper()},\n    }},\n')
        fh.write("];\n\n")
        for v, crates, names, ident in progs:
            fh.write(f"static NAMES_{rs(v['variant']).upper()}: &[(u32, &str)] = &[\n")
            for e, n in sorted(names):
                fh.write(f'    (0x{e:08x}, "{n}"),\n')
            fh.write("];\n\n")

    pkgs = ["ps2-runtime", f"{PREFIX}-recomp", f"{PREFIX}-bevy"]
    write_if_changed(RUST / "Cargo.toml",
        '[workspace]\nresolver = "3"\nmembers = [\n    "crates/ee-core",\n    "crates/ps2-runtime",\n'
        f'    "crates/{PREFIX}-recomp",\n    "crates/{PREFIX}-bevy",\n' + "".join(f'    "{m}",\n' for m in all_dirs) + "]\n\n"
        "# The game is played from this profile (play.bat). Debug assertions and integer overflow\n"
        "# checks would sit on every guest instruction and throughout Bevy, so they are off.\n"
        "[profile.dev]\nopt-level = 1\ndebug = \"line-tables-only\"\ndebug-assertions = false\noverflow-checks = false\n\n"
        "# Bevy and the runtime are small and hot: optimize them even in dev builds.\n"
        '[profile.dev.package."*"]\nopt-level = 3\n\n'
        + "".join(f'[profile.dev.package.{p}]\nopt-level = 3\n\n' for p in pkgs) +
        '[profile.dev.package.ee-core]\nopt-level = 3\ncodegen-units = 1\nincremental = false\n\n'
        "# The generated crates: T5_PART_OPT (default 3) when generating; 1 builds faster, runs slower.\n"
        + "".join(f'[profile.dev.package.{c}]\nopt-level = {PART_OPT}\ndebug = false\ncodegen-units = 16\nincremental = false\n\n' for c in all_crates)
        + "[profile.release]\ncodegen-units = 16\n")
    print(f"{len(progs)} programs, {len(all_crates)} generated crates")


if __name__ == "__main__":
    main()
