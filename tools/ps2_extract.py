"""Extract a PlayStation 2 DVD image (ISO9660, 2048-byte sectors) into a folder.

Usage: python ps2_extract.py <image.iso> <out_dir> [--max-size BYTES]

Writes every ISO9660 file up to --max-size bytes (default 64 MiB: the TK5DATA archives stay on
the disc, the runtime and the archive tools read them from the image) and disc_files.csv
(path, lba, size, extracted).
"""
import csv
import os
import struct
import sys

SECTOR = 2048


def read_extent(f, lba, size):
    """`size` bytes starting at sector `lba`."""
    f.seek(lba * SECTOR)
    return f.read(size)


def parse_dir(f, lba, size, prefix, files):
    """Walk an ISO9660 directory extent recursively, appending (path, lba, size) to files."""
    data = read_extent(f, lba, size)
    pos = 0
    while pos < len(data):
        ln = data[pos]
        if ln == 0:
            pos = (pos // SECTOR + 1) * SECTOR
            continue
        rec = data[pos:pos + ln]
        ext_lba = struct.unpack_from('<I', rec, 2)[0]
        ext_size = struct.unpack_from('<I', rec, 10)[0]
        flags = rec[25]
        name = rec[33:33 + rec[32]]
        pos += ln
        if name in (b'\0', b'\1'):
            continue
        name = name.decode('latin1').split(';')[0]
        if flags & 2:
            parse_dir(f, ext_lba, ext_size, prefix + name + '/', files)
        else:
            files.append((prefix + name, ext_lba, ext_size))


def main():
    """Extract the image given on the command line."""
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    max_size = 64 << 20
    if '--max-size' in sys.argv:
        max_size = int(sys.argv[sys.argv.index('--max-size') + 1], 0)
        args = [a for a in args if a != sys.argv[sys.argv.index('--max-size') + 1]]
    image, out = args[0], args[1]
    files = []
    with open(image, 'rb') as f:
        pvd = read_extent(f, 16, SECTOR)
        if pvd[1:6] != b'CD001':
            sys.exit('not an ISO9660 image')
        root = pvd[156:190]
        parse_dir(f, struct.unpack_from('<I', root, 2)[0], struct.unpack_from('<I', root, 10)[0], '', files)
        os.makedirs(os.path.join(out, 'files'), exist_ok=True)
        with open(os.path.join(out, 'disc_files.csv'), 'w', newline='') as fo:
            w = csv.writer(fo)
            w.writerow(['path', 'lba', 'size', 'extracted'])
            for path, lba, size in sorted(files, key=lambda x: x[1]):
                ext = size <= max_size
                if ext:
                    dst = os.path.join(out, 'files', path)
                    os.makedirs(os.path.dirname(dst), exist_ok=True)
                    with open(dst, 'wb') as d:
                        d.write(read_extent(f, lba, size))
                w.writerow([path, lba, size, int(ext)])
                print(f'{lba:8d} {size:11d} {"x" if ext else " "} {path}')


if __name__ == '__main__':
    main()
