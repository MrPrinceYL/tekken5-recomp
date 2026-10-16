"""IOP part of the Rust projection (called by gen_rust.py).

sheets/iop/instructions/<module>.csv -> rust/crates/recomp_iop/<module>/src/lib.rs (t5-iop-<module>)
sheets/iop/modules.csv + imports.csv + exports.csv -> rust/crates/t5-recomp/src/iop_table.rs

The IOP is an R3000A (Ghidra MIPS:LE:32:default), translated as Tekken3Decomp does it: 32-bit
registers, one function per guest function with one match arm per basic block, tail jumps for
jumps out of a function. Import stubs (`jr ra; addiu zero,zero,n`) are compiled with an HLE hook:
the runtime's IOP kernel serves the library function, or forwards it to the module that exports
it. Every module lives at its own fixed address (tools/iop_modules.py), so names are
`i_<ADDR>` and calls between functions of one module are linked directly.
"""
import csv
from collections import defaultdict
from pathlib import Path

import gen_rust as g

IOP_REGMAP = {o: o for o in range(0, 0x80)}
IOP_REGMAP.update({0x80: 0x80, 0x3000: 0x84, 0x3004: 0x88, 0x2030: 0x8C, 0x2034: 0x90, 0x2038: 0x94})
COP0_NUM = {0x2030: 12, 0x2034: 13, 0x2038: 14}
ISA_MODE = 0x3F00


class GenIop:
    """Translates one IOP module's p-code into Rust (32-bit values carried in u64)."""

    def __init__(self, entries, leaders, hooks):
        self.entries = entries
        self.leaders = leaders
        self.hooks = hooks
        self.blk = {}
        self.cur = 0
        self.asm = ""

    def reg(self, off):
        if off not in IOP_REGMAP:
            raise ValueError(f"unmapped IOP register {off:#x} at {self.cur:#x} ({self.asm})")
        return IOP_REGMAP[off]

    def rd(self, v):
        k, off, s = v
        if k == "C":
            return f"0x{g.mask(off, s):x}u64"
        if k == "R":
            if off < 4:
                return "0u64"
            return f"(c.r(0x{self.reg(off):x},{s}) as u64)"
        if k == "U":
            return f"u{off:x}"
        return f"(c.ld(0x{off:x},{s}) as u64)"

    def wr(self, v, expr):
        k, off, s = v
        if k == "R" and (off < 4 or off == ISA_MODE):
            return f"let _={expr};" if "c." in expr else ""
        if k != "U" and "c." in expr:
            return f"{{let v={expr};{self.wr(v, 'v')}}}"
        if k == "R":
            if off in COP0_NUM:
                return f"c.mtc0({COP0_NUM[off]},({expr}) as u32);"
            return f"c.wr(0x{self.reg(off):x},{s},({expr}) as u32);"
        if k == "U":
            return f"u{off:x}={expr};"
        return f"c.st(0x{off:x},{s},({expr}) as u32);"

    def branch(self, target, idx, own, relative):
        k, off, s = target
        if k == "C":
            rel = off - (1 << 32) if off >= 1 << 31 else off
            relative[0] = True
            return f"i={idx + rel};continue;"
        if off in own and off in self.blk:
            poll = "c.poll();if unsafe{iop_core::ABORT}{return;}" if off <= self.cur else ""
            return f"{poll}b={self.blk[off]};continue 'f;"
        return f"c.tail(0x{off:x});return;"

    def call(self, off):
        if off in self.entries:
            return f"i_{off:08X}(c,0x{off:x});c.settle();if unsafe{{iop_core::ABORT}}{{return;}}"
        return f"c.call(0x{off:x});if unsafe{{iop_core::ABORT}}{{return;}}"

    def callother(self, name, out, ins):
        if name == "setISAMode":
            return ""
        if name == "syscall":
            return f"c.syscall(({self.rd(ins[0])}) as u32);"
        if name == "trap":
            return f"c.brk(({self.rd(ins[0])}) as u32);"
        if name in ("getCopReg", "getCopControlWord") and ins and ins[0][1] == 0:
            sel = ins[1]
            n = sel[1] >> 11 if sel[0] == "C" else (COP0_NUM.get(sel[1], 0) if sel[0] == "R" else 0)
            return self.wr(out, f"(c.mfc0({n}) as u64)")
        if name in ("setCopReg",) and ins and ins[0][1] == 0:
            sel = ins[1]
            n = sel[1] >> 11 if sel[0] == "C" else COP0_NUM.get(sel[1], 0)
            return f"c.mtc0({n},({self.rd(ins[2])}) as u32);"
        args = ",".join(f"({self.rd(v)}) as u32" for v in ins)
        call = f'c.intrinsic("{name}",&[{args}])'
        return self.wr(out, f"({call} as u64)") if out else call + ";"

    def op(self, text, idx, own, relative):
        mnem, name, out, ins = g.parse_op(text)
        if mnem == "COPY":
            return self.wr(out, self.rd(ins[0]))
        if mnem == "LOAD":
            return self.wr(out, f"(c.ld(({self.rd(ins[0])}) as u32,{out[2]}) as u64)")
        if mnem == "STORE":
            a, v = self.rd(ins[0]), self.rd(ins[1])
            return f"{{let a=({a}) as u32;let v=({v}) as u32;c.st(a,{ins[1][2]},v);}}"
        if mnem in g.BIN:
            return self.wr(out, f"{mnem.lower()}({self.rd(ins[0])},{self.rd(ins[1])},{ins[0][2]})")
        if mnem in g.UN:
            return self.wr(out, f"{mnem.lower()}({self.rd(ins[0])},{ins[0][2]})")
        if mnem == "INT_SEXT":
            return self.wr(out, f"mask(int_sext({self.rd(ins[0])},{ins[0][2]}),{out[2]})")
        if mnem == "SUBPIECE":
            return self.wr(out, f"subpiece({self.rd(ins[0])},{ins[1][1]},{out[2]})")
        if mnem == "PIECE":
            return self.wr(out, f"piece({self.rd(ins[0])},{self.rd(ins[1])},{ins[1][2]})")
        if mnem == "BRANCH":
            return self.branch(ins[0], idx, own, relative)
        if mnem == "CBRANCH":
            return f"if {self.rd(ins[1])}!=0{{{self.branch(ins[0], idx, own, relative)}}}"
        if mnem == "BRANCHIND":
            return (f"{{let t=({self.rd(ins[0])}) as u32;let k=blk(t);"
                    f"if k!=0{{b=k;continue 'f;}}c.tail(t);return;}}")
        if mnem == "CALL":
            k, off, _ = ins[0]
            return self.call(off) if k == "M" else f"c.call(({self.rd(ins[0])}) as u32);if unsafe{{iop_core::ABORT}}{{return;}}"
        if mnem == "CALLIND":
            return f"c.call(({self.rd(ins[0])}) as u32);if unsafe{{iop_core::ABORT}}{{return;}}"
        if mnem == "RETURN":
            return "return;"
        if mnem == "CALLOTHER":
            return self.callother(name, out, ins)
        if mnem == "UNIMPLEMENTED":
            return "c.brk(0xDEAD);return;"
        if mnem.startswith("FLOAT") or mnem in ("INT2FLOAT", "TRUNC"):
            raise ValueError(f"float op {mnem} in IOP code at {self.cur:#x}")
        raise ValueError(f"unhandled p-code op {mnem} at {self.cur:#x}")

    def instruction(self, row, own):
        self.cur = int(row["addr"], 16)
        self.asm = row["asm"]
        ops = [p for p in row["pcode"].split(" | ") if p]
        uniq = sorted({int(m.group(2), 16) for m in g.VN.finditer(" | ".join(ops)) if m.group(1) == "U"})
        relative = [False]
        body = [self.op(o, i, own, relative) for i, o in enumerate(ops)]
        decl = "".join(f"let mut u{u:x}=0u64;" for u in uniq)
        if relative[0]:
            arms = "".join(f"{i}=>{{{b}i={i + 1};}}" for i, b in enumerate(body))
            return f"{{{decl}let mut i:i64=0;loop{{match i{{{arms}_=>break}}}}}}"
        return "{" + decl + "".join(body) + "}"

    def function(self, entry, name, rows):
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
            after_jump = g.ends_block(r["pcode"]) or r["delay"] == "1"
            prev_nxt = a + (8 if a in delay_of else 4)
            if r["delay"] == "1":
                prev_nxt = a + 4
        self.blk = {a: i + 1 for i, a in enumerate(lead)}
        lead_set = set(lead)
        table = ",".join(f"0x{a:x}=>{i + 1}" for i, a in enumerate(lead))
        hook = "true" if entry in self.hooks else "false"
        lines = [f"/// {name} ({len(rows)} instructions, {len(lead)} blocks)",
                 f"#[unsafe(no_mangle)]\npub fn i_{entry:08X}(c:&mut Ctx,entry:u32){{if c.enter(0x{entry:x},entry,{hook}){{return;}}",
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
            lines.append(f"// {a:08X} {asm}\n{self.instruction(r, own)}")
            last = k + 1 == len(emit) or int(emit[k + 1]["addr"], 16) in lead_set
            if last:
                lines.append(f"b={self.blk[nxt]};continue 'f;}}" if nxt in self.blk else f"c.tail(0x{nxt:x});return;}}")
        lines.append("_=>return}}}")
        return "\n".join(lines)


def generate(crates_dir, opt):
    """Generate the IOP module crates; returns (crate names, crate dirs)."""
    sheets = g.SHEETS / "iop"
    if not (sheets / "modules.csv").exists():
        return [], []
    mods = g.read(sheets / "modules.csv")
    funcs = {(r["module"], int(r["addr"], 16)): r for r in g.read(sheets / "functions.csv")}
    imports = g.read(sheets / "imports.csv")
    exports = g.read(sheets / "exports.csv")
    crates, dirs = [], []
    root = crates_dir / "recomp_iop"
    keep = {m["module"] for m in mods}
    if root.exists():
        import shutil
        for stale in root.iterdir():
            if stale.name not in keep:
                shutil.rmtree(stale)
    for mrow in mods:
        name = mrow["module"]
        instructions = g.read(sheets / "instructions" / f"{name}.csv")
        memory = g.read(sheets / "memory" / f"{name}.csv")
        by_func, _ = g.group(instructions)
        entries = sorted(by_func)
        leaders, _ = g.find_leaders(instructions, set(entries), memory)
        owner = {}
        for f, rows in by_func.items():
            for r in rows:
                owner[int(r["addr"], 16)] = f
        for a, f in owner.items():
            if owner.get(a - 4) not in (None, f):
                leaders.add(a)
        hooks = {int(r["stub"], 16) for r in imports if r["module"] == name}
        gen = GenIop(set(entries), leaders, hooks)
        crate = f"t5-iop-{name.replace('_', '-')}"
        d = root / name
        g.write_if_changed(d / "Cargo.toml", f'[package]\nname = "{crate}"\nversion = "0.1.0"\nedition = "2024"\n\n'
                                             f'[dependencies]\niop-core = {{ path = "../../iop-core" }}\n')
        with g.Out(d / "src" / "lib.rs") as fh:
            fh.write("//! GENERATED by tools/gen_iop.py from sheets/iop/instructions. Do not edit.\n"
                     f"//! IOP module: {mrow['irx_name']} ({name}) at {mrow['base']}.\n"
                     "#![allow(unused, unreachable_code, non_snake_case, unused_parens, unused_braces, clippy::all)]\n"
                     "use iop_core::{Ctx, ops::*};\n\n")
            fh.write("pub static FUNCS: &[(u32, u32, fn(&mut Ctx, u32))] = &[\n")
            for e in entries:
                for lo, hi in g.ranges(by_func[e]):
                    fh.write(f"    (0x{lo:x}, 0x{hi:x}, i_{e:08X}),\n")
            fh.write("];\n\n")
            for e in entries:
                f = funcs.get((name, e))
                fname = f["name"] if f else f"orphan_{e:08X}"
                fh.write(gen.function(e, fname, by_func[e]) + "\n\n")
        crates.append(crate)
        dirs.append(f"crates/recomp_iop/{name}")
        print(f"  {crate}: {len(entries)} functions, {len(instructions)} instructions", flush=True)
    # The module table: images are matched by IRX SHA-1 at load time.
    rs = lambda c: c.replace("-", "_")
    with g.Out(crates_dir / "t5-recomp" / "src" / "iop_table.rs") as fh:
        fh.write("//! GENERATED by tools/gen_iop.py. Do not edit.\nuse ps2_runtime::iop::IopModule;\n\n")
        fh.write("pub static IOP_MODULES: &[IopModule] = &[\n")
        for mrow, crate in zip(mods, crates):
            name = mrow["module"]
            imps = ", ".join(f'(0x{int(r["stub"], 16):08x}, "{r["library"]}", {r["index"]})' for r in imports if r["module"] == name)
            exps = ", ".join(f'("{r["library"]}", {r["index"]}, 0x{int(r["addr"], 16):08x})' for r in exports if r["module"] == name)
            fh.write(f'    IopModule {{\n        name: "{mrow["irx_name"]}",\n        sha1: "{mrow["irx_sha1"]}",\n'
                     f'        base: {mrow["base"]},\n        size: {mrow["size"]},\n        entry: {mrow["entry"]},\n'
                     f'        gp: {mrow["gp"]},\n        funcs: {rs(crate)}::FUNCS,\n        imports: &[{imps}],\n'
                     f'        exports: &[{exps}],\n    }},\n')
        fh.write("];\n")
    return crates, dirs
