"""Unpack the code entries of TK5DATA3.BIN (Tekken 5, SLUS-21059).

Usage: python tk5data.py <image.iso> <extracted_dir>

TK5DATA3.BIN starts with a table of contents: u32 count, then count x {u32 offset, u32 size}.
Entries 0..6 are code compressed with the boot loader's LZ scheme (`lz_decompress`, a port of
SLUS 0x00D06010): 0..4 are complete ELFs, 5 and 6 are raw images that the boot code (0x00D060D0)
decompresses to 0x001F0180 and 0x00488000. Entries 7..9 are data archives.

Writes extracted/tk5data3/entry<N>.bin (decompressed), extracted/tk5data3_toc.csv, and for the
archives the stretch holding their IOP modules (the Arcade History titles load theirs from there)
as extracted/tk5data3/entry<N>_irx.bin (input of iop_modules.py).
"""
import csv
import hashlib
import os
import re
import struct
import sys

COMPRESSED = range(0, 7)


def lz_decompress(src, pos=0):
    """Port of FUN_00d06010: flag byte (bits LSB-first above a sentinel bit, 0 ends the stream,
    1 is an empty group); 1 = literal byte, 0 = big-endian u16 back-reference with an 11-bit
    distance (0 = 0x800) and a 5-bit length (0 = 32)."""
    out = bytearray()
    flag = src[pos]
    while flag != 0:
        pos += 1
        if flag >= 2:
            while flag > 1:
                if flag & 1:
                    out.append(src[pos])
                    pos += 1
                else:
                    v = src[pos] << 8 | src[pos + 1]
                    pos += 2
                    dist = v & 0x7FF or 0x800
                    length = v >> 11 or 0x20
                    start = len(out) - dist
                    for i in range(length):
                        out.append(out[start + i])
                flag >>= 1
        flag = src[pos]
    return bytes(out), pos + 1


def main():
    """Unpack TK5DATA3's code entries from the image given on the command line."""
    image, out = sys.argv[1], sys.argv[2]
    lba = None
    with open(os.path.join(out, 'disc_files.csv')) as f:
        for row in csv.DictReader(f):
            if row['path'] == 'TK5DATA3.BIN':
                lba, total = int(row['lba']), int(row['size'])
    base = lba * 2048
    os.makedirs(os.path.join(out, 'tk5data3'), exist_ok=True)
    with open(image, 'rb') as f, open(os.path.join(out, 'tk5data3_toc.csv'), 'w', newline='') as fo:
        w = csv.writer(fo)
        w.writerow(['index', 'offset', 'size', 'compressed', 'unpacked_size', 'kind', 'sha1'])
        f.seek(base)
        head = f.read(0x800)
        for i in range(struct.unpack_from('<I', head, 0)[0]):
            off, size = struct.unpack_from('<II', head, 4 + 8 * i)
            if i in COMPRESSED:
                f.seek(base + off)
                data, used = lz_decompress(f.read(size))
                if used > size:
                    sys.exit(f'entry {i}: stream overruns its {size} bytes')
                kind = 'elf' if data[:4] == b'\x7fELF' else 'raw'
                with open(os.path.join(out, 'tk5data3', f'entry{i}.bin'), 'wb') as d:
                    d.write(data)
                w.writerow([i, f'0x{off:X}', size, 1, len(data), kind, hashlib.sha1(data).hexdigest()])
                print(f'{i}: {size:#x} -> {len(data):#x} {kind}')
            else:
                w.writerow([i, f'0x{off:X}', size, 0, size, 'archive', ''])
                f.seek(base + off)
                data = f.read(size)
                irx = [m.start() for m in re.finditer(rb'\x7fELF\x01\x01\x01', data)
                       if struct.unpack_from('<H', data, m.start() + 16)[0] in (0xFF80, 0xFF81)]
                if irx:
                    # ponytail: one stretch from the first module to 256 KiB past the last
                    # (no IRX is larger); split per module if an archive ever spreads them out.
                    with open(os.path.join(out, 'tk5data3', f'entry{i}_irx.bin'), 'wb') as d:
                        d.write(data[irx[0]:irx[-1] + 0x40000])
                    print(f'{i}: {len(irx)} IOP modules')


if __name__ == '__main__':
    main()
