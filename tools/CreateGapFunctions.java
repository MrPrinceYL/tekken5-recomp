// Create functions for code Ghidra's auto-analysis skipped (callbacks, state handlers and C++
// virtual methods that are only reached through pointers). Tekken 5's code images hold data
// tables too (TK5DATA3 entry 5 is text and data in one image), so a gap address in an executable
// block only becomes a function when there is evidence it is code:
//   - it is a stack-frame prologue (addiu sp,sp,-N), or
//   - its address is stored as a 32-bit word somewhere in the program image (a function pointer,
//     e.g. a vtable slot)
// and the result decodes cleanly and ends in a jump/return. Repeats until nothing new is found.
// Usage: ghidra script run CreateGapFunctions.java
//@category Analysis
import ghidra.app.cmd.disassemble.DisassembleCommand;
import ghidra.app.cmd.function.CreateFunctionCmd;
import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.*;
import ghidra.program.model.listing.*;
import ghidra.program.model.mem.*;

import java.util.*;

public class CreateGapFunctions extends GhidraScript {
    private boolean sane(Function f) throws Exception {
        Listing listing = currentProgram.getListing();
        boolean ends = false;
        long n = 0;
        for (Instruction ins : listing.getInstructions(f.getBody(), true)) {
            n++;
            if (ins.getFlowType().isTerminal() || ins.getFlowType().isJump() && !ins.getFlowType().isConditional())
                ends = true;
        }
        return ends && n > 0 && n * 4 >= f.getBody().getNumAddresses();
    }

    static Set<Long> storedPointers(Memory mem, AddressSetView code) throws Exception {
        Set<Long> ptrs = new HashSet<>();
        for (MemoryBlock b : mem.getBlocks()) {
            if (!b.isInitialized() || !b.isLoaded()) continue;
            byte[] buf = new byte[(int) b.getSize()];
            b.getBytes(b.getStart(), buf);
            for (int i = 0; i + 4 <= buf.length; i += 4) {
                long v = (buf[i] & 0xFFL) | (buf[i + 1] & 0xFFL) << 8 | (buf[i + 2] & 0xFFL) << 16 | (buf[i + 3] & 0xFFL) << 24;
                if ((v & 3) == 0 && v != 0 && code.contains(b.getStart().getNewAddress(v))) ptrs.add(v);
            }
        }
        return ptrs;
    }

    static AddressSet codeBlocks(Memory mem) {
        AddressSet code = new AddressSet();
        for (MemoryBlock b : mem.getBlocks())
            if (b.isExecute() && b.isInitialized() && b.isLoaded()) code.add(b.getStart(), b.getEnd());
        return code;
    }

    @Override
    protected void run() throws Exception {
        Memory mem = currentProgram.getMemory();
        FunctionManager fm = currentProgram.getFunctionManager();
        Listing listing = currentProgram.getListing();
        AddressSet code = codeBlocks(mem);
        Set<Long> ptrs = storedPointers(mem, code);
        int created = 0, rejected = 0, pass = 0;
        boolean changed = true;
        while (changed && pass < 8) {
            changed = false;
            pass++;
            for (AddressRange range : code) {
                Address a = range.getMinAddress();
                Address end = range.getMaxAddress();
                while (a.compareTo(end) < 0 && !monitor.isCancelled()) {
                    Function f = fm.getFunctionContaining(a);
                    if (f != null) {
                        Address next = f.getBody().getMaxAddress().add(1);
                        a = next.compareTo(a) > 0 ? next.add((4 - next.getOffset() % 4) % 4) : a.add(4);
                        continue;
                    }
                    if (a.add(3).compareTo(end) > 0) break;
                    int w = mem.getInt(a);
                    boolean prologue = (w >>> 16) == 0x27BD && (w & 0x8000) != 0;
                    if (w == 0 || !(prologue || ptrs.contains(a.getOffset()))) {
                        a = a.add(4);
                        continue;
                    }
                    if (listing.getDefinedDataAt(a) != null && !prologue) {
                        a = a.add(4);  // a pointer to typed data (string, table) is not code
                        continue;
                    }
                    AddressSetView fresh = new AddressSet();
                    if (listing.getInstructionAt(a) == null) {
                        if (listing.getDefinedDataAt(a) != null) listing.clearCodeUnits(a, a, false);
                        DisassembleCommand dis = new DisassembleCommand(a, null, true);
                        dis.applyTo(currentProgram, monitor);
                        fresh = dis.getDisassembledAddressSet();
                    }
                    if (listing.getInstructionAt(a) != null && new CreateFunctionCmd(a).applyTo(currentProgram, monitor)) {
                        Function nf = fm.getFunctionAt(a);
                        if (nf != null && sane(nf)) {
                            created++;
                            changed = true;
                            Address next = nf.getBody().getMaxAddress().add(1);
                            a = next.compareTo(a) > 0 ? next.add((4 - next.getOffset() % 4) % 4) : a.add(4);
                            continue;
                        }
                        if (nf != null) fm.removeFunction(a);
                        rejected++;
                    }
                    // Not code after all: undo what this attempt disassembled.
                    for (AddressRange r : fresh) listing.clearCodeUnits(r.getMinAddress(), r.getMaxAddress(), false, monitor);
                    a = a.add(4);
                }
            }
            println("pass " + pass + ": " + created + " functions created, " + rejected + " rejected");
        }
        println("done: created " + created + " functions, total " + fm.getFunctionCount());
    }
}
