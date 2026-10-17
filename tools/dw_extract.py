"""Devil Within's data archive TK5DATA2.BIN ("cd:tk5data2.bin", plain, not compressed).

The program carries the directory itself: 0x146 entries of 16 bytes at 0x362900 in devil.elf
{name checksum, name offset into the pool at 0x363D60, offset, size} (lookup 0x20EEB0, count
0x20EEA8). Writes every file to extracted/tk5data2/<path> and the list to
sheets/dw_files.csv (path, offset, size).

    python tools/dw_extract.py "E:/Decompilation/Game/Tekken 5 (USA).iso" E:/Decompilation/Decomps/Tekken5/extracted
"""
import csv
import os
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIR, POOL, COUNT = 0x362900, 0x363D60, 0x146


def main(image, extracted):
    elf = open(os.path.join(extracted, "elf", "devil.elf"), "rb").read()
    phoff, = struct.unpack_from("<I", elf, 0x1C)
    phnum, = struct.unpack_from("<H", elf, 0x2C)
    segs = [struct.unpack_from("<6I", elf, phoff + i * 32) for i in range(phnum)]

    def off(va):
        return next(o + va - v for _, o, v, _, fs, _ in segs if fs and v <= va < v + fs)

    def cstr(va):
        o = off(va)
        return elf[o:elf.index(b"\0", o)].decode("latin1")

    lba = next(int(r["lba"]) for r in csv.DictReader(open(os.path.join(extracted, "disc_files.csv"), newline=""))
               if r["path"] == "TK5DATA2.BIN")
    rows = []
    with open(image, "rb") as iso:
        for i in range(COUNT):
            _, name_off, offset, size = struct.unpack_from("<4I", elf, off(DIR + i * 16))
            name = cstr(POOL + name_off)
            if not name:
                continue
            rows.append((name, offset, size))
            dst = os.path.join(extracted, "tk5data2", *name.split("/"))
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            iso.seek(lba * 2048 + offset)
            with open(dst, "wb") as f:
                f.write(iso.read(size))
    with open(ROOT / "sheets" / "dw_files.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["path", "offset", "size"])
        for name, offset, size in rows:
            w.writerow([name, f"{offset:#x}", size])
    print(f"{len(rows)} files -> {os.path.join(extracted, 'tk5data2')}, sheets/dw_files.csv")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
