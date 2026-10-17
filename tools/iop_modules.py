"""Find the IOP modules (IRX) the game's programs carry, relocate each to a fixed IOP address and
write an ELF per module for Ghidra, plus their import / export tables.

Usage: python iop_modules.py <extracted_dir>

IRX: an ELF of type 0xFF80 / 0xFF81 linked at 0 with REL relocations (R_MIPS_32, R_MIPS_26,
R_MIPS_HI16, R_MIPS_LO16) and an .iopmod section { moduleinfo, entry, gp, text size, data size,
bss size, u16 version, name }. Modules are placed at 64 KiB-aligned addresses (the runtime's
loader uses the same table), so LO16 halves never change and only HI16 / 26 / 32 fields are
adjusted.

Import tables (in .text): 0x41E00000, 0, version, name[8], then pairs { jr ra; addiu zero,zero,n }
(stub of function n), ended by 0. Export tables: 0x41C00000, 0, version, name[8], function
addresses, ended by 0.

Writes extracted/iop/<module>.elf and extracted/iop_modules.csv, extracted/iop_imports.csv,
extracted/iop_exports.csv.
"""
import csv
import hashlib
import os
import re
import struct
import sys

# entry 6: the main game; IRXARC.BIN: Devil Within (other builds of NU_File_Manager etc.);
# archive entry 7 (tk5data.py): the Arcade History titles.
SOURCES = ["tk5data3/entry6.bin", "files/IRXARC.BIN", "tk5data3/entry7_irx.bin"]
# Sources whose modules go to the upper area: the main game never loads them, so its IOP heap
# (which starts after the modules below 0x100000, see `heap_start` in iop/exec.rs) stays as is.
UPPER = {"tk5data3/entry7_irx.bin"}
# Sony drivers whose services the runtime emulates at the RPC / SIF level: not recompiled.
EMULATED = {"mcman", "mcserv", "sio2man", "sio2d", "padman", "ds2u_d", "Dbc_Manager"}
FIRST_BASE = 0x0002_0000
SLOT = 0x1_0000
LAST = 0x0010_0000
UPPER_BASE = 0x0010_0000
UPPER_LAST = 0x001C_0000


def u32(b, o):
    return struct.unpack_from("<I", b, o)[0]


def u16(b, o):
    return struct.unpack_from("<H", b, o)[0]


def parse(img):
    """Sections and module info of an IRX image."""
    shoff, shnum, shstr = u32(img, 32), u16(img, 48), u16(img, 50)
    sh = [struct.unpack_from("<10I", img, shoff + 40 * i) for i in range(shnum)]
    so = sh[shstr][4]
    secs = []
    for s in sh:
        name = img[so + s[0]:img.index(b"\0", so + s[0])].decode()
        secs.append(dict(name=name, type=s[1], addr=s[3], off=s[4], size=s[5], info=s[7]))
    iop = next(s for s in secs if s["type"] == 0x70000080)
    d = img[iop["off"]:iop["off"] + iop["size"]]
    info = dict(moduleinfo=u32(d, 0), entry=u32(d, 4), gp=u32(d, 8), text=u32(d, 12), data=u32(d, 16),
                bss=u32(d, 20), version=u16(d, 24), name=d[26:].split(b"\0")[0].decode())
    return secs, info


def image_size(img):
    """Bytes of the ELF file: the end of the section header table or of the last section."""
    shoff, shnum = u32(img, 32), u16(img, 48)
    end = shoff + 40 * shnum
    for i in range(shnum):
        s = struct.unpack_from("<10I", img, shoff + 40 * i)
        if s[1] != 8:
            end = max(end, s[4] + s[5])
    return end


def relocate(img, secs, base):
    """The loaded image (text + data, bss zeroed) at `base`."""
    alloc = [s for s in secs if s["addr"] or s["name"] == ".text"]
    alloc = [s for s in alloc if s["type"] in (1, 8) and s["name"] not in ("", ".shstrtab")]
    end = max(s["addr"] + s["size"] for s in alloc)
    mem = bytearray(end)
    for s in alloc:
        if s["type"] == 1:
            mem[s["addr"]:s["addr"] + s["size"]] = img[s["off"]:s["off"] + s["size"]]
    for s in secs:
        if s["type"] != 9:
            continue
        for i in range(0, s["size"], 8):
            off, inf = struct.unpack_from("<II", img, s["off"] + i)
            t = inf & 0xFF
            if off + 4 > len(mem):
                continue
            w = u32(mem, off)
            if t == 2:
                w = (w + base) & 0xFFFFFFFF
            elif t == 4:
                target = ((w & 0x3FFFFFF) << 2) + base
                w = (w & 0xFC000000) | ((target >> 2) & 0x3FFFFFF)
            elif t == 5:
                w = (w & 0xFFFF0000) | (((w & 0xFFFF) + (base >> 16)) & 0xFFFF)
            elif t == 6:
                pass  # base is 64 KiB-aligned
            elif t == 0:
                continue
            else:
                raise SystemExit(f"relocation type {t}")
            struct.pack_into("<I", mem, off, w)
    return bytes(mem)


def tables(mem, base):
    """Import stubs (address, library, index) and export tables (library, index, address)."""
    imports, exports = [], []
    for o in range(0, len(mem) - 24, 4):
        w = u32(mem, o)
        if w == 0x41E00000 and u32(mem, o + 4) == 0:
            lib = mem[o + 12:o + 20].split(b"\0")[0].decode("latin1")
            p = o + 20
            while p + 8 <= len(mem):
                a, b = u32(mem, p), u32(mem, p + 4)
                if a != 0x03E00008 or b >> 16 != 0x2400:
                    break
                imports.append((base + p, lib, b & 0xFFFF))
                p += 8
        if w == 0x41C00000 and u32(mem, o + 4) == 0:
            lib = mem[o + 12:o + 20].split(b"\0")[0].decode("latin1")
            p = o + 20
            n = 0
            while p + 4 <= len(mem):
                a = u32(mem, p)
                if a == 0:
                    break
                exports.append((lib, n, a))
                n += 1
                p += 4
    return imports, exports


def write_elf(path, base, mem, entry, gp):
    """A plain MIPS32 LE executable with the image as one RWX section at `base`."""
    names = b"\0.text\0.shstrtab\0"
    data_off = 52 + 32
    sh_off = (data_off + len(mem) + len(names) + 3) & ~3
    head = struct.pack("<4sBBBB8sHHIIIIIHHHHHH", b"\x7fELF", 1, 1, 1, 0, b"\0" * 8, 2, 8, 1, base + entry,
                       52, sh_off, 0x1000, 52, 32, 1, 40, 3, 2)
    ph = struct.pack("<8I", 1, data_off, base, base, len(mem), len(mem), 7, 16)
    body = mem + names
    pad = b"\0" * (sh_off - data_off - len(body))
    sh = b"\0" * 40
    sh += struct.pack("<10I", 1, 1, 7, base, data_off, len(mem), 0, 0, 16, 0)
    sh += struct.pack("<10I", 7, 3, 0, 0, data_off + len(mem), len(names), 0, 0, 1, 0)
    with open(path, "wb") as f:
        f.write(head + ph + body + pad + sh)
    _ = gp


def main():
    ex = sys.argv[1]
    found = {}
    for src in SOURCES:
        with open(os.path.join(ex, src), "rb") as f:
            blob = f.read()
        for m in re.finditer(rb"\x7fELF\x01\x01\x01", blob):
            o = m.start()
            if u16(blob, o + 16) not in (0xFF80, 0xFF81):
                continue
            img = blob[o:o + image_size(blob[o:])]
            sha = hashlib.sha1(img).hexdigest()
            if sha in found:
                continue
            secs, info = parse(img)
            if info["name"] in EMULATED:
                continue
            found[sha] = dict(img=img, secs=secs, info=info, src=f"{src}+{o:#x}", order=(SOURCES.index(src), o))
    os.makedirs(os.path.join(ex, "iop"), exist_ok=True)
    mods = sorted(found.items(), key=lambda kv: kv[1]["order"])
    base = FIRST_BASE
    rows, imps, exps = [], [], []
    for sha, md in mods:
        upper = md["src"].split("+")[0] in UPPER
        if upper:
            base = max(base, UPPER_BASE)
        info = md["info"]
        tag = re.sub(r"[^A-Za-z0-9]+", "_", info["name"]).strip("_").lower()
        name = f"{tag}_{sha[:6]}"
        probe = relocate(md["img"], md["secs"], 0)
        size = len(probe) + info["bss"]
        slots = (size + SLOT - 1) // SLOT
        if base + slots * SLOT > (UPPER_LAST if upper else LAST):
            raise SystemExit("IOP module area full")
        mem = relocate(md["img"], md["secs"], base)
        write_elf(os.path.join(ex, "iop", f"{name}.elf"), base, mem, info["entry"], info["gp"])
        im, xp = tables(mem, base)
        rows.append(dict(module=name, irx_name=info["name"], version=f"{info['version']:#06x}", source=md["src"],
                         irx_sha1=sha, irx_size=len(md["img"]), base=f"0x{base:08X}", size=f"0x{size:X}",
                         entry=f"0x{base + info['entry']:08X}", gp=f"0x{base + info['gp']:08X}",
                         elf=f"iop/{name}.elf"))
        imps += [dict(module=name, stub=f"0x{a:08X}", library=l, index=i) for a, l, i in im]
        exps += [dict(module=name, library=l, index=i, addr=f"0x{a:08X}") for l, i, a in xp]
        print(f"{name:40s} {md['src']:28s} base {base:#x} size {size:#x} imports {len(im)} exports {len(xp)}")
        base += slots * SLOT
    for fname, data, cols in [("iop_modules.csv", rows, list(rows[0])),
                              ("iop_imports.csv", imps, ["module", "stub", "library", "index"]),
                              ("iop_exports.csv", exps, ["module", "library", "index", "addr"])]:
        with open(os.path.join(ex, fname), "w", newline="") as f:
            w = csv.DictWriter(f, cols)
            w.writeheader()
            w.writerows(data)


if __name__ == "__main__":
    main()
