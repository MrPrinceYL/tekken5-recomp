"""Write every TIM2 picture of a Devil Within texture bundle (BDPK entry type 2, `nut_*`) as PNG.

    python tools/tim2png.py <pack.pak> <entry name> <out dir>

TIM2: 16-byte file header (+6 picture count), per picture {+0 total size, +4 CLUT size, +8
image size, +0xC header size, +0xE CLUT colours, +0x12 CLUT type (1/2/3 = 16/24/32-bit), +0x13 image type (3 = 32-bit, 4 = 4-bit, 5 =
8-bit indexed), +0x14 width, +0x16 height}; 8-bit CLUTs in CSM1 order (entries 8-15 and 16-23 of
every 32 swapped).
"""
import os
import struct
import sys
import zlib


def png(path, w, h, rgba):
    raw = b''.join(b'\0' + rgba[y * w * 4:(y + 1) * w * 4] for y in range(h))
    chunk = lambda t, d: struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xFFFFFFFF)
    open(path, 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0))
                           + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))


def entry(pak, name):
    d = open(pak, 'rb').read()
    n, = struct.unpack_from('<I', d, 4)
    for i in range(n):
        e = d[0x80 * (i + 1):0x80 * (i + 2)]
        if e[0x14:0x80].split(b'\0')[0].decode() == name:
            size, off = struct.unpack_from('<I', e, 8)[0], struct.unpack_from('<I', e, 0x10)[0]
            return d[off:off + size]
    raise SystemExit(f'{name} not in {pak}')


def main(pak, name, out):
    b = entry(pak, name)
    os.makedirs(out, exist_ok=True)
    at = 0x80 if b[5] == 1 else 0x10
    for k in range(struct.unpack_from('<H', b, 6)[0]):
        total, clut, img, hsize, colors = struct.unpack_from('<IIIHH', b, at)
        itype, w, h = b[at + 0x13], *struct.unpack_from('<HH', b, at + 0x14)
        pix = b[at + hsize:at + hsize + img]
        pal = b[at + hsize + img:at + hsize + img + clut]
        csize = {1: 2, 2: 3, 3: 4}.get(b[at + 0x12] & 0x3F, 4)
        if csize == 3:
            cols = [pal[i * 3:i * 3 + 3] + bytes([0x80]) for i in range(len(pal) // 3)]
        elif csize == 2:
            cols = []
            for i in range(len(pal) // 2):
                v, = struct.unpack_from('<H', pal, i * 2)
                cols.append(bytes(((v & 31) << 3, (v >> 5 & 31) << 3, (v >> 10 & 31) << 3, 0x80 if v >> 15 else 0)))
        else:
            cols = [pal[i * 4:i * 4 + 4] for i in range(len(pal) // 4)]
        if itype == 5 and len(cols) >= 256:
            cols = [cols[(i & ~0x18) | ((i & 8) << 1) | ((i & 16) >> 1)] for i in range(256)]
        fix = lambda c: bytes((c[0], c[1], c[2], min(255, c[3] * 2)))
        if itype == 3:
            rgba = b''.join(fix(pix[i:i + 4]) for i in range(0, w * h * 4, 4))
        elif itype == 5:
            rgba = b''.join(fix(cols[v]) for v in pix[:w * h])
        elif itype == 4:
            rgba = b''.join(fix(cols[(pix[i // 2] >> (4 * (i & 1))) & 15]) for i in range(w * h))
        else:
            print(f'picture {k}: type {itype} not handled')
            at += total
            continue
        path = os.path.join(out, f'{name}_{k}.png')
        png(path, w, h, rgba)
        print(f'picture {k}: {w}x{h} type {itype} -> {path}')
        at += total


if __name__ == '__main__':
    main(*sys.argv[1:4])
