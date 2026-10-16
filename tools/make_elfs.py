"""Build one ELF per code variant of Tekken 5 for Ghidra, and extracted/variants.csv.

Usage: python make_elfs.py <extracted_dir>

Tekken 5 is several separate PS2 programs, switched with LoadExecPS2 (each replaces the whole of
EE RAM, so a variant is a complete program, never an overlay):

  main       SLUS_210.59 (crt0, boot loader in .y4, Sony libraries) plus the two images its boot
             loader (0x00D060D0) decompresses from TK5DATA3.BIN: entry 5 at 0x001F0180 (the game's
             code and data; main() is 0x001F0790) and entry 6 at 0x00488000 (the SLUS's .dmh
             section, stripped from the file: sound system and IOP module images)
  tekken1..  TK5DATA3.BIN entries 0..4, complete ELFs (Arcade History titles, Devil Within);
             see VARIANTS for which is which
  loader     TK5DATA4.BIN, an ELF linked at 0x01E00000 (above every program) that reboots the
             IOP with IOPRP300.IMG and unpacks a TK5DATA3 program into 0x00100000

Writes extracted/elf/<variant>.elf (sections named, gp in .reginfo kept) and
extracted/variants.csv (variant, source, entry, gp, start, end, sha1).
"""
import csv
import hashlib
import os
import struct
import sys

# (variant, source, title). Sources: 'main', a disc file name or a TK5DATA3 entry index.
VARIANTS = [
    ('main', 'main', 'Tekken 5 (SLUS_210.59 + TK5DATA3 entries 5 and 6)'),
    ('tekken1', 0, 'Arcade History: Tekken (System 11 port)'),
    ('tekken2', 1, 'Arcade History: Tekken 2'),
    ('tekken3', 2, 'Arcade History: Tekken 3'),
    ('starblade', 3, 'StarBlade'),
    ('devil', 4, 'Devil Within'),
    ('loader', 'TK5DATA4.BIN', 'Program loader (TK5DATA4.BIN)'),
    ('lzstub', 'lzstub', 'Loader unpack routine (decrypted to 0x01E73800)'),
]

# The loader (TK5DATA4 0x01E00658) XORs 46 words at 0x01E1C740 with 0xFF0FFF0F, copies them to
# 0x01E73800 (past its bss) and calls them: the routine that unpacks the program it starts.
STUB_SRC, STUB_WORDS, STUB_KEY, STUB_ADDR = 0x01E1C740, 46, 0xFF0FFF0F, 0x01E73800

MAIN_TEXT = 0x001F0180   # TK5DATA3 entry 5 (SLUS 0x00D060D0: decompress to 0x1F0180)
MAIN_DMH = 0x00488000    # TK5DATA3 entry 6 (SLUS 0x00D060D0: decompress to 0x488000)
MAIN_HEAP_END = 0x01BCA460  # end of .dmh (SetupHeap end in _start)

SHT_PROGBITS, SHT_NOBITS, SHT_STRTAB, SHT_MIPS_REGINFO = 1, 8, 3, 0x70000006
SHF_WRITE, SHF_ALLOC, SHF_EXEC = 1, 2, 4


def read_elf(data):
    """Sections of an ELF as a list of dicts (name, type, flags, addr, data / size)."""
    e_entry = struct.unpack_from('<I', data, 24)[0]
    e_flags = struct.unpack_from('<I', data, 36)[0]
    shoff = struct.unpack_from('<I', data, 32)[0]
    shnum, shstr = struct.unpack_from('<HH', data, 48)
    sh = [struct.unpack_from('<10I', data, shoff + 40 * i) for i in range(shnum)]
    so = sh[shstr][4]
    out = []
    for s in sh[1:]:
        name = data[so + s[0]:data.index(b'\0', so + s[0])].decode()
        typ, flags, addr, off, size = s[1], s[2], s[3], s[4], s[5]
        if not flags & SHF_ALLOC and typ != SHT_MIPS_REGINFO:
            continue
        if size == 0:
            continue
        sec = dict(name=name, type=typ, flags=flags, addr=addr, size=size)
        if typ != SHT_NOBITS:
            sec['data'] = data[off:off + size]
        out.append(sec)
    return e_entry, e_flags, out


def write_elf(path, entry, flags, sections):
    """A little-endian MIPS ELF executable with these sections (one PT_LOAD per allocated one)."""
    names = b'\0'
    for s in sections:
        s['name_off'] = len(names)
        names += s['name'].encode() + b'\0'
    shstr_name = len(names)
    names += b'.shstrtab\0'
    loads = [s for s in sections if s['flags'] & SHF_ALLOC]
    phoff = 52
    off = phoff + 32 * len(loads)
    body = bytearray()
    for s in sections:
        off_aligned = (off + len(body) + 15) & ~15
        body += b'\0' * (off_aligned - off - len(body))
        s['offset'] = off_aligned
        if s['type'] != SHT_NOBITS:
            body += s['data']
    names_off = off + len(body)
    body += names
    shoff = (off + len(body) + 3) & ~3
    body += b'\0' * (shoff - off - len(body))
    shnum = len(sections) + 2
    head = struct.pack('<4sBBBB8sHHIIIIIHHHHHH', b'\x7fELF', 1, 1, 1, 0, b'\0' * 8, 2, 8, 1, entry,
                       phoff, shoff, flags, 52, 32, len(loads), 40, shnum, shnum - 1)
    ph = b''
    for s in loads:
        filesz = 0 if s['type'] == SHT_NOBITS else s['size']
        pf = 4 | (2 if s['flags'] & SHF_WRITE else 0) | (1 if s['flags'] & SHF_EXEC else 0)
        ph += struct.pack('<8I', 1, s['offset'], s['addr'], s['addr'], filesz, s['size'], pf, 16)
    sh = b'\0' * 40
    for s in sections:
        sh += struct.pack('<10I', s['name_off'], s['type'], s['flags'], s['addr'], s['offset'],
                          s['size'], 0, 0, 16 if s['flags'] & SHF_ALLOC else 4, 0)
    sh += struct.pack('<10I', shstr_name, SHT_STRTAB, 0, 0, names_off, len(names), 0, 0, 1, 0)
    with open(path, 'wb') as f:
        f.write(head + ph + body + sh)


def main_sections(ex):
    """The main program: SLUS_210.59 with .dmh replaced by TK5DATA3 entry 6, entry 5 added."""
    with open(os.path.join(ex, 'files', 'SLUS_210.59'), 'rb') as f:
        entry, flags, secs = read_elf(f.read())
    secs = [s for s in secs if s['name'] != '.dmh']
    with open(os.path.join(ex, 'tk5data3', 'entry5.bin'), 'rb') as f:
        text = f.read()
    with open(os.path.join(ex, 'tk5data3', 'entry6.bin'), 'rb') as f:
        dmh = f.read()
    rwx = SHF_ALLOC | SHF_WRITE | SHF_EXEC
    secs.append(dict(name='.tk5text', type=SHT_PROGBITS, flags=rwx, addr=MAIN_TEXT, size=len(text), data=text))
    secs.append(dict(name='.dmh', type=SHT_PROGBITS, flags=rwx, addr=MAIN_DMH, size=len(dmh), data=dmh))
    y4 = next(s for s in secs if s['name'] == '.y4')
    heap = (MAIN_DMH + len(dmh) + 15) & ~15
    secs.append(dict(name='.heap', type=SHT_NOBITS, flags=SHF_ALLOC | SHF_WRITE, addr=heap,
                     size=y4['addr'] - heap))
    y4_end = (y4['addr'] + y4['size'] + 15) & ~15
    secs.append(dict(name='.heap2', type=SHT_NOBITS, flags=SHF_ALLOC | SHF_WRITE, addr=y4_end,
                     size=MAIN_HEAP_END - y4_end))
    secs.sort(key=lambda s: (s['addr'] == 0, s['addr']))
    return entry, flags, secs


def lzstub_sections(ex):
    """The routine the loader decrypts and runs at 0x01E73800 (gp and flags of the loader)."""
    with open(os.path.join(ex, 'files', 'TK5DATA4.BIN'), 'rb') as f:
        _, flags, secs = read_elf(f.read())
    src = next(s for s in secs if s['type'] != SHT_NOBITS and s['flags'] & SHF_ALLOC
               and s['addr'] <= STUB_SRC < s['addr'] + s['size'])
    o = STUB_SRC - src['addr']
    words = struct.unpack_from(f'<{STUB_WORDS}I', src['data'], o)
    text = b''.join(struct.pack('<I', x ^ STUB_KEY) for x in words)
    reg = next(s for s in secs if s['type'] == SHT_MIPS_REGINFO)
    out = [dict(name='.text', type=SHT_PROGBITS, flags=SHF_ALLOC | SHF_EXEC, addr=STUB_ADDR, size=len(text), data=text),
           dict(reg)]
    return STUB_ADDR, flags, out


def main():
    """Write the ELFs and variants.csv for the extracted folder given on the command line."""
    ex = sys.argv[1]
    os.makedirs(os.path.join(ex, 'elf'), exist_ok=True)
    with open(os.path.join(ex, 'variants.csv'), 'w', newline='') as fo:
        w = csv.writer(fo)
        w.writerow(['variant', 'source', 'title', 'entry', 'gp', 'start', 'end', 'elf', 'sha1'])
        for name, src, title in VARIANTS:
            if src == 'lzstub':
                entry, flags, secs = lzstub_sections(ex)
                source = 'TK5DATA4.BIN@0x01E1C740^0xFF0FFF0F'
            elif src == 'main':
                entry, flags, secs = main_sections(ex)
                source = 'SLUS_210.59+TK5DATA3:5@0x001F0180+TK5DATA3:6@0x00488000'
            elif isinstance(src, str):
                with open(os.path.join(ex, 'files', src), 'rb') as f:
                    entry, flags, secs = read_elf(f.read())
                source = src
            else:
                with open(os.path.join(ex, 'tk5data3', f'entry{src}.bin'), 'rb') as f:
                    entry, flags, secs = read_elf(f.read())
                source = f'TK5DATA3:{src}'
            reg = next(s for s in secs if s['type'] == SHT_MIPS_REGINFO)
            gp = struct.unpack_from('<I', reg['data'], 20)[0]
            path = os.path.join(ex, 'elf', f'{name}.elf')
            write_elf(path, entry, flags, secs)
            alloc = [s for s in secs if s['flags'] & SHF_ALLOC]
            start = min(s['addr'] for s in alloc)
            end = max(s['addr'] + s['size'] for s in alloc if s['type'] != SHT_NOBITS)
            with open(path, 'rb') as f:
                sha = hashlib.sha1(f.read()).hexdigest()
            w.writerow([name, source, title, f'0x{entry:08X}', f'0x{gp:08X}', f'0x{start:08X}',
                        f'0x{end:08X}', f'elf/{name}.elf', sha])
            print(f'{name}: entry {entry:#x} gp {gp:#x} {start:#x}..{end:#x}')


if __name__ == '__main__':
    main()
