// Disassemble code that is only reached through addresses stored in memory: switch jump tables
// Ghidra could not resolve and callbacks inside code it doesn't attribute to a function.
// Every aligned 32-bit word of the initialized image whose value lies in an executable block and
// points at undefined, non-zero bytes right next to known code is disassembled (following its
// flow). gen_rust.py attributes such code to the enclosing function and makes every stored code
// address a block leader.
// Usage: ghidra script run DisassemblePointerTargets.java
//@category Analysis
import ghidra.app.cmd.disassemble.DisassembleCommand;
import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.*;
import ghidra.program.model.listing.*;
import ghidra.program.model.mem.*;

import java.util.*;

public class DisassemblePointerTargets extends GhidraScript {
    @Override
    protected void run() throws Exception {
        Memory mem = currentProgram.getMemory();
        Listing listing = currentProgram.getListing();
        AddressSet code = new AddressSet();
        for (MemoryBlock b : mem.getBlocks())
            if (b.isExecute() && b.isInitialized() && b.isLoaded()) code.add(b.getStart(), b.getEnd());
        AddressSpace sp = currentProgram.getAddressFactory().getDefaultAddressSpace();
        TreeSet<Long> targets = new TreeSet<>();
        for (MemoryBlock b : mem.getBlocks()) {
            if (!b.isInitialized() || !b.isLoaded()) continue;
            byte[] buf = new byte[(int) b.getSize()];
            b.getBytes(b.getStart(), buf);
            for (int i = 0; i + 4 <= buf.length; i += 4) {
                long v = (buf[i] & 0xFFL) | (buf[i + 1] & 0xFFL) << 8 | (buf[i + 2] & 0xFFL) << 16 | (buf[i + 3] & 0xFFL) << 24;
                if ((v & 3) == 0 && v != 0 && code.contains(sp.getAddress(v))) targets.add(v);
            }
        }
        // Only targets adjacent to known code (case blocks follow their switch's `jr` or another
        // case): a stored address of data inside a code image must not become code. Repeat: a
        // newly found block makes the next one adjacent.
        int done = 0;
        long bytes = 0;
        boolean changed = true;
        int pass = 0;
        while (changed && !monitor.isCancelled() && pass++ < 16) {
            changed = false;
            for (long t : targets) {
                Address a = sp.getAddress(t);
                if (!mem.contains(a) || listing.getInstructionAt(a) != null) continue;
                if (listing.getDefinedDataAt(a) != null || mem.getInt(a) == 0) continue;
                if (listing.getInstructionContaining(a) != null) continue;  // inside an instruction
                boolean adjacent = listing.getInstructionAt(a.subtract(4)) != null
                    || listing.getInstructionAt(a.subtract(8)) != null
                    || listing.getInstructionAt(a.add(4)) != null;
                if (!adjacent) continue;
                DisassembleCommand dis = new DisassembleCommand(a, null, true);
                dis.applyTo(currentProgram, monitor);
                long n = dis.getDisassembledAddressSet().getNumAddresses();
                if (n > 0) {
                    done++;
                    bytes += n;
                    changed = true;
                }
            }
        }
        println("done: " + targets.size() + " stored code addresses, " + done + " disassembled (" + bytes + " bytes)");
    }
}
