"""Critical path per game logic frame from a `T5_TIMELINE=1` run log.

    python tools/timeline.py ../Tekken5/run/tl.log [seconds-from-end]

Events (diag::tl): "EE logic" (game frame counter 0x151F90 changed), "EE idle" / "EE wake"
(guest thread parked / resumed), "EE vblank", "EE dma vif1|gif", "W busy" / "W idle" (MTVU
worker), "W gswait" / "W gsdone", "G busy" / "G idle" (GS thread), "R flush" (raster batch
dispatched), "R wait" / "R waited" (GS thread waits for the raster workers), "C new" / "C same"
(copy pass onto the display buffer from a redrawn / unchanged source).

Per logic frame: wall time, busy time of EE / worker / GS, raster flushes and waits, vblanks.
"""
import re
import sys
from collections import Counter


def spans(ev, a, b, on, off, start_on=False):
    """Total time inside [a, b) between `on` and `off` events."""
    total, t0 = 0, (a if start_on else None)
    for t, e in ev:
        if e == on and t0 is None:
            t0 = t
        elif e == off and t0 is not None:
            total += min(t, b) - max(t0, a)
            t0 = None
    if t0 is not None:
        total += b - max(t0, a)
    return total


def main():
    path = sys.argv[1]
    window = float(sys.argv[2]) if len(sys.argv) > 2 else 10.0
    ev = []
    for line in open(path, errors="replace"):
        m = re.search(r"TL (\d+) (.*)", line)
        if m:
            ev.append((int(m.group(1)), m.group(2).strip()))
    end = ev[-1][0]
    ev = [e for e in ev if e[0] > end - window * 1e6]
    ticks = [t for t, e in ev if e == "EE logic"]
    rows = []
    # Per-thread on/off state at each frame start comes from scanning the whole window.
    for a, b in zip(ticks, ticks[1:]):
        seg = [(t, e) for t, e in ev if a <= t < b]
        prev = [(t, e) for t, e in ev if t < a]

        def state(on, off):
            for t, e in reversed(prev):
                if e == on:
                    return True
                if e == off:
                    return False
            return False

        ee_idle = spans(seg, a, b, "EE idle", "EE wake", state("EE idle", "EE wake"))
        w = spans(seg, a, b, "W busy", "W idle", state("W busy", "W idle"))
        wgs = spans(seg, a, b, "W gswait", "W gsdone", state("W gswait", "W gsdone"))
        g = spans(seg, a, b, "G busy", "G idle", state("G busy", "G idle"))
        rw = spans(seg, a, b, "R wait", "R waited", state("R wait", "R waited"))
        c = Counter(e for _, e in seg)
        rows.append(dict(dur=(b - a) / 1e3, ee=(b - a - ee_idle) / 1e3, w=w / 1e3, wgs=wgs / 1e3, g=g / 1e3,
                         rw=rw / 1e3, fl=c["R flush"], vb=c["EE vblank"], vif=c["EE dma vif1"], cnew=c["C new"], csame=c["C same"]))
    if not rows:
        print("no logic frames in the window")
        return
    n = len(rows)
    avg = {k: sum(r[k] for r in rows) / n for k in rows[0]}
    print(f"{n} logic frames in {window:.0f} s = {n / window:.1f} fps")
    print("per frame (avg ms): wall {dur:.1f} | EE busy {ee:.1f} | worker busy {w:.1f} (GS wait {wgs:.1f}) | "
          "GS busy {g:.1f} (raster wait {rw:.1f}) | flushes {fl:.1f} | vblanks {vb:.2f} | vif1 kicks {vif:.1f} | "
          "copy new {cnew:.2f} same {csame:.2f}".format(**avg))
    budget = 1000 / 60
    print(f"over the 16.7 ms budget: worker {max(0, avg['w'] - budget):.1f} ms, GS {max(0, avg['g'] - budget):.1f} ms, "
          f"EE {max(0, avg['ee'] - budget):.1f} ms")
    hist = Counter(round(r["dur"] / budget) for r in rows)
    print("frame length in fields:", dict(sorted(hist.items())))
    print("last frames:")
    for r in rows[-8:]:
        print("  wall {dur:5.1f} EE {ee:5.1f} W {w:5.1f} (gs {wgs:4.1f}) G {g:5.1f} (rw {rw:4.1f}) fl {fl:3d} vb {vb} vif {vif} C+{cnew} C={csame}".format(**r))


if __name__ == "__main__":
    main()
