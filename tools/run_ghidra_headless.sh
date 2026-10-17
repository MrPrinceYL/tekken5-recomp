#!/usr/bin/env bash
# Linux without ghidra-cli: the steps of run_ghidra.sh for ONE program / IOP module, done by
# Ghidra's analyzeHeadless (same scripts, same order, one session).
#
# Usage: bash run_ghidra_headless.sh <name> <elf> <gp hex> <export dir>
#   IOP module (row of extracted/iop_modules.csv):
#     bash run_ghidra_headless.sh owadpcm_driver_072460 ../../Tekken5/extracted/iop/owadpcm_driver_072460.elf \
#          0x00119B80 ../../Tekken5/export_iop/owadpcm_driver_072460
# Env:   GHIDRA_DIR  Ghidra install (default: a copy in ~/ghidra_t5, see below)
#        GHIDRA_PROJ project folder (default ../../Tekken5/ghidra/headless; delete it afterwards)
#        JAVA_HOME   JDK 21 (default /usr/lib/jvm/java-21-openjdk)
#
# Pitfalls (17.10.):
# - The Ghidra in Tools/ is checked in from Windows: launch.sh, the decompiler and the demangler
#   are not executable. Do not chmod them in the repo (mode changes in git); copy it once:
#     cp -a ../../../Tools/ghidra_12.1.4_PUBLIC ~/ghidra_t5 && chmod +x ~/ghidra_t5/support/*.sh \
#       ~/ghidra_t5/support/analyzeHeadless ~/ghidra_t5/Ghidra/Features/*/os/linux_x86_64/* \
#       ~/ghidra_t5/GPL/*/os/linux_x86_64/*
# - The project path must not contain an element starting with '.' (e.g. ~/.claude/...).
# - EE programs need the ghidra-emotionengine-reloaded extension (r5900) in that Ghidra; IOP
#   modules use the built-in MIPS:LE:32:default.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
NAME=$1; ELF=$(realpath "$2"); GP=$3; EX=$(realpath -m "$4")
G="${GHIDRA_DIR:-$HOME/ghidra_t5}"
P="${GHIDRA_PROJ:-$(realpath -m "$HERE/../../Tekken5/ghidra/headless")}"
export JAVA_HOME="${JAVA_HOME:-/usr/lib/jvm/java-21-openjdk}"
mkdir -p "$P"
rm -rf "$EX"
SEEDS=()
[ -f "$HERE/../../Tekken5/extracted/seeds/$NAME.csv" ] && SEEDS=(-postScript CreateFunctions.java "$(realpath "$HERE/../../Tekken5/extracted/seeds/$NAME.csv")")
bash "$G/support/analyzeHeadless" "$P" T5 -import "$ELF" -overwrite -noanalysis -scriptPath "$HERE" \
  -postScript SetGp.java "$GP" -postScript Analyze.java -postScript FixNoReturn.java \
  -postScript CreateGapFunctions.java -postScript FixDelaySlotTargets.java \
  -postScript DisassemblePointerTargets.java "${SEEDS[@]}" \
  -postScript ExportPcode.java "$EX" -postScript ExportSheets.java "$EX"
n=$(( $(wc -l < "$EX/instructions.csv") - 1 ))
f=$(( $(wc -l < "$EX/functions_raw.csv") - 1 ))
[ "$n" -gt 0 ] && [ "$f" -gt 0 ] || { echo "$NAME: empty export" >&2; exit 1; }
echo "$NAME: $n instructions, $f functions exported"
