#!/usr/bin/env bash
# Import, analyze and export every code variant of Tekken 5 with ghidra-cli.
#
# Each row of extracted/variants.csv (make_elfs.py) is one whole PS2 program: extracted/elf/<v>.elf.
#
# Usage: bash run_ghidra.sh [variant ...]   (default: every variant)
#        IOP=1 bash run_ghidra.sh [module ...] (the IOP modules of extracted/iop_modules.csv,
#        exported to export_iop/<module>)
# Env:   T5_WORK   work folder (default ../../Tekken5: extracted/, ghidra/, export/)
#        GHIDRA    ghidra-cli executable
#
# Resumable: markers in ghidra/ (<prog>.imported, .analyzed, .delayfix, .ptrcode). Delete
# <prog>.analyzed to re-analyze one variant. Fails loudly if an export is empty.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd -W 2>/dev/null || pwd)"
WORK="${T5_WORK:-$(cd "$HERE/../.." && pwd -W 2>/dev/null || pwd)/Tekken5}"
GHIDRA="${GHIDRA:-ghidra}"
PROJ=TEKKEN5
export GHIDRA_CLI_READ_TIMEOUT=0
mkdir -p "$WORK/ghidra" "$WORK/export"

g() {  # ghidra-cli, retried: a freshly (re)started bridge sometimes refuses the first connection
  local n
  for n in 1 2 3; do
    "$GHIDRA" --projects-dir "$WORK/ghidra" --project $PROJ "$@" && return 0
    echo "retrying ($n): $*" >&2
    sleep 10
  done
  return 1
}

bridge_stop() {  # graceful: the bridge commits and saves its program only when it shuts down
  "$GHIDRA" --projects-dir "$WORK/ghidra" --project $PROJ stop >/dev/null 2>&1 || true
}

bridge_stop_empty() {  # a bridge started without a program does not exit after `stop`;
                       # it holds nothing unsaved, so end its JVM (found by its command line)
  timeout 60 "$GHIDRA" --projects-dir "$WORK/ghidra" --project $PROJ stop >/dev/null 2>&1 || true
  sleep 5
  powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='java.exe'\" | Where-Object { \$_.CommandLine -match '\"-process\" \"-noanalysis\"' } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force }" || true
  sleep 2
  rm -f "$LOCALAPPDATA"/ghidra-cli/bridge-*.pid "$LOCALAPPDATA"/ghidra-cli/bridge-*.port
}

process() {  # name elf gp
  local name=$1 elf=$2 gp=$3 prog done n f
  prog="$(basename "$elf")"
  done="$WORK/ghidra/$prog.analyzed"
  echo "=== $name ($prog) gp=$gp"
  bridge_stop
  if [ ! -f "$done" ]; then
    if [ ! -f "$WORK/ghidra/$prog.imported" ]; then
      if [ -f "$WORK/ghidra/$PROJ.gpr" ]; then
        # Start the bridge explicitly first: a bridge started implicitly by a command keeps
        # that command's output open, so a client whose output is piped never seems to return.
        g start >/dev/null 2>&1 </dev/null
        g import "$elf" --no-analyze </dev/null
      else
        # The first import creates the project (`project create` ignores --projects-dir). Its
        # client starts the bridge itself and may not return: wait for the project, then end it.
        "$GHIDRA" --projects-dir "$WORK/ghidra" --project $PROJ import "$elf" --no-analyze </dev/null >/dev/null 2>&1 &
        local imp=$! i
        for i in $(seq 1 300); do
          [ -d "$WORK/ghidra/$PROJ.rep" ] && "$GHIDRA" --projects-dir "$WORK/ghidra" --project $PROJ \
            program list </dev/null 2>/dev/null | grep -q "$prog" && break
          sleep 2
        done
        kill "$imp" 2>/dev/null || true
      fi
      bridge_stop_empty
      touch "$WORK/ghidra/$prog.imported"
    fi
    g --program "$prog" script run "$HERE/SetGp.java" -- "$gp"
    g --program "$prog" script run "$HERE/Analyze.java"
    g --program "$prog" script run "$HERE/FixNoReturn.java"
    g --program "$prog" script run "$HERE/CreateGapFunctions.java"
    bridge_stop  # saves the analysis
    touch "$done"
  fi
  if [ ! -f "$WORK/ghidra/$prog.delayfix" ]; then
    g --program "$prog" script run "$HERE/FixDelaySlotTargets.java"
    bridge_stop  # saves
    touch "$WORK/ghidra/$prog.delayfix"
  fi
  if [ ! -f "$WORK/ghidra/$prog.ptrcode" ]; then
    g --program "$prog" script run "$HERE/DisassemblePointerTargets.java"
    bridge_stop  # saves
    touch "$WORK/ghidra/$prog.ptrcode"
  fi
  if [ -f "$WORK/extracted/seeds/$name.csv" ] && [ ! -f "$WORK/ghidra/$prog.seeds" ]; then
    # Functions found missing later (unknown call targets at run time), with evidence.
    g --program "$prog" script run "$HERE/CreateFunctions.java" -- "$WORK/extracted/seeds/$name.csv"
    bridge_stop  # saves
    touch "$WORK/ghidra/$prog.seeds"
  fi
  if [ -f "$HERE/ApplyAnnotations.java" ]; then
    # Names and comments from the sheets (the code and its p-code are unchanged).
    g --program "$prog" script run "$HERE/ApplyAnnotations.java" -- "$HERE/../sheets" "$name"
    bridge_stop  # saves
  fi
  local ex="${EXPORT_DIR:-$WORK/export}/$name"
  rm -rf "$ex"
  g --program "$prog" script run "$HERE/ExportPcode.java" -- "$ex"
  g --program "$prog" script run "$HERE/ExportSheets.java" -- "$ex"
  bridge_stop
  n=$(( $(wc -l < "$ex/instructions.csv") - 1 ))
  if [ "$n" -le 0 ]; then
    echo "$name: export has no instructions (analysis not saved?); delete $done and rerun" >&2
    exit 1
  fi
  f=$(( $(wc -l < "$ex/functions_raw.csv") - 1 ))
  if [ "$f" -le 0 ]; then
    echo "$name: export has no functions; delete $done and rerun" >&2
    exit 1
  fi
  echo "$name: $n instructions, $f functions exported"
}

want() { [ ${#SEL[@]} -eq 0 ] && return 0; local v; for v in "${SEL[@]}"; do [ "$v" = "$1" ] && return 0; done; return 1; }
SEL=("$@")

if [ "${IOP:-0}" = 1 ]; then
  EXPORT_DIR="$WORK/export_iop"
  tail -n +2 "$WORK/extracted/iop_modules.csv" | tr -d '\r' | while IFS=, read -r module irx version source sha size base msize entry gp elf; do
    want "$module" || continue
    process "$module" "$WORK/extracted/$elf" "$gp"
  done
else
  tail -n +2 "$WORK/extracted/variants.csv" | tr -d '\r' | while IFS=, read -r variant source title entry gp start end elf sha; do
    want "$variant" || continue
    process "$variant" "$WORK/extracted/$elf" "$gp"
  done
fi
echo "done"
