"""VU micro code: sheet and Rust translation.

The game uploads its VU1 / VU0 micro programs at run time (VIF MPG). `t5-headless` with
T5_VUDUMP=1 writes every distinct micro memory image it runs to run/vu_dumps/. This tool

  --collect-elf VARIANT  the same statically from the program's .vutext section (all of
                  its VU1 micro code, blocks and entry shapes; see collect_elf);
  --collect DIR   merges the instruction pairs of those images into sheets/vu_microcode.csv
                  (vu, upper, lower, disassembly), the source of truth for the translation;
  (default)       generates crates/t5-vu from the sheet: every pair becomes a specialised Rust
                  function (ps2_runtime::vu::pair_const::<LOWER, UPPER>, whose decoding the
                  compiler folds away), in a table the runtime looks pairs up in.

    python tools/gen_vu.py --collect ../Tekken5/run/vu_dumps
    python tools/gen_vu.py
"""
import csv
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SHEET = ROOT / "sheets" / "vu_microcode.csv"
BLOCKS = ROOT / "sheets" / "vu_blocks.csv"
BCOLS = ["vu", "pc", "len", "key", "pairs", "mac_live", "shapes"]
CRATE = ROOT / "rust" / "crates" / "t5-vu"
# Crates the translated blocks are spread over (built in parallel). Each crate needs about
# 12 MB of compiler memory per block at opt-level 3: 16 crates of ~250 blocks (3 GB each,
# 16 at once on 16 cores) ran 32 GB PCs out of memory (17.10.). Keep blocks / PARTS <= ~100
# (more blocks in the sheet -> raise PARTS, then run gen_rust.py: it lists the crates).
PARTS = 48
COLS = ["vu", "upper", "lower", "disasm"]

sys.path.insert(0, str(Path(__file__).resolve().parent))
import vudis  # noqa: E402
import vuc  # noqa: E402


def read_sheet():
    if not SHEET.exists():
        return {}
    with open(SHEET, newline="") as f:
        return {(r["vu"], r["upper"], r["lower"]): r for r in csv.DictReader(f)}


def fnv(words):
    h = 0xcbf29ce484222325
    for w in words:
        for b in struct.pack("<I", w):
            h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def pair_at(code, j):
    return struct.unpack_from("<II", code, j * 8)


def is_branch(lo, up):
    return up >> 31 == 0 and (lo >> 25) in (0x20, 0x21, 0x24, 0x25, 0x28, 0x29, 0x2C, 0x2D, 0x2E, 0x2F)


def block_len(code, pc):
    """As ps2_runtime::vu::block_len."""
    n = len(code) // 8
    j = pc
    while j < n and j - pc < 256:
        lo, up = pair_at(code, j)
        if up & 0x40000000 or is_branch(lo, up):
            return j + 2 - pc if j + 1 < n else None
        j += 1
    return None


def backup_writer(lo, up):
    """As ps2_runtime::vu::backup_reg: the pair's lower op writes a VI register a following
    branch reads the old value of."""
    if up >> 31:
        return False
    it, is_, id_ = (lo >> 16) & 15, (lo >> 11) & 15, (lo >> 6) & 15
    op = lo >> 25
    if op in (0x08, 0x09):
        return it != 0
    if op == 0x40:
        f = lo & 0x3F
        if f in (0x30, 0x31, 0x34, 0x35):
            return id_ != 0
        if f == 0x32:
            return it != 0
        if f >= 0x3C:
            return ((f & 3) | ((lo >> 6) & 31) << 2) in (0x3C, 0x34, 0x36, 0x35, 0x37)
    return False


def upper_sets_mac(w):
    """As ps2_runtime::vu::upper_sets_mac."""
    f = w & 0x3F
    if f < 0x3C:
        return not (0x10 <= f <= 0x17 or f in (0x1D, 0x1F, 0x2B, 0x2F) or 0x30 <= f <= 0x3B)
    op = (f & 3) | ((w >> 6) & 31) << 2
    return op <= 0x0F or 0x18 <= op <= 0x1C or op == 0x1E or 0x20 <= op <= 0x2A or 0x2C <= op <= 0x2E


def is_mac_reader(lo, up):
    return up >> 31 == 0 and (lo >> 25) in (0x18, 0x1A, 0x1B)


def mac_live(code, pc):
    """As ps2_runtime::vu::mac_live."""
    n = len(code) // 8
    if not upper_sets_mac(pair_at(code, pc)[1]):
        return False
    c = end = None
    j = pc + 1
    while True:
        if j >= n:
            return True
        lo, up = pair_at(code, j)
        if is_mac_reader(lo, up) and (c is None or j <= c + 3):
            return True
        if c is not None and j >= c + 3:
            return False
        if c is None and upper_sets_mac(up):
            c = j
        if end == j:
            return True
        if end is None and (up & 0x40000000 or is_branch(lo, up)):
            end = j + 1
        j += 1


def read_blocks():
    """Rows by key. The key is recomputed from the content (as ps2_runtime::vu::block_key: VU
    number + words, not the address): the same block loaded at another pc is one row, its entry
    shapes merged; `pc` is where it was first seen."""
    if not BLOCKS.exists():
        return {}
    rows = {}
    with open(BLOCKS, newline="") as f:
        for r in csv.DictReader(f):
            r["key"] = f"{block_key(int(r['vu']), r['pairs']):016x}"
            old = rows.get(r["key"])
            if old is None:
                rows[r["key"]] = r
                continue
            have = [x for x in (old.get("shapes") or "").split("|") if x]
            for sh in (r.get("shapes") or "").split("|"):
                if sh and sh not in have:
                    have.append(sh)
            old["shapes"] = "|".join(have)
    return rows


def block_key(vu, pairs):
    """Key of a block from its sheet text ("upper.lower ...")."""
    words = []
    for pr in pairs.split():
        up, lo = (int(x, 16) for x in pr.split("."))
        words += [lo, up]
    return fnv([vu] + words)


def collect_blocks(dump_dir):
    """Block starts logged by the runtime (starts.txt: image, pc) -> sheets/vu_blocks.csv."""
    starts = Path(dump_dir) / "starts.txt"
    rows = read_blocks()
    before = len(rows)
    if starts.exists():
        seen = set()
        for line in starts.read_text().splitlines():
            img, pc = line.split()
            if (img, pc) in seen:
                continue
            seen.add((img, pc))
            f = Path(dump_dir) / f"{img}.bin"
            if not f.exists():
                continue
            code = f.read_bytes()
            pc = int(pc, 16)
            n = block_len(code, pc)
            if n is None:
                continue
            words = list(struct.unpack_from(f"<{2 * n}I", code, pc * 8))
            key = f"{fnv([int(img[2])] + words):016x}"
            if key in rows:
                continue
            pairs = " ".join(f"{words[2 * i + 1]:08x}.{words[2 * i]:08x}" for i in range(n))
            live = "".join("1" if mac_live(code, pc + i) else "0" for i in range(n))
            rows[key] = {"vu": img[2], "pc": f"{pc:x}", "len": n, "key": key, "pairs": pairs, "mac_live": live, "shapes": ""}
    with open(BLOCKS, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=BCOLS)
        w.writeheader()
        for k in sorted(rows):
            w.writerow(rows[k])
    print(f"{len(rows)} blocks in {BLOCKS.name} ({len(rows) - before} new)")


def collect_shapes(path):
    """Entry shapes seen per block (vu-bench VUBENCH_SHAPES / the runtime: lines
    "block-key shape-key shape") -> sheets/vu_blocks.csv column `shapes` ("|"-separated)."""
    rows = read_blocks()
    added = 0
    for line in Path(path).read_text().splitlines():
        parts = line.split(" ", 2)
        if len(parts) < 2:
            continue
        bkey, skey = parts[0], parts[1]
        shape = parts[2] if len(parts) > 2 else ""
        if bkey not in rows:
            continue
        if f"{vuc.shape_key(shape):016x}" != skey:
            raise SystemExit(f"shape key mismatch for {shape!r}: {skey}")
        # An empty shape (nothing in flight) is written "-".
        have = [x for x in (rows[bkey].get("shapes") or "").split("|") if x]
        mark = shape if shape else "-"
        if mark not in have:
            have.append(mark)
            rows[bkey]["shapes"] = "|".join(have)
            added += 1
    with open(BLOCKS, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=BCOLS)
        w.writeheader()
        for k in sorted(rows):
            w.writerow({c: rows[k].get(c, "") for c in BCOLS})
    print(f"{added} entry shapes added to {BLOCKS.name}")


def collect(dump_dir):
    rows = read_sheet()
    before = len(rows)
    for f in sorted(Path(dump_dir).glob("vu*_*.bin")):
        vu = f.name[2]
        code = f.read_bytes()
        for pc in range(len(code) // 8):
            lo, up = struct.unpack_from("<II", code, pc * 8)
            key = (vu, f"{up:08x}", f"{lo:08x}")
            if key not in rows:
                rows[key] = {"vu": vu, "upper": key[1], "lower": key[2], "disasm": vudis.pair(code, pc)[5:].strip()}
    SHEET.parent.mkdir(parents=True, exist_ok=True)
    with open(SHEET, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=COLS)
        w.writeheader()
        for k in sorted(rows):
            w.writerow(rows[k])
    print(f"{len(rows)} pairs in {SHEET.name} ({len(rows) - before} new)")
    collect_blocks(dump_dir)


def write_blocks(rows):
    with open(BLOCKS, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=BCOLS)
        w.writeheader()
        for k in sorted(rows):
            w.writerow({c: rows[k].get(c, "") for c in BCOLS})


def collect_elf(variant):
    """VU1 micro code straight from a program's `.vutext` section (no run needed): every pair
    into sheets/vu_microcode.csv, every block (linear sweep + static branch targets) into
    sheets/vu_blocks.csv, and entry shapes followed statically: a program starts with an empty
    pipeline ("-" after the section start and after every E-bit block), each translated block
    hands its exit shape to its fall-through and static branch successors. Blocks are keyed by
    content, so where the game loads them later does not matter."""
    sec = next(r for r in csv.DictReader(open(ROOT / "sheets" / "sections.csv", newline=""))
               if r["variant"] == variant and r["name"] == ".vutext")
    start, end = int(sec["start"], 16), int(sec["end"], 16) + 1
    elf = (ROOT.parent / "Tekken5" / "extracted" / "elf" / f"{variant}.elf").read_bytes()
    phoff, = struct.unpack_from("<I", elf, 0x1C)
    phnum, = struct.unpack_from("<H", elf, 0x2C)
    code = None
    for i in range(phnum):
        _, off, va, _, fsz, _ = struct.unpack_from("<6I", elf, phoff + i * 32)
        if va <= start and end <= va + fsz:
            code = elf[off + start - va:off + end - va]
    code = code[:len(code) // 8 * 8]
    n = len(code) // 8
    # Pairs.
    rows = read_sheet()
    before = len(rows)
    for pc in range(n):
        lo, up = struct.unpack_from("<II", code, pc * 8)
        key = ("1", f"{up:08x}", f"{lo:08x}")
        if key not in rows:
            rows[key] = {"vu": "1", "upper": key[1], "lower": key[2], "disasm": vudis.pair(code, pc)[5:].strip()}
    with open(SHEET, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=COLS)
        w.writeheader()
        for k in sorted(rows):
            w.writerow(rows[k])
    print(f"{variant} .vutext: {n} pairs, {len(rows) - before} new in {SHEET.name}")
    # Blocks: start of the section, after every block, static branch targets.
    blocks = {}
    entry = set()
    work = [0]
    while work:
        pc = work.pop()
        if pc in blocks or not 0 <= pc < n:
            continue
        ln = block_len(code, pc)
        if ln is None:
            continue
        blocks[pc] = ln
        lo, up = pair_at(code, pc + ln - 2)
        work.append(pc + ln)
        if up & 0x40000000:
            entry.add(pc + ln)
        if is_branch(lo, up) and (lo >> 25) not in (0x24, 0x25):
            work.append(pc + ln - 1 + (((lo & 0x7FF) ^ 0x400) - 0x400))
    entry.add(0)
    brows = read_blocks()
    bbefore = len(brows)
    info = {}
    for pc, ln in blocks.items():
        words = list(struct.unpack_from(f"<{2 * ln}I", code, pc * 8))
        key = f"{fnv([1] + words):016x}"
        prs = [(words[2 * i + 1], words[2 * i]) for i in range(ln)]
        live = "".join("1" if mac_live(code, pc + i) else "0" for i in range(ln))
        if key not in brows:
            brows[key] = {"vu": "1", "pc": f"{pc:x}", "len": ln, "key": key,
                          "pairs": " ".join(f"{u:08x}.{l:08x}" for u, l in prs), "mac_live": live, "shapes": ""}
        info[pc] = (key, prs, live)
    # Entry shapes, followed to a fixed point (at most MAX_SHAPES per block).
    MAX_SHAPES = 4
    seen = set()
    work = [(pc, "") for pc in entry if pc in info]
    for pc, (key, _, _) in info.items():
        work += [(pc, "" if sh == "-" else sh) for sh in (brows[key].get("shapes") or "").split("|") if sh]
    added = 0
    while work:
        pc, sh = work.pop()
        if (pc, sh) in seen or pc not in info:
            continue
        key, prs, live = info[pc]
        have = [x for x in (brows[key].get("shapes") or "").split("|") if x]
        mark = sh if sh else "-"
        if mark not in have:
            if len(have) >= MAX_SHAPES:
                continue
            have.append(mark)
            brows[key]["shapes"] = "|".join(have)
            added += 1
        seen.add((pc, sh))
        out = vuc.exit_shape(pc, prs, live, sh)
        if out is None:
            continue
        ln = len(prs)
        up, lo = prs[-2]
        if up & 0x40000000:
            continue
        if not (is_branch(lo, up) and (lo >> 25) in (0x20, 0x21, 0x24, 0x25)):
            work.append((pc + ln, out))
        if is_branch(lo, up) and (lo >> 25) not in (0x24, 0x25):
            work.append((pc + ln - 1 + (((lo & 0x7FF) ^ 0x400) - 0x400), out))
    write_blocks(brows)
    print(f"{len(blocks)} blocks in .vutext, {len(brows) - bbefore} new in {BLOCKS.name}, {added} entry shapes added")


def write_if_changed(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    # CRLF on every platform (as written on Windows), so Linux and Windows output is identical.
    data = text.replace("\n", "\r\n").encode()
    if not path.exists() or path.read_bytes() != data:
        path.write_bytes(data)


def generate():
    rows = read_sheet()
    # Both VUs share the pair semantics; one table, keyed by the instruction words.
    keys = sorted({(int(u, 16) << 32 | int(l, 16)) for (_, u, l) in rows})
    write_if_changed(CRATE / "Cargo.toml",
                     '[package]\nname = "t5-vu"\nversion = "0.1.0"\nedition = "2024"\n\n'
                     '[dependencies]\nps2-runtime = { path = "../ps2-runtime" }\n')
    out = ["//! GENERATED by tools/gen_vu.py from sheets/vu_microcode.csv. Do not edit.",
           "//! The game's VU micro code, translated pair by pair (see ps2_runtime::vu::pair_const).",
           "use ps2_runtime::vu::{pair_const, pair_const_nf, PairFn, PairFns};", "",
           f"pub static PAIRS: [PairFns; {len(keys)}] = ["]
    for k in keys:
        lo, up = f"0x{k & 0xFFFFFFFF:08x}", f"0x{k >> 32:08x}"
        out.append(f"    (0x{k:016x}, pair_const::<{lo}, {up}> as PairFn, pair_const_nf::<{lo}, {up}> as PairFn),")
    out += ["];", ""]
    # Blocks: one function each with the pairs inline (ps2_runtime::vu::pair_lean): the flag
    # variant each pair's liveness needs, control flow only in the branch / E-bit pair and its
    # delay slot, the VI backup only near its writers and branches. Spread over PARTS crates so
    # the compiler builds them in parallel.
    blocks = sorted(read_blocks().values(), key=lambda r: r["key"])
    parts = [blocks[i::PARTS] for i in range(PARTS)]
    names = [f"t5-vu-{i:02}" for i in range(PARTS)]
    for name, part in zip(names, parts):
        po = ["//! GENERATED by tools/gen_vu.py from sheets/vu_blocks.csv. Do not edit.",
              "//! Translated VU micro code blocks (see ps2_runtime::vu::pair_lean).",
              "#![allow(unused, non_snake_case, clippy::all)]",
              "use ps2_runtime::vu::{blk_enter, blk_one, blk_parts, pair_lean, Ctl, PairFn, Pipe, Run, VuRegs};",
              "use ps2_runtime::vuc;",
              "use ps2_runtime::Machine;", ""]
        for b in part:
            pc = int(b["pc"], 16)
            vu = int(b["vu"])
            prs = [tuple(int(x, 16) for x in pr.split(".")) for pr in b["pairs"].split()]  # (up, lo)
            n = len(prs)
            wr = [backup_writer(lo, up) for (up, lo) in prs]
            key = b["key"]
            # Entry: the run must be at the block start with no branch pending; then the kernel,
            # whose reference parameters do not alias (registers stay in host registers).
            po.append("#[inline(never)]")
            po.append(f"fn b_{key}(r: &mut Run) -> bool {{")
            po.append(f"    if !blk_enter(r, {n}) {{ return blk_one(r); }}")
            po.append("    let (regs, data, p, c, mp) = blk_parts(r);")
            po.append("    let pc0 = c.pc;")
            # Statically translated variants per entry shape (tools/vuc.py), else pair by pair.
            variants = []
            if vu == 1 and b.get("shapes"):
                for sh in b["shapes"].split("|"):
                    sh = "" if sh == "-" else sh
                    src = vuc.compile_block(f"s_{key}_{len(variants)}", pc, prs, b["mac_live"], sh)
                    if src is not None:
                        variants.append((vuc.shape_key(sh), src))
            if variants:
                po.append("    let key = if p.key_ok { p.key } else { p.canon(regs); p.shape_key() };")
                po.append("    match key {")
                for j, (sk, _) in enumerate(variants):
                    po.append(f"        0x{sk:016x} => s_{key}_{j}(regs, data, p, c, mp),")
                po.append(f"        _ => {{ p.key_ok = false; k_{key}(pc0, regs, data, p, c, mp) }}")
                po.append("    }")
            else:
                po.append("    p.key_ok = false;")
                po.append(f"    k_{key}(pc0, regs, data, p, c, mp)")
            po.append("}")
            for _, src in variants:
                po.append(src)
            po.append("#[inline(never)]")
            po.append(f"fn k_{key}(pc0: u32, regs: &mut VuRegs, data: *mut u8, p: &mut Pipe, c: &mut Ctl, mp: *mut Machine) -> bool {{")
            icount = 0x800 if vu == 1 else 0x200
            for i, ((up, lo), live) in enumerate(zip(prs, b["mac_live"])):
                ctrl = i >= n - 2
                vib = i < 2 or wr[i] or (i >= 1 and wr[i - 1]) or (i >= 2 and wr[i - 2]) or is_branch(lo, up)
                args = (f"0x{lo:08x}, 0x{up:08x}, {'true' if live == '1' else 'false'}, "
                        f"{'true' if ctrl else 'false'}, {'true' if vib else 'false'}, {vu}")
                at = f"(pc0 + {i}) % {icount}"
                call = f"pair_lean::<{args}>({at}, regs, data, p, c, mp)"
                if not ctrl:
                    po.append(f"    {call};")
                else:
                    if i == n - 2:
                        po.append(f"    c.pc = {at};")
                    else:
                        po.append(f"    if c.pc != {at} {{ return false; }}")
                    po.append(f"    if {call} {{ return true; }}")
            po.append("    false")
            po.append("}")
        po.append(f"pub static BLOCKS: [(u64, PairFn); {len(part)}] = [")
        for b in part:
            po.append(f"    (0x{b['key']}, b_{b['key']} as PairFn),")
        po += ["];", ""]
        d = ROOT / "rust" / "crates" / name
        write_if_changed(d / "Cargo.toml",
                         f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n\n'
                         '[dependencies]\nps2-runtime = { path = "../ps2-runtime" }\n')
        write_if_changed(d / "src" / "lib.rs", "\n".join(po))
    write_if_changed(CRATE / "Cargo.toml",
                     '[package]\nname = "t5-vu"\nversion = "0.1.0"\nedition = "2024"\n\n'
                     '[dependencies]\nps2-runtime = { path = "../ps2-runtime" }\n'
                     + "".join(f'{nm} = {{ path = "../{nm}" }}\n' for nm in names))
    tables = ", ".join(f"&{nm.replace('-', '_')}::BLOCKS[..]" for nm in names)
    out += ["/// Hand the tables to the runtime.", "pub fn register() {",
            "    ps2_runtime::vu::set_pair_table(&PAIRS);",
            f"    ps2_runtime::vu::set_block_tables(&[{tables}]);", "}", ""]
    write_if_changed(CRATE / "src" / "lib.rs", "\n".join(out))
    print(f"t5-vu: {len(keys)} pairs, {len(blocks)} blocks in {PARTS} crates")
    return ["t5-vu"] + names, ["crates/t5-vu"] + [f"crates/{nm}" for nm in names]


if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--collect":
        collect(sys.argv[2])
    elif len(sys.argv) > 2 and sys.argv[1] == "--collect-elf":
        collect_elf(sys.argv[2])
    elif len(sys.argv) > 2 and sys.argv[1] == "--collect-shapes":
        collect_shapes(sys.argv[2])
    else:
        generate()
