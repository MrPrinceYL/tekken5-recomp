"""Build the Spreadsheet-Method tables from the raw Ghidra exports.

Inputs  (../../Tekken5/export/<variant>/): functions_raw.csv calls.csv strings.csv globals.csv
                                           sections.csv instructions.csv memory.csv registers.csv
                                           raw_c/*.c
        (../../Tekken5/extracted/):        variants.csv (make_elfs.py), disc_files.csv
                                           (ps2_extract.py), tk5data3_toc.csv (tk5data.py), files/
Outputs (sheets/):  meta.csv variants.csv functions.csv units.csv calls.csv strings.csv
                    globals.csv sections.csv registers.csv disc_files.csv tk5data3_toc.csv
                    instructions/<variant>.csv memory/<variant>.csv  (big: one file per variant)
        (src/raw_c/<variant>/, src/units/): the Ghidra C, one file per function and per unit

Tekken 5 is several whole PS2 programs (sheets/variants.csv), switched with LoadExecPS2 / ExecPS2:
the main game (SLUS_210.59 plus the code its boot loader unpacks from TK5DATA3.BIN), the Arcade
History titles, Devil Within and the program loader (TK5DATA4.BIN). Programs share addresses, so
every row of functions / instructions / calls is keyed by (variant, addr).

The programs have no symbols. Sony library functions are recognised by the hardware registers
they touch, the kernel calls they make (Ghidra's EE analyzer names the syscall stubs) and the
strings they print; the remaining ones inherit the library of their neighbours when both sides
agree. Everything else is game code, split into address-ordered work units of UNIT_BYTES.

Re-running preserves the hand-edited columns (name, unit, status, owner, notes) of
sheets/functions.csv, so the sheet stays the source of truth across re-exports.
sheets/manual_names.csv (variant, addr, name, status, evidence, section, summary) always wins.
"""
import csv
import hashlib
import os
import re
import shutil
import textwrap
from collections import Counter, defaultdict
from pathlib import Path

csv.field_size_limit(1 << 30)
OUT = Path(__file__).resolve().parent.parent                          # Tekken5Decomp/
WORK = Path(os.environ.get("T5_WORK", OUT.parent / "Tekken5"))         # Ghidra project, export/, extracted/
EXP, EX, SHEETS, SRC = WORK / "export", WORK / "extracted", OUT / "sheets", OUT / "src"
STATUSES = ["raw", "named", "typed", "clean", "verified"]
UNIT_BYTES = 0x4000
NEIGHBOUR_GAP = 12

# Evidence -> Sony EE library, checked in order on each function's decompiled C.
LIB_RULES = [
    ("libipu", [r"0x1000200[0-9a-f]", r"_1000201", r"\bIPU\b", r"sceIpu", r"MPEG", r"sceMpeg"]),
    ("libgraph", [r"0x1200[0-9a-f]{4}", r"_120010", r"sceGs[A-Z]", r"0x1000300[0-9a-f]", r"GsSync"]),
    ("libdma", [r"0x1000[89a-e][0-9a-f]{3}\b", r"sceDma[A-Z]", r"_1000e0"]),
    ("libvu0", [r"sceVu0[A-Z]"]),
    ("libvif", [r"0x10003[89a-f][0-9a-f]{2}", r"sceVif"]),
    ("libpad", [r"libpad", r"padman", r"scePad[A-Z]"]),
    ("libmc", [r"libmc", r"mcserv", r"mcman", r"sceMc[A-Z]"]),
    ("libcdvd", [r"sceCd[A-Z]", r"cdvd", r"CDVD"]),
    ("libsd", [r"sceSd[A-Z]", r"libsdr", r"sdrdrv", r"SDR driver"]),
    ("libkernl", [r"\bSif[A-Z]", r"sceSif[A-Z]", r"0x1000f2[0-9a-f]{2}", r"Sif(Set|Get|Call|Bind|Init)",
                  r"sceSifLoadModule", r"isceSif"]),
    ("libdev", [r"sceDev[A-Z]", r"0x1000[0-7][0-9a-f]{3}\b"]),
    ("libpkt", [r"libpkt", r"sceVif1Pk", r"sceGifPk"]),
]
# Kernel calls (Ghidra's EE analyzer names the syscall stubs after the kernel's table).
KERNEL = {"ResetEE", "SetGsCrt", "Exit", "_ExceptionEpilogue", "LoadExecPS2", "ExecPS2", "RFU009",
          "AddSbusIntcHandler", "RemoveSbusIntcHandler", "Interrupt2Iop", "SetVTLBRefillHandler",
          "SetVCommonHandler", "SetVInterruptHandler", "AddIntcHandler", "AddIntcHandler2",
          "RemoveIntcHandler", "AddDmacHandler", "AddDmacHandler2", "RemoveDmacHandler",
          "_EnableIntc", "_DisableIntc", "_EnableDmac", "_DisableDmac", "_SetAlarm", "_ReleaseAlarm",
          "_iEnableIntc", "_iDisableIntc", "_iEnableDmac", "_iDisableDmac", "_iSetAlarm",
          "_iReleaseAlarm", "CreateThread", "DeleteThread", "StartThread", "ExitThread",
          "ExitDeleteThread", "TerminateThread", "iTerminateThread", "DisableDispatchThread",
          "EnableDispatchThread", "ChangeThreadPriority", "iChangeThreadPriority",
          "RotateThreadReadyQueue", "iRotateThreadReadyQueue", "ReleaseWaitThread",
          "iReleaseWaitThread", "GetThreadId", "ReferThreadStatus", "iReferThreadStatus",
          "SleepThread", "WakeupThread", "iWakeupThread", "CancelWakeupThread",
          "iCancelWakeupThread", "SuspendThread", "iSuspendThread", "ResumeThread",
          "iResumeThread", "JoinThread", "SetupThread", "SetupHeap", "EndOfHeap", "CreateSema",
          "DeleteSema", "SignalSema", "iSignalSema", "WaitSema", "PollSema", "iPollSema",
          "ReferSemaStatus", "iReferSemaStatus", "FlushCache", "iFlushCache", "GsGetIMR",
          "iGsGetIMR", "GsPutIMR", "iGsPutIMR", "SetVSyncFlag", "SetSyscall", "SifStopDma",
          "SifDmaStat", "isceSifDmaStat", "SifSetDma", "isceSifSetDma", "SifSetDChain",
          "isceSifSetDChain", "SifSetReg", "SifGetReg", "ExecOSD", "Deci2Call", "MachineType",
          "GetMemorySize", "_InitTLB", "Copy", "SetAlarm", "ReleaseAlarm", "iSetAlarm",
          "iReleaseAlarm", "EnableIntc", "DisableIntc", "EnableDmac", "DisableDmac",
          "iEnableIntc", "iDisableIntc", "iEnableDmac", "iDisableDmac"}
LIBC = {"memcpy", "memset", "memmove", "memcmp", "strlen", "strcpy", "strncpy", "strcmp", "strncmp",
        "strcat", "strchr", "strrchr", "sprintf", "vsprintf", "printf", "puts", "malloc", "free",
        "calloc", "realloc", "abort", "exit", "atexit", "_exit", "rand", "srand", "qsort", "atoi",
        "abs", "sqrtf", "sinf", "cosf", "atan2f", "floor", "ceil", "fmod", "__main", "_start"}


def clear_dir(d):
    """Empty a generated folder (files removed, folders kept: removing a folder fails on
    Windows while any program has it open)."""
    for f in sorted(d.rglob("*"), reverse=True):
        if f.is_file():
            f.unlink()
        else:
            try:
                f.rmdir()
            except OSError:
                pass


def read(p):
    """Rows of a CSV file as dicts."""
    with open(p, newline="", encoding="utf-8") as fh:
        return list(csv.DictReader(fh))


def write(p, rows, cols):
    """Write rows (dicts) to a CSV file with the given column order."""
    p.parent.mkdir(parents=True, exist_ok=True)
    with open(p, "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, cols, extrasaction="ignore")
        w.writeheader()
        w.writerows(rows)


def classify(name, code, lib_strings):
    """Sony library of a function from its name (kernel stubs, libc), the hardware registers,
    kernel calls and library tags its decompiled C mentions; "" if nothing matches."""
    if name in KERNEL:
        return "libkernl"
    if name in LIBC:
        return "libc"
    for tag in lib_strings:
        return tag
    for lib, pats in LIB_RULES:
        if any(re.search(p, code) for p in pats):
            return lib
    return ""


def variant_rows():
    """Variants in extracted/variants.csv order. With T5_PARTIAL=1, variants that have not been
    exported yet are left out (for trying the pipeline while Ghidra still works)."""
    out = []
    for r in read(EX / "variants.csv"):
        if not (EXP / r["variant"] / "functions_raw.csv").exists():
            if os.environ.get("T5_PARTIAL") == "1":
                print(f"warning: {r['variant']} not exported yet, left out")
                continue
            raise SystemExit(f"{r['variant']}: no export in {EXP / r['variant']} (run tools/run_ghidra.sh)")
        out.append(dict(r))
    return out


def main():
    """Merge every variant's raw export into the sheets, keeping hand-edited columns, and project
    the C sources."""
    SHEETS.mkdir(parents=True, exist_ok=True)
    variants = variant_rows()
    prev = {}
    if (SHEETS / "functions.csv").exists():
        prev = {(r["variant"], r["addr"]): r for r in read(SHEETS / "functions.csv")}
    manual = {}
    if (SHEETS / "manual_names.csv").exists():
        manual = {(r["variant"], r["addr"].upper().replace("0X", "0x")): r for r in read(SHEETS / "manual_names.csv")}
    # Sony SDK functions identified by tools/sce_symbols.py (PS2Recomp's SCE symbol database).
    sce = {}
    if (SHEETS / "sce_symbols.csv").exists():
        sce = {(r["variant"], r["addr"]): r for r in read(SHEETS / "sce_symbols.csv")}

    rows, calls, strings, globs, instr_n = [], [], [], [], Counter()
    raw_root = SRC / "raw_c"
    if raw_root.exists():
        clear_dir(raw_root)
    for v in variants:
        name = v["variant"]
        exp = EXP / name
        funcs = sorted(read(exp / "functions_raw.csv"), key=lambda r: int(r["addr"], 16))
        shutil.copytree(exp / "raw_c", raw_root / name, dirs_exist_ok=True)
        code = {f["addr"]: (exp / "raw_c" / f"{f['addr'][2:]}.c").read_text(encoding="utf-8") for f in funcs}
        vstrings = read(exp / "strings.csv")
        tags = defaultdict(list)  # function -> library tags of the strings it references
        for s in vstrings:
            m = re.match(r"PsIIlib(\w+)", s["text"]) or re.search(r"\blib(pad|mc|cdvd|sdr|pkt|graph|dma|vu0|ipu|mpeg|kernl)\b", s["text"])
            if m:
                for f in filter(None, s["ref_funcs"].split(";")):
                    tags[f].append("lib" + m.group(1).replace("sdr", "sd").replace("mpeg", "ipu"))
        libs = {f["addr"]: classify(f["name"], code[f["addr"]], tags.get(f["addr"], [])) for f in funcs}
        # Neighbours: an unclassified function between two of the same library at most
        # NEIGHBOUR_GAP functions apart belongs to it (the linker keeps each library's objects
        # together; game code between two library calls of the game must stay game code).
        addrs = [f["addr"] for f in funcs]
        known = [(i, libs[a]) for i, a in enumerate(addrs) if libs[a]]
        ki = 0
        for i, a in enumerate(addrs):
            while ki < len(known) and known[ki][0] < i:
                ki += 1
            if libs[a]:
                continue
            p = next((l for j, l in reversed(known[:ki]) if j < i), "")
            n = known[ki][1] if ki < len(known) else ""
            pj = next((j for j, l in reversed(known[:ki]) if j < i), None)
            nj = known[ki][0] if ki < len(known) else None
            if p and p == n and nj - pj <= NEIGHBOUR_GAP:
                libs[a] = p.rstrip("?") + "?"
        for f in funcs:
            a = int(f["addr"], 16)
            key = (name, f["addr"])
            old = prev.get(key, {})
            man = manual.get(key)
            lib = libs[f["addr"]]
            sect = f["section"]
            if lib:
                module, unit = "sce", f"{name}/sce/{lib.rstrip('?')}"
                note = "library inferred from neighbours" if lib.endswith("?") else "library from hardware / kernel / string evidence"
            else:
                module = "game"
                seg = a // UNIT_BYTES * UNIT_BYTES
                unit = f"{name}/{sect.lstrip('.')}/seg_{seg:08X}"
                note = ""
            fname = f["name"]
            status = "raw" if fname.startswith(("FUN_", "LAB_", "caseD_", "thunk_FUN_")) else "named"
            summary = ""
            sym = sce.get(key)
            if sym and not man:
                module, unit = "sce", f"{name}/sce/{sym['library']}"
                fname, status, note = sym["name"], "named", "SCE symbol database match (PS2Recomp)"
                if old.get("name", "").startswith(("FUN_", "LAB_", "thunk_FUN_")):
                    old = {k: x for k, x in old.items() if k not in ("name", "status", "notes", "unit")}
            if man:
                fname, status, note = man["name"], man["status"], man["evidence"]
                summary = man.get("summary", "")
                old = {k: x for k, x in old.items() if k not in ("name", "status", "notes")}
            rows.append({
                **f, "variant": name,
                "name": old.get("name") or fname,
                "module": module,
                "unit": old.get("unit") or unit,
                "status": old.get("status") or status,
                "owner": old.get("owner", ""),
                "notes": old.get("notes") or note,
                "c_path": f"src/raw_c/{name}/{f['addr'][2:]}.c",
                "summary": summary,
            })
        calls += [{"variant": name, **r} for r in read(exp / "calls.csv")]
        strings += [{"variant": name, **r} for r in vstrings]
        globs += [{"variant": name, **r} for r in read(exp / "globals.csv")]

    fcols = ["variant", "addr", "name", "size", "section", "module", "unit", "status", "owner", "calls_out",
             "called_by", "decomp_lines", "decomp_ok", "signature", "summary", "c_path", "notes"]
    write(SHEETS / "functions.csv", rows, fcols)
    write(SHEETS / "calls.csv", calls, ["variant", "caller", "callee"])
    write(SHEETS / "strings.csv", strings, ["variant", "addr", "section", "len", "text", "ref_funcs"])
    write(SHEETS / "globals.csv", globs, ["variant", "addr", "name", "section", "datatype", "size", "xrefs"])

    # instructions/<variant>.csv and memory/<variant>.csv: per variant (each is a whole program).
    for sub in ("instructions", "memory"):
        (SHEETS / sub).mkdir(parents=True, exist_ok=True)
    secs = []
    for v in variants:
        name = v["variant"]
        with open(SHEETS / "instructions" / f"{name}.csv", "w", newline="", encoding="utf-8") as fh, \
                open(EXP / name / "instructions.csv", newline="", encoding="utf-8") as src:
            w = csv.writer(fh)
            r = csv.reader(src)
            next(r)
            w.writerow(["addr", "func", "bytes", "asm", "delay", "pcode"])
            for row in r:
                w.writerow(row)
                instr_n[name] += 1
        shutil.copy(EXP / name / "memory.csv", SHEETS / "memory" / f"{name}.csv")
        secs += [{"variant": name, **s} for s in read(EXP / name / "sections.csv")]
    write(SHEETS / "sections.csv", secs, ["variant", "name", "start", "end", "size", "perms", "initialized"])
    shutil.copy(EXP / variants[0]["variant"] / "registers.csv", SHEETS / "registers.csv")
    for name in ["disc_files.csv", "tk5data3_toc.csv"]:
        shutil.copy(EX / name, SHEETS / name)

    nf = Counter(r["variant"] for r in rows)
    for v in variants:
        v["functions"] = nf[v["variant"]]
        v["instructions"] = instr_n[v["variant"]]
        v["elf_sha1"] = v.pop("sha1", "")
    write(SHEETS / "variants.csv", variants,
          ["variant", "source", "title", "entry", "gp", "start", "end", "elf", "elf_sha1", "functions", "instructions"])

    by_unit = defaultdict(list)
    for r in rows:
        by_unit[(r["variant"], r["unit"])].append(r)
    units = []
    for (vname, u), rs in by_unit.items():
        st = Counter(r["status"] for r in rs)
        units.append({
            "unit": u, "variant": vname, "module": rs[0]["module"],
            "library": u.split("/")[2] if "/sce/" in u else "",
            "first_addr": rs[0]["addr"], "last_addr": rs[-1]["addr"],
            "n_funcs": len(rs), "total_size": sum(int(r["size"]) for r in rs),
            **{s: st.get(s, 0) for s in STATUSES},
            "owner": "", "src_path": f"src/units/{u.replace('/', '__')}.c",
        })
    order = {v["variant"]: i for i, v in enumerate(variants)}
    units.sort(key=lambda r: (order[r["variant"]], r["module"] != "game", int(r["first_addr"], 16)))
    write(SHEETS / "units.csv", units, ["unit", "variant", "module", "library", "first_addr", "last_addr",
                                        "n_funcs", "total_size", *STATUSES, "owner", "src_path"])

    # Projection: each unit's functions concatenated into one C file.
    units_dir = SRC / "units"
    if units_dir.exists():
        clear_dir(units_dir)
    units_dir.mkdir(parents=True, exist_ok=True)
    for (vname, u), rs in by_unit.items():
        with open(units_dir / f"{u.replace('/', '__')}.c", "a", encoding="utf-8") as fh:
            fh.write(f"// unit {u} ({vname}, {rs[0]['module']}): {len(rs)} functions {rs[0]['addr']}..{rs[-1]['addr']}\n"
                     f"// GENERATED from sheets/functions.csv - edit the sheet, not this file.\n\n")
            for r in rs:
                if r["summary"]:
                    fh.write("/* " + "\n * ".join(textwrap.wrap(r["summary"], 94)) + " */\n")
                fh.write((OUT / r["c_path"]).read_text(encoding="utf-8"))
                fh.write("\n")

    files = EX / "files"
    slus = (files / "SLUS_210.59").read_bytes()
    cnf = (files / "SYSTEM.CNF").read_text(errors="replace")
    meta = [
        ("game_id", "SLUS-21059"),
        ("title", "Tekken 5 (USA)"),
        ("platform", "Sony PlayStation 2 (Emotion Engine: MIPS R5900 + VU0/VU1, little endian; IOP: R3000A)"),
        ("engine", "Namco game code (C++) + Sony EE libraries (libgraph, libdma, libvu0, libpad, libmc, "
                   "libcdvd, libkernl, libipu/libmpeg, libpkt, libsdr) + Namco nuSound"),
        ("boot", "SYSTEM.CNF BOOT2 = cdrom0:\\SLUS_210.59;1"),
        ("entry_point", "0x00100008 (_start: SetupThread, SetupHeap, boot loader 0x00D060D0, main 0x001F0790)"),
        ("main_gp", next(v["gp"] for v in variants if v["variant"] == "main") if any(v["variant"] == "main" for v in variants) else ""),
        ("boot_loader", "0x00D060D0 (.y4): logo, IOP reset with IOPRP300.IMG, TK5DATA3.BIN entries 5 -> 0x001F0180 "
                        "and 6 -> 0x00488000 (LZ, 0x00D06010)"),
        ("tk5data3", "u32 count, count x {u32 offset, u32 size}; entries 0..6 code (LZ), 7..9 data archives"),
        ("heap", "0x0072D600..0x01BCA460 (SetupHeap end), .y4 boot code at 0x00D05700 inside it"),
        ("programs", "; ".join(f"{v['variant']} = {v['title']}" for v in variants)),
        ("system_cnf", " ".join(cnf.split())),
        ("slus_sha1", hashlib.sha1(slus).hexdigest()),
        ("slus_size", str(len(slus))),
        ("symbols", "none shipped; kernel syscall stubs named by Ghidra's EE analyzer"),
        ("toolchain", "Ghidra 12.1.4 + ghidra-emotionengine-reloaded 2.1.38 (r5900:LE:32:default) via "
                      "ghidra-cli; ELFs from tools/make_elfs.py"),
    ]
    write(SHEETS / "meta.csv", [{"key": k, "value": v} for k, v in meta], ["key", "value"])

    tot = Counter(r["status"] for r in rows)
    lib = Counter(r["unit"].split("/sce/")[1] for r in rows if r["module"] == "sce")
    print(f"{len(rows)} functions in {len(variants)} variants, {sum(instr_n.values())} instructions, "
          f"{len(units)} units, status {dict(tot)}")
    print("per variant:", {v["variant"]: (v["functions"], v["instructions"]) for v in variants})
    print("libraries:", dict(lib))


# IOP kernel library functions by import index (Sony IOP kernel, as ps2sdk documents them).
IOP_NAMES = {
    "sysmem": {4: "AllocSysMemory", 5: "FreeSysMemory", 6: "QueryMemSize", 7: "QueryMaxFreeMemSize",
               8: "QueryTotalFreeMemSize", 9: "QueryBlockTopAddress", 10: "QueryBlockSize", 14: "Kprintf"},
    "loadcore": {3: "GetLibraryEntryTable", 4: "FlushIcache", 5: "FlushDcache", 6: "RegisterLibraryEntries",
                 7: "ReleaseLibraryEntries"},
    "intrman": {4: "RegisterIntrHandler", 5: "ReleaseIntrHandler", 6: "EnableIntr", 7: "DisableIntr",
                8: "CpuDisableIntr", 9: "CpuEnableIntr", 17: "CpuSuspendIntr", 18: "CpuResumeIntr",
                23: "QueryIntrContext"},
    "thbase": {4: "CreateThread", 5: "DeleteThread", 6: "StartThread", 7: "StartThreadArgs", 8: "ExitThread",
               9: "ExitDeleteThread", 10: "TerminateThread", 11: "iTerminateThread", 12: "DisableDispatchThread",
               13: "EnableDispatchThread", 14: "ChangeThreadPriority", 15: "iChangeThreadPriority",
               16: "RotateThreadReadyQueue", 17: "iRotateThreadReadyQueue", 18: "ReleaseWaitThread",
               19: "iReleaseWaitThread", 20: "GetThreadId", 21: "CheckThreadStack", 22: "ReferThreadStatus",
               23: "iReferThreadStatus", 24: "SleepThread", 25: "WakeupThread", 26: "iWakeupThread",
               27: "CancelWakeupThread", 28: "iCancelWakeupThread", 29: "SuspendThread", 30: "iSuspendThread",
               31: "ResumeThread", 32: "iResumeThread", 33: "DelayThread", 34: "GetSystemTime", 35: "SetAlarm",
               36: "iSetAlarm", 37: "CancelAlarm", 38: "iCancelAlarm", 39: "USec2SysClock",
               40: "SysClock2USec", 41: "GetSystemStatusFlag", 42: "GetThreadCurrentPriority",
               43: "GetSystemTimeLow", 44: "ReferSystemStatus", 45: "ReferThreadRunStatus",
               46: "GetThreadStackFreeSize", 47: "GetThreadmanIdList"},
    "thsemap": {4: "CreateSema", 5: "DeleteSema", 6: "SignalSema", 7: "iSignalSema", 8: "WaitSema",
                9: "PollSema", 10: "iPollSema", 11: "ReferSemaStatus", 12: "iReferSemaStatus"},
    "thevent": {4: "CreateEventFlag", 5: "DeleteEventFlag", 6: "SetEventFlag", 7: "iSetEventFlag",
                8: "ClearEventFlag", 9: "iClearEventFlag", 10: "WaitEventFlag", 11: "PollEventFlag",
                12: "iPollEventFlag", 13: "ReferEventFlagStatus"},
    "thfpool": {4: "CreateFpl", 5: "DeleteFpl", 6: "AllocateFpl", 7: "pAllocateFpl", 8: "ipAllocateFpl", 9: "FreeFpl"},
    "thvpool": {4: "CreateVpl", 5: "DeleteVpl", 6: "AllocateVpl", 7: "pAllocateVpl", 8: "ipAllocateVpl", 9: "FreeVpl"},
    "timrman": {4: "AllocHardTimer", 5: "ReferHardTimer", 6: "FreeHardTimer", 7: "SetTimerMode",
                8: "GetTimerStatus", 9: "SetTimerCounter", 10: "GetTimerCounter", 11: "SetTimerCompare",
                12: "GetTimerCompare", 13: "SetHoldMode", 14: "GetHoldMode", 15: "GetHoldReg",
                16: "GetHardTimerIntrCode", 20: "SetTimerHandler", 21: "SetOverflowHandler",
                22: "SetupHardTimer", 23: "StartHardTimer", 24: "StopHardTimer"},
    "sifman": {4: "sceSifDma2Init", 5: "sceSifInit", 6: "sceSifSetDChain", 7: "sceSifSetDma", 8: "sceSifDmaStat",
               29: "sceSifCheckInit", 32: "sceSifSetDmaIntr"},
    "sifcmd": {4: "sceSifInitCmd", 5: "sceSifExitCmd", 6: "sceSifGetSreg", 7: "sceSifSetSreg",
               8: "sceSifSetCmdBuffer", 9: "sceSifSetSysCmdBuffer", 10: "sceSifAddCmdHandler",
               11: "sceSifRemoveCmdHandler", 12: "sceSifSendCmd", 13: "isceSifSendCmd", 14: "sceSifInitRpc",
               15: "sceSifBindRpc", 16: "sceSifCallRpc", 17: "sceSifRegisterRpc", 18: "sceSifCheckStatRpc",
               19: "sceSifSetRpcQueue", 20: "sceSifGetNextRequest", 21: "sceSifExecRequest",
               22: "sceSifRpcLoop", 23: "sceSifGetOtherData", 24: "sceSifRemoveRpc", 25: "sceSifRemoveRpcQueue"},
    "sysclib": {4: "setjmp", 5: "longjmp", 6: "toupper", 7: "tolower", 8: "look_ctype_table",
                9: "get_ctype_table", 10: "memchr", 11: "memcmp", 12: "memcpy", 13: "memmove", 14: "memset",
                15: "bcmp", 16: "bcopy", 17: "bzero", 18: "prnt", 19: "sprintf", 20: "strcat", 21: "strchr",
                22: "strcmp", 23: "strcpy", 24: "strcspn", 25: "index", 26: "rindex", 27: "strlen",
                28: "strncat", 29: "strncmp", 30: "strncpy", 31: "strpbrk", 32: "strrchr", 33: "strspn",
                34: "strstr", 35: "strtok", 36: "strtol", 37: "atob", 38: "strtoul", 40: "wmemcopy",
                41: "wmemset", 42: "vsprintf"},
    "stdio": {4: "printf", 5: "getchar", 6: "putchar", 7: "puts", 8: "gets", 9: "fdprintf"},
    "ioman": {4: "open", 5: "close", 6: "read", 7: "write", 8: "lseek", 9: "ioctl", 10: "remove", 11: "mkdir",
              12: "rmdir", 13: "dopen", 14: "dclose", 15: "dread", 16: "getstat", 17: "chstat", 18: "format",
              20: "AddDrv", 21: "DelDrv"},
    "cdvdman": {4: "sceCdInit", 5: "sceCdStandby", 6: "sceCdRead", 7: "sceCdSeek", 8: "sceCdGetError",
                9: "sceCdGetToc", 10: "sceCdSearchFile", 11: "sceCdSync", 12: "sceCdGetDiskType",
                13: "sceCdDiskReady", 14: "sceCdTrayReq", 15: "sceCdStop", 16: "sceCdPosToInt",
                17: "sceCdIntToPos", 21: "sceCdCheckCmd", 22: "sceCdReadILinkID", 24: "sceCdReadClock",
                28: "sceCdStatus"},
}


def build_iop():
    """sheets/iop/: the IOP modules the game runs (tools/iop_modules.py, IOP=1 run_ghidra.sh)."""
    exp, out, src = WORK / "export_iop", SHEETS / "iop", SRC / "iop"
    out.mkdir(parents=True, exist_ok=True)
    mods = [r for r in read(EX / "iop_modules.csv") if (exp / r["module"] / "functions_raw.csv").exists()]
    imports = read(EX / "iop_imports.csv")
    stubs = {(r["module"], r["stub"]): r for r in imports}
    raw_root = src / "raw_c"
    if raw_root.exists():
        clear_dir(raw_root)
    rows, calls, strings = [], [], []
    for mrow in mods:
        name = mrow["module"]
        e = exp / name
        shutil.copytree(e / "raw_c", raw_root / name, dirs_exist_ok=True)
        for f in read(e / "functions_raw.csv"):
            st = stubs.get((name, f["addr"]))
            fname = f["name"]
            status = "raw" if fname.startswith(("FUN_", "LAB_", "thunk_")) else "named"
            note = ""
            if st:
                lib, idx = st["library"], int(st["index"])
                fname = f"{lib}_{IOP_NAMES.get(lib, {}).get(idx, idx)}"
                status, note = "named", f"import stub: {lib} #{idx}"
            rows.append({**f, "module": name, "name": fname, "status": status, "notes": note,
                         "c_path": f"src/iop/raw_c/{name}/{f['addr'][2:]}.c"})
        calls += [{"module": name, **r} for r in read(e / "calls.csv")]
        strings += [{"module": name, **r} for r in read(e / "strings.csv")]
        for sub in ("instructions", "memory"):
            (out / sub).mkdir(parents=True, exist_ok=True)
        shutil.copy(e / "instructions.csv", out / "instructions" / f"{name}.csv")
        shutil.copy(e / "memory.csv", out / "memory" / f"{name}.csv")
        mrow["functions"] = sum(1 for r in rows if r["module"] == name)
    write(out / "modules.csv", mods, list(mods[0].keys()))
    write(out / "functions.csv", rows, ["module", "addr", "name", "size", "status", "calls_out", "called_by",
                                        "decomp_lines", "decomp_ok", "signature", "c_path", "notes"])
    write(out / "calls.csv", calls, ["module", "caller", "callee"])
    write(out / "strings.csv", strings, ["module", "addr", "section", "len", "text", "ref_funcs"])
    for r in imports:
        r["name"] = IOP_NAMES.get(r["library"], {}).get(int(r["index"]), "")
    write(out / "imports.csv", imports, ["module", "stub", "library", "index", "name"])
    shutil.copy(EX / "iop_exports.csv", out / "exports.csv")
    shutil.copy(exp / mods[0]["module"] / "registers.csv", out / "registers.csv")
    print(f"IOP: {len(mods)} modules, {len(rows)} functions")


if __name__ == "__main__":
    import sys
    if "--iop" in sys.argv:
        build_iop()
    else:
        main()
