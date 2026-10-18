"""Collect Devil Within's texts, old file versions and text images into one folder (devTextEtc).

    python tools/dw_dev_texts.py <extracted/tk5data2> <out dir>

Writes (UTF-8; Shift-JIS converted):
  story.txt              the intermission texts per scene and page (as the game shows them)
  messages_all.txt       every message of cap_teg_msg_font_account by number (menu help, rewards)
  menutext.txt           the short menu texts (cap_teg_msg_font_menutext)
  warp_list_final.txt    area/mpl.txt (developer warp list, 81 entries)
  warp_list_old.txt      area/mpl (older version: coordinates, 6 rooms per area)
  sound_config_final.txt / sound_config_2005-01-25.txt   common/nus_config_ps2.txt / .txt~
  chr_000_final.txt / chr_000_old.txt                     area/chr/chr_000.txt / .txt~
  images/*.png           intermission background, instruction screen, menu font
  README.md              what is what (DevilWithin.md, section 4)
"""
import os
import shutil
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tim2png  # noqa: E402

# Intermission pages (0x2CEEF8 pages per scene, 0x2CEF10 first message per page, 13 lines each).
SCENES = [('Opening', [63, 76, 89]), ('Before stage 2', [102, 115]), ('Before stage 3', [128, 141, 154]),
          ('Before stage 4', [167, 180]), ('Before stage 5', [193, 206]), ('Ending', [219, 232, 245, 258])]
LINES = 13


def char(v):
    c = v - 0xC1
    if not 0x61 <= c <= 0x7A:
        c -= 0x20
    return chr(c) if 0x20 <= c < 0x7F else f'<{v:#x}>'


def cap(path, name):
    """(messages {number: offset}, text-area reader) of a CAP message file."""
    b = tim2png.entry(path, name)
    index, texts = struct.unpack_from('<II', b, 0x18)
    msgs = {}
    for o in range(index, texts, 8):
        off, _, num, _ = struct.unpack_from('<HHHH', b, o)
        msgs[num] = off

    def first_code(off):
        return struct.unpack_from('<H', b, texts + off * 2)[0]

    def line(off, help_text=False):
        s, o = '', texts + off * 2
        while o + 1 < len(b):
            v, = struct.unpack_from('<H', b, o)
            o += 2
            if v == 0:
                if s.strip() or help_text:
                    break
                continue
            if v == 2:
                break
            s += ' ' if v == 3 else ' / ' if v == 1 else char(v) if v >= 0x100 else ''
        return s
    return msgs, line, first_code


def sjis(src, dst):
    open(dst, 'w', encoding='utf-8', newline='\n').write(open(src, 'rb').read().decode('shift_jis', 'replace').replace('\r\n', '\n'))


def main(root, out):
    os.makedirs(os.path.join(out, 'images'), exist_ok=True)
    base = os.path.join(root, 'common', 'base.pak')
    msgs, line, first_code = cap(base, 'cap_teg_msg_font_account')
    with open(os.path.join(out, 'story.txt'), 'w', encoding='utf-8', newline='\n') as f:
        for scene, pages in SCENES:
            f.write(f'===== {scene} =====\n')
            for p, first in enumerate(pages, 1):
                f.write(f'--- page {p} ---\n')
                seen = []
                for n in range(first, first + LINES):
                    # 0x15F478 skips an entry that starts with a pause (code 0): only the entry
                    # at a line's first character is shown as a line.
                    if n in msgs and first_code(msgs[n]) != 0:
                        seen.append(line(msgs[n]).strip())
                f.write('\n'.join(seen) + '\n\n')
    with open(os.path.join(out, 'messages_all.txt'), 'w', encoding='utf-8', newline='\n') as f:
        f.write('# cap_teg_msg_font_account (common/base.pak): number, text. 63-270 = intermission\n'
                '# lines (one per typewriter pause), 271-324 menu help, 366-406 rewards; 325-365 absent.\n')
        for n in sorted(msgs):
            f.write(f'{n:4}  {line(msgs[n], help_text=True)}\n')
    mm, _, _ = cap(base, 'cap_teg_msg_font_menutext')
    with open(os.path.join(out, 'menutext.txt'), 'w', encoding='utf-8', newline='\n') as f:
        offs = sorted(mm.items(), key=lambda kv: kv[1])
        b = tim2png.entry(base, 'cap_teg_msg_font_menutext')
        texts = struct.unpack_from('<I', b, 0x1C)[0]
        for k, (n, off) in enumerate(offs):
            end = offs[k + 1][1] if k + 1 < len(offs) else (len(b) - texts) // 2
            words = [struct.unpack_from('<H', b, texts + 2 * i)[0] for i in range(off, end)]
            f.write(f'{n:4}  ' + ''.join(' ' if v in (2, 3) else char(v) if v >= 0x100 else '' for v in words).strip() + '\n')
    for src, dst in [('area/mpl.txt', 'warp_list_final.txt'), ('area/mpl', 'warp_list_old.txt'),
                     ('common/nus_config_ps2.txt', 'sound_config_final.txt'),
                     ('common/nus_config_ps2.txt~', 'sound_config_2005-01-25.txt'),
                     ('area/chr/chr_000.txt', 'chr_000_final.txt'), ('area/chr/chr_000.txt~', 'chr_000_old.txt')]:
        sjis(os.path.join(root, *src.split('/')), os.path.join(out, dst))
    img = os.path.join(out, 'images')
    for pak, name in [('demo.pak', 'nut_hy_makuma'), ('inst.pak', 'nut_hy_instruction'), ('demo.pak', 'nut_menu_font_4')]:
        tim2png.main(os.path.join(root, 'common', pak), name, img)
    readme = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'dw_dev_texts_readme.md')
    shutil.copy(readme, os.path.join(out, 'README.md'))
    print(f'written to {out}')


if __name__ == '__main__':
    main(sys.argv[1], sys.argv[2])
