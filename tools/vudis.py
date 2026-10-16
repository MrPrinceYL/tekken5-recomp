"""VU micro-code disassembler (lower and upper instructions) for runtime dumps.

    python tools/vudis.py vu1_code.bin [start_pc_hex [count]]
    python tools/vudis.py vu1_code.bin --trace headless.log   # annotate a T5_VUTRACE log

Mnemonics follow the EE Core / VU User's Manual (as PCSX2's VU disassembler).
"""
import re
import struct
import sys

DEST = lambda w: "".join(c for c, b in zip("xyzw", (21 + 3, 21 + 2, 21 + 1, 21)) if w >> b & 1)
FLD = "xyzw"
BC = "xyzw"

UPPER = {0x00: "addbc", 0x04: "subbc", 0x08: "maddbc", 0x0C: "msubbc", 0x10: "maxbc", 0x14: "minibc",
         0x18: "mulbc", 0x1C: "mulq", 0x1D: "maxi", 0x1E: "muli", 0x1F: "minii", 0x20: "addq", 0x21: "maddq",
         0x22: "addi", 0x23: "maddi", 0x24: "subq", 0x25: "msubq", 0x26: "subi", 0x27: "msubi", 0x28: "add",
         0x29: "madd", 0x2A: "mul", 0x2B: "max", 0x2C: "sub", 0x2D: "msub", 0x2E: "opmsub", 0x2F: "mini"}
UPPER2 = {0x00: "addabc", 0x04: "subabc", 0x08: "maddabc", 0x0C: "msubabc", 0x10: "itof0", 0x11: "itof4",
          0x12: "itof12", 0x13: "itof15", 0x14: "ftoi0", 0x15: "ftoi4", 0x16: "ftoi12", 0x17: "ftoi15",
          0x18: "mulabc", 0x1C: "mulaq", 0x1D: "abs", 0x1E: "mulai", 0x1F: "clipw", 0x20: "addaq", 0x21: "maddaq",
          0x22: "addai", 0x23: "maddai", 0x24: "subaq", 0x25: "msubaq", 0x26: "subai", 0x27: "msubai",
          0x28: "adda", 0x29: "madda", 0x2A: "mula", 0x2C: "suba", 0x2D: "msuba", 0x2E: "opmula", 0x2F: "nop"}


def upper(w):
    d = DEST(w)
    ft, fs, fd = w >> 16 & 31, w >> 11 & 31, w >> 6 & 31
    f = w & 0x3F
    if f < 0x3C:
        base = f & ~3 if f < 0x1C else f
        name = UPPER.get(base, f"?{f:02x}")
        if name.endswith("bc"):
            return f"{name[:-2]}{BC[f & 3]}.{d} vf{fd}, vf{fs}, vf{ft}"
        return f"{name}.{d} vf{fd}, vf{fs}, vf{ft}"
    op = (f & 3) | (w >> 6 & 31) << 2
    base = op & ~3 if op < 0x10 or 0x18 <= op < 0x1C else op
    name = UPPER2.get(base, f"?{op:02x}")
    if name == "nop":
        return "nop"
    if name.endswith("bc"):
        return f"{name[:-2]}{BC[op & 3]}.{d} ACC, vf{fs}, vf{ft}"
    return f"{name}.{d} vf{ft}, vf{fs}" if name[:4] in ("itof", "ftoi") or name == "abs" else f"{name}.{d} ACC, vf{fs}, vf{ft}"


def s11(w):
    v = w & 0x7FF
    return v - 0x800 if v & 0x400 else v


def lower(w, pc):
    d = DEST(w)
    ft, fs, fd = w >> 16 & 31, w >> 11 & 31, w >> 6 & 31
    it, is_, id_ = ft & 15, fs & 15, fd & 15
    fsf, ftf = w >> 21 & 3, w >> 23 & 3
    op = w >> 25
    tgt = lambda: f"{(pc + 1 + s11(w)) & 0x7FF:#06x}"
    imm15 = (w & 0x7FF) | (w >> 21 & 0xF) << 11
    imm12 = (w & 0x7FF) | (w >> 21 & 1) << 11
    simple = {
        0x00: f"lq.{d} vf{ft}, {s11(w)}(vi{is_})", 0x01: f"sq.{d} vf{fs}, {s11(w)}(vi{it})",
        0x04: f"ilw.{d} vi{it}, {s11(w)}(vi{is_})", 0x05: f"isw.{d} vi{it}, {s11(w)}(vi{is_})",
        0x08: f"iaddiu vi{it}, vi{is_}, {imm15:#x}", 0x09: f"isubiu vi{it}, vi{is_}, {imm15:#x}",
        0x10: f"fceq vi1, {w & 0xFFFFFF:#x}", 0x11: f"fcset {w & 0xFFFFFF:#x}", 0x12: f"fcand vi1, {w & 0xFFFFFF:#x}",
        0x13: f"fcor vi1, {w & 0xFFFFFF:#x}", 0x14: f"fseq vi{it}, {imm12:#x}", 0x15: f"fsset {imm12:#x}",
        0x16: f"fsand vi{it}, {imm12:#x}", 0x17: f"fsor vi{it}, {imm12:#x}", 0x18: f"fmeq vi{it}, vi{is_}",
        0x1A: f"fmand vi{it}, vi{is_}", 0x1B: f"fmor vi{it}, vi{is_}", 0x1C: f"fcget vi{it}",
        0x20: f"b {tgt()}", 0x21: f"bal vi{it}, {tgt()}", 0x24: f"jr vi{is_}", 0x25: f"jalr vi{it}, vi{is_}",
        0x28: f"ibeq vi{it}, vi{is_}, {tgt()}", 0x29: f"ibne vi{it}, vi{is_}, {tgt()}",
        0x2C: f"ibltz vi{is_}, {tgt()}", 0x2D: f"ibgtz vi{is_}, {tgt()}", 0x2E: f"iblez vi{is_}, {tgt()}",
        0x2F: f"ibgez vi{is_}, {tgt()}",
    }
    if op in simple:
        return simple[op]
    if op != 0x40:
        return f"?op{op:02x}"
    f = w & 0x3F
    if f == 0x30:
        return f"iadd vi{id_}, vi{is_}, vi{it}"
    if f == 0x31:
        return f"isub vi{id_}, vi{is_}, vi{it}"
    if f == 0x32:
        i5 = fd - 32 if fd & 16 else fd
        return f"iaddi vi{it}, vi{is_}, {i5}"
    if f == 0x34:
        return f"iand vi{id_}, vi{is_}, vi{it}"
    if f == 0x35:
        return f"ior vi{id_}, vi{is_}, vi{it}"
    if f < 0x3C:
        return f"?special{f:02x}"
    o = (f & 3) | (w >> 6 & 31) << 2
    tab = {
        0x30: f"move.{d} vf{ft}, vf{fs}", 0x31: f"mr32.{d} vf{ft}, vf{fs}", 0x34: f"lqi.{d} vf{ft}, (vi{is_}++)",
        0x35: f"sqi.{d} vf{fs}, (vi{it}++)", 0x36: f"lqd.{d} vf{ft}, (--vi{is_})", 0x37: f"sqd.{d} vf{fs}, (--vi{it})",
        0x38: f"div Q, vf{fs}{FLD[fsf]}, vf{ft}{FLD[ftf]}", 0x39: f"sqrt Q, vf{ft}{FLD[ftf]}",
        0x3A: f"rsqrt Q, vf{fs}{FLD[fsf]}, vf{ft}{FLD[ftf]}", 0x3B: "waitq", 0x3C: f"mtir vi{it}, vf{fs}{FLD[fsf]}",
        0x3D: f"mfir.{d} vf{ft}, vi{is_}", 0x3E: f"ilwr.{d} vi{it}, (vi{is_})", 0x3F: f"iswr.{d} vi{it}, (vi{is_})",
        0x40: f"rnext.{d} vf{ft}, R", 0x41: f"rget.{d} vf{ft}, R", 0x42: f"rinit R, vf{fs}{FLD[fsf]}",
        0x43: f"rxor R, vf{fs}{FLD[fsf]}", 0x64: f"mfp.{d} vf{ft}, P", 0x68: f"xtop vi{it}", 0x69: f"xitop vi{it}",
        0x6C: f"xgkick vi{is_}", 0x70: f"esadd P, vf{fs}", 0x71: f"ersadd P, vf{fs}", 0x72: f"eleng P, vf{fs}",
        0x73: f"erleng P, vf{fs}", 0x74: f"eatanxy P, vf{fs}", 0x75: f"eatanxz P, vf{fs}", 0x76: f"esum P, vf{fs}",
        0x78: f"esqrt P, vf{fs}{FLD[fsf]}", 0x79: f"ersqrt P, vf{fs}{FLD[fsf]}", 0x7A: f"ercpr P, vf{fs}{FLD[fsf]}",
        0x7B: "waitp", 0x7C: f"esin P, vf{fs}{FLD[fsf]}", 0x7D: f"eatan P, vf{fs}{FLD[fsf]}",
        0x7E: f"eexp P, vf{fs}{FLD[fsf]}", 0x2F: "nop", 0x0F: "nop",
    }
    return tab.get(o, f"?lower{o:02x}")


def pair(code, pc):
    lo, up = struct.unpack_from("<II", code, pc * 8)
    flags = "".join(c for c, b in zip("IEMDT", (31, 30, 29, 28, 27)) if up >> b & 1)
    lw = f"loi {lo:#010x}" if up >> 31 & 1 else ("nop" if lo == 0x8000033C else lower(lo, pc))
    return f"{pc:04x} {flags:3} {upper(up & 0x7FFFFFF):38} {lw}"


def main():
    code = open(sys.argv[1], "rb").read()
    if len(sys.argv) > 3 and sys.argv[2] == "--trace":
        for line in open(sys.argv[3], errors="replace"):
            m = re.search(r"VUT ([0-9a-f]{4}) L \S+ U \S+ vi (\S+)", line)
            if m:
                print(f"{pair(code, int(m.group(1), 16))}    | vi1..15 {m.group(2)}")
            elif "VUT start" in line:
                print(line.strip())
        return
    start = int(sys.argv[2], 16) if len(sys.argv) > 2 else 0
    count = int(sys.argv[3]) if len(sys.argv) > 3 else len(code) // 8 - start
    for pc in range(start, start + count):
        print(pair(code, pc))


if __name__ == "__main__":
    main()
