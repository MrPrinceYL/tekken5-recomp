"""Fix the R5900 multiply / divide semantics of ghidra-emotionengine-reloaded 2.1.38, then
recompile r5900.sla.

Usage: python fix_ee_sleigh.py [<extension languages dir>]

The extension's p-code for these instructions is wrong, and the recompiler runs the p-code:
- divu: signed division and remainder (s/, s%);
- mult / multu (3-operand forms): only the low word of LO / HI written, rd gets the whole
  64-bit product instead of sign-extended LO; and the 32-bit product is formed before widening,
  so HI is just the sign of LO;
- mult1 / multu1 / madd / maddu / madd1 / maddu1: the same narrow product, the accumulator
  formed as `lo | hi` instead of HI:LO, unsigned results zero-extended instead of sign-extended;
- divu1: results zero-extended;
- dsllv / dsrlv / dsrav: the shift amount is not masked to 6 bits.
On the R5900 the product is the full 64-bit one, HI:LO (or HI1:LO1) are its two words, each
sign-extended into the 64-bit registers, and rd (if any) gets LO. Division results are
sign-extended too.
The original files are kept as *.orig. Run Ghidra's export again afterwards (run_ghidra.sh only
re-exports when the analysis markers exist: delete export/<variant>).
"""
import os
import re
import shutil
import subprocess
import sys

LANG = sys.argv[1] if len(sys.argv) > 1 else os.path.expandvars(
    r"%APPDATA%\ghidra\ghidra_12.1.4_PUBLIC\Extensions\ghidra-emotionengine-reloaded\data\languages")
GHIDRA = r"E:\Decompilation\Tools\ghidra_12.1.4_PUBLIC"
JDK = r"E:\Decompilation\Tools\jdk-21"


def mul(src_ext, lo, hi, rd, acc=False):
    """Body of a multiply (or multiply-add) into `lo` / `hi`."""
    body = f"    prod:8 = {src_ext}(RS32src) * {src_ext}(RT32src);\n"
    if acc:
        body = (f"    accum:8 = (zext({hi}[0,32]) << 32) | zext({lo}[0,32]);\n"
                f"    prod:8 = accum + {src_ext}(RS32src) * {src_ext}(RT32src);\n")
    body += f"    {lo} = sext(prod[0,32]);\n    {hi} = sext(prod[32,32]);\n"
    if rd:
        body += f"    RD = {lo};\n"
    return body


def div(signed, lo, hi):
    op, rem = ("s/", "s%") if signed else ("/", "%")
    return f"    {lo} = sext(RS32src {op} RT32src);\n    {hi} = sext(RS32src {rem} RT32src);\n"


# (file, constructor header regex, new body)
FIXES = [
    # Variable 64-bit shifts use the low 6 bits of rs.
    ("mips.sinc", r":dsllv RD, RTsrc, RSsrc\s+is[^{]*\{", "    RD = RTsrc << (RSsrc & 0x3f);\n"),
    ("mips.sinc", r":dsrlv RD, RTsrc, RSsrc\s+is[^{]*\{", "    RD = RTsrc >> (RSsrc & 0x3f);\n"),
    ("mips.sinc", r":dsrav RD, RTsrc, RSsrc\s+is[^{]*\{", "    RD = RTsrc s>> (RSsrc & 0x3f);\n"),
    ("mips.sinc", r":divu RS32src, RT32src\s+is[^{]*\{", div(False, "lo", "hi")),
    ("mips.sinc", r":mult RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo", "hi", False)),
    ("mips.sinc", r":multu RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo", "hi", False)),
    ("eecore.sinc", r":mult RD, RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo", "hi", True)),
    ("eecore.sinc", r":multu RD, RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo", "hi", True)),
    ("eecore.sinc", r":mult1 RD, RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo1", "hi1", True)),
    ("eecore.sinc", r":mult1 RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo1", "hi1", False)),
    ("eecore.sinc", r":multu1 RD, RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo1", "hi1", True)),
    ("eecore.sinc", r":multu1 RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo1", "hi1", False)),
    ("eecore.sinc", r":div1 RS32src, RT32src\s+is[^{]*\{", div(True, "lo1", "hi1")),
    ("eecore.sinc", r":divu1 RS32src, RT32src\s+is[^{]*\{", div(False, "lo1", "hi1")),
    ("eecore.sinc", r":madd RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo", "hi", False, True)),
    ("eecore.sinc", r":madd RD, RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo", "hi", True, True)),
    ("eecore.sinc", r":madd1 RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo1", "hi1", False, True)),
    ("eecore.sinc", r":madd1 RD, RS32src, RT32src\s+is[^{]*\{", mul("sext", "lo1", "hi1", True, True)),
    ("eecore.sinc", r":maddu RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo", "hi", False, True)),
    ("eecore.sinc", r":maddu RD, RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo", "hi", True, True)),
    ("eecore.sinc", r":maddu1 RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo1", "hi1", False, True)),
    ("eecore.sinc", r":maddu1 RD, RS32src, RT32src\s+is[^{]*\{", mul("zext", "lo1", "hi1", True, True)),
]


def main():
    texts = {}
    for name in {f for f, _, _ in FIXES}:
        path = os.path.join(LANG, name)
        if not os.path.exists(path + ".orig"):
            shutil.copy(path, path + ".orig")
        with open(path + ".orig") as fh:
            texts[name] = fh.read()
    for name, head, body in FIXES:
        s = texts[name]
        m = re.search(head, s)
        if not m:
            sys.exit(f"{name}: no match for {head}")
        end = s.index("}", m.end())
        texts[name] = s[:m.end()] + "\n" + body + s[end:]
    for name, s in texts.items():
        with open(os.path.join(LANG, name), "w") as fh:
            fh.write(s)
    env = dict(os.environ, JAVA_HOME=JDK, PATH=os.path.join(JDK, "bin") + os.pathsep + os.environ["PATH"])
    r = subprocess.run([os.path.join(GHIDRA, "support", "sleigh.bat"), "r5900.slaspec"], cwd=LANG, env=env,
                       capture_output=True, text=True)
    print(r.stdout[-2000:], r.stderr[-2000:])
    if r.returncode != 0:
        sys.exit("sleigh failed")
    print(f"patched {len(FIXES)} constructors, r5900.sla rebuilt")


if __name__ == "__main__":
    main()
