"""Identify Sony SDK library functions in the EE programs.

Uses the SCE symbol database shipped with PS2Recomp (ps2xAnalyzer/include/ps2recomp/
sce_symbol_database_data.h, generated from sce-symbol-scanner): a prefix tree over the first
instruction words narrows candidates, then a SHA-1 over the function bytes with relocated fields
cleared confirms them (same rules as PS2Recomp's sce_symbol_scanner.cpp).

Writes sheets/sce_symbols.csv (variant,addr,name,library,size).

    python tools/sce_symbols.py [path/to/sce_symbol_database_data.h]
"""
import csv
import hashlib
import json
import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ELF_DIR = Path(r"E:\Decompilation\Decomps\Tekken5\extracted\elf")
DB = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(
    r"E:\Decompilation\Decomps\_references\PS2Recomp\ps2xAnalyzer\include\ps2recomp\sce_symbol_database_data.h")

MASK = {None: 0xFFFFFFFF, "MIPS_26": 0xFC000000, "MIPS_HI16": 0xFFFF0000, "MIPS_LO16": 0xFFFF0000,
        "MIPS_GPREL16": 0xFFFF0000, "MIPS_LITERAL": 0xFFFF0000, "MIPS_32": 0}
STATIC_BITS = {None: 32, "MIPS_26": 6, "MIPS_HI16": 16, "MIPS_LO16": 16, "MIPS_GPREL16": 16,
               "MIPS_LITERAL": 16, "MIPS_32": 0}


def chunks(text, name):
    body = text[text.index(name):]
    body = body[:body.index("};")]
    return "".join(re.findall(r'R"PS2SDB\((.*?)\)PS2SDB"', body, re.S))


def load_db():
    text = DB.read_text(encoding="utf-8")
    return json.loads(chunks(text, "kSymbolsJsonChunks")), json.loads(chunks(text, "kTreeJsonChunks"))


def index_tree(node):
    """Group each node's edges by mask so a word is checked with one dict lookup per mask."""
    groups = {}
    for e in node.get("next", []):
        rel = e["match"].get("relocation", {}).get("type")
        mask = MASK.get(rel, 0xFFFF0000)
        groups.setdefault(mask, {}).setdefault(e["match"]["value"] & mask, []).append(e["child"])
        index_tree(e["child"])
    node["_g"] = groups


def symbol_record(symbols, key):
    by_name = symbols.get(key["library"], {}).get(key["name"], {})
    rec = by_name.get(key["hash"], {}).get(format(key["variant"], "08x"))
    return rec


def matches(code, off, rec, digest):
    size = rec["size"]
    if off + size > len(code):
        return False
    b = bytearray(code[off:off + size])
    for ro, r in rec["relocations"].items():
        ro = int(ro)
        if ro + 4 > size:
            continue
        v = struct.unpack_from("<I", b, ro)[0]
        struct.pack_into("<I", b, ro, v & MASK.get(r["type"], 0xFFFF0000))
    return hashlib.sha1(b).hexdigest() == digest


def static_bits(rec):
    reloc = sum(STATIC_BITS[None] - STATIC_BITS.get(r["type"], 16) for r in rec["relocations"].values())
    return rec["size"] * 8 - reloc


def exec_segments(path):
    d = path.read_bytes()
    phoff, = struct.unpack_from("<I", d, 0x1C)
    phentsize, phnum = struct.unpack_from("<HH", d, 0x2A)
    for i in range(phnum):
        p_type, p_off, p_vaddr, _, p_filesz, _, p_flags, _ = struct.unpack_from("<8I", d, phoff + i * phentsize)
        if p_type == 1 and p_flags & 1:
            yield p_vaddr, d[p_off:p_off + p_filesz]


def scan(code, base, tree, symbols):
    found = []
    for off in range(0, len(code) - 4, 4):
        cands = []
        stack = [tree]
        while stack:
            n = stack.pop()
            o = off + n.get("offset", 0)
            if o + 4 > len(code):
                continue
            w = struct.unpack_from("<I", code, o)[0]
            for mask, by_val in n["_g"].items():
                for child in by_val.get(w & mask, ()):
                    cands.extend(child.get("symbols", ()))
                    if child.get("next"):
                        stack.append(child)
        ids = {}
        for key in cands:
            rec = symbol_record(symbols, key)
            if rec and static_bits(rec) >= 256 and matches(code, off, rec, key["hash"]):
                ids.setdefault((rec["library"], key["name"]), rec["size"])
        if len(ids) == 1:
            (lib, name), size = ids.popitem()
            found.append((base + off, name, lib, size))
    return found


def main():
    symbols, tree = load_db()
    index_tree(tree)
    rows = []
    for elf in sorted(ELF_DIR.glob("*.elf")):
        for base, code in exec_segments(elf):
            for addr, name, lib, size in scan(code, base, tree, symbols):
                rows.append((elf.stem, f"0x{addr:08X}", name, lib, size))
        print(elf.stem, sum(1 for r in rows if r[0] == elf.stem))
    with open(ROOT / "sheets" / "sce_symbols.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["variant", "addr", "name", "library", "size"])
        w.writerows(rows)


if __name__ == "__main__":
    main()
