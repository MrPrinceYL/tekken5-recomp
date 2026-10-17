"""VU1 static compiler (microVU principle) for tools/gen_vu.py.

A translated block (sheets/vu_blocks.csv) runs straight from its start to its branch / E-bit
delay slot. The pair-by-pair kernel (`ps2_runtime::vu::pair_lean`) evaluates the pipeline
model (stalls, FMAC flag entries, FDIV / EFU, IALU, VI backup) at run time. Its decisions depend
only on the instruction words and on the timing state the block is entered with (the "shape":
`Pipe::shape` / `Pipe::shape_key`). For each shape seen at run time (sheet column `shapes`) this
module evaluates the same model here, once, and emits straight-line Rust that only computes
values: registers in locals, flags / Q / P resolved to the values the model would make
visible, the timing state the block leaves written back at its end. The entry checks the shape
key; any other shape runs the pair-by-pair kernel. Bit-exactness is checked by `vu-bench`
against captured runs.

Mirrors (keep in step): ps2_runtime::vu::{pair_lean, Pipe::{stalls, retire, fmac_write,
apply_fmac, canon, shape_key}, upper_info, lower_info, needs_retire, backup_reg, lower_op,
lower_special} and ps2_runtime::vuc.
"""

VI_STATUS, VI_MAC, VI_CLIP, VI_R, VI_I, VI_Q, VI_P = 16, 17, 18, 20, 21, 22, 23
P_NONE, P_FMAC, P_FDIV, P_EFU, P_IALU, P_BRANCH = 0, 1, 2, 3, 4, 5
W_STATUS, W_CLIP, W_Q, W_P = 1 << 16, 1 << 18, 1 << 22, 1 << 23
ICOUNT = 0x800  # VU1 micro memory: 2048 pairs


class Unsupported(Exception):
    pass


class Info:
    __slots__ = ("pipe", "vfw", "vfw_m", "r0", "r0_m", "r1", "r1_m", "viw", "vir", "cycles")

    def __init__(self, pipe=P_NONE, vfw=0, vfw_m=0, r0=0, r0_m=0, r1=0, r1_m=0, viw=0, vir=0, cycles=0):
        self.pipe, self.vfw, self.vfw_m = pipe, vfw, vfw_m
        self.r0, self.r0_m, self.r1, self.r1_m = r0, r0_m, r1, r1_m
        self.viw, self.vir, self.cycles = viw, vir, cycles


def xyzw(w):
    return (w >> 21) & 0xF


def upper_info(w):
    fd, fs, ft = (w >> 6) & 31, (w >> 11) & 31, (w >> 16) & 31
    m = xyzw(w)
    bcm = 8 >> (w & 3)

    def fmac(vfw, r0, r0_m, r1, r1_m, viw):
        return Info(P_FMAC, vfw, m, r0, r0_m, r1, r1_m, viw)
    funct = w & 0x3F
    if funct < 0x3C:
        if funct <= 0x1B:
            return fmac(fd, fs, m, ft, bcm, 0)
        if funct <= 0x27:
            return fmac(fd, fs, m, 0, 0, 0)
        if 0x28 <= funct <= 0x2D or funct == 0x2F:
            return fmac(fd, fs, m, ft, m, 0)
        if funct == 0x2E:
            i = fmac(fd, fs, 0xE, ft, 0xE, 0)
            i.vfw_m = 0xE
            return i
        return Info()
    op = (funct & 3) | ((w >> 6) & 31) << 2
    if op <= 0x0F or 0x18 <= op <= 0x1B:
        return fmac(0, fs, m, ft, bcm, 0)
    if 0x10 <= op <= 0x17 or op == 0x1D:
        return fmac(ft, fs, m, 0, 0, 0)
    if op in (0x1C, 0x1E) or 0x20 <= op <= 0x27:
        return fmac(0, fs, m, 0, 0, 0)
    if op == 0x1F:
        return fmac(0, fs, 0xE, ft, 0x1, W_CLIP)
    if 0x28 <= op <= 0x2A or op in (0x2C, 0x2D):
        return fmac(0, fs, m, ft, m, 0)
    if op == 0x2E:
        return fmac(0, fs, 0xE, ft, 0xE, 0)
    return Info()


def lower_info(w):
    ft, fs = (w >> 16) & 31, (w >> 11) & 31
    it, is_, id_ = 1 << (ft & 15), 1 << (fs & 15), 1 << (((w >> 6) & 31) & 15)
    m = xyzw(w)
    fsm = 1 << (3 - ((w >> 21) & 3))
    ftm = 1 << (3 - ((w >> 23) & 3))
    op = w >> 25
    I = Info
    if op == 0x00:
        return I(P_FMAC, vfw=ft, vfw_m=m, vir=is_)
    if op == 0x01:
        return I(P_FMAC, r0=fs, r0_m=m, vir=it)
    if op == 0x04:
        return I(P_IALU, viw=it, vir=is_, cycles=4)
    if op == 0x05:
        return I(P_IALU, vir=is_ | it)
    if op in (0x08, 0x09):
        return I(P_IALU, viw=it, vir=is_)
    if op in (0x10, 0x12, 0x13):
        return I(P_FMAC, viw=1 << 1)
    if op == 0x11:
        return I(P_FMAC, viw=W_CLIP)
    if op == 0x15:
        return I(P_FMAC, viw=W_STATUS)
    if op in (0x14, 0x16, 0x17, 0x1C):
        return I(P_FMAC, viw=it)
    if op in (0x18, 0x1A, 0x1B):
        return I(P_FMAC, viw=it, vir=is_)
    if op == 0x20:
        return I(P_BRANCH)
    if op == 0x21:
        return I(P_BRANCH, viw=it)
    if op == 0x24:
        return I(P_BRANCH, vir=is_)
    if op == 0x25:
        return I(P_BRANCH, viw=it, vir=is_)
    if op in (0x28, 0x29):
        return I(P_BRANCH, vir=is_ | it)
    if 0x2C <= op <= 0x2F:
        return I(P_BRANCH, vir=is_)
    if op == 0x40:
        funct = w & 0x3F
        if funct in (0x30, 0x31, 0x34, 0x35):
            return I(P_IALU, viw=id_, vir=is_ | it)
        if funct == 0x32:
            return I(P_IALU, viw=it, vir=is_)
        if funct < 0x3C:
            return I()
        sop = (funct & 3) | ((w >> 6) & 31) << 2
        if sop == 0x30:
            return I(P_NONE if ft == 0 else P_FMAC, vfw=ft, vfw_m=m, r0=fs, r0_m=m)
        if sop == 0x31:
            return I(P_FMAC, vfw=ft, vfw_m=m, r0=fs, r0_m=(m >> 1) | ((m << 3) & 8))
        if sop in (0x34, 0x36):
            return I(P_FMAC, vfw=ft, vfw_m=m, viw=is_, vir=is_)
        if sop in (0x35, 0x37):
            return I(P_FMAC, r0=fs, r0_m=m, viw=it, vir=it)
        if sop == 0x38:
            return I(P_FDIV, r0=fs, r0_m=fsm, r1=ft, r1_m=ftm, viw=W_Q, cycles=7)
        if sop == 0x39:
            return I(P_FDIV, r1=ft, r1_m=ftm, viw=W_Q, cycles=7)
        if sop == 0x3A:
            return I(P_FDIV, r0=fs, r0_m=fsm, r1=ft, r1_m=ftm, viw=W_Q, cycles=13)
        if sop == 0x3B:
            return I(P_FDIV)
        if sop == 0x3C:
            return I(P_FMAC, r0=fs, r0_m=fsm, viw=it)
        if sop in (0x3D, 0x40, 0x41, 0x64):
            return I(P_FMAC, vfw=ft, vfw_m=m, vir=is_)
        if sop == 0x3E:
            return I(P_IALU, viw=it, vir=is_, cycles=4)
        if sop == 0x3F:
            return I(P_IALU, vir=is_ | it)
        if sop in (0x42, 0x43):
            return I(P_FMAC, r0=fs, r0_m=fsm)
        if sop in (0x68, 0x69):
            return I(P_IALU, viw=it)
        lat = {0x70: 11, 0x71: 18, 0x72: 18, 0x73: 24, 0x74: 54, 0x75: 54, 0x76: 12}
        if sop in lat:
            return I(P_EFU, r0=fs, r0_m=m, viw=W_P, cycles=lat[sop])
        lat2 = {0x78: 12, 0x7A: 12, 0x79: 18, 0x7C: 29, 0x7D: 54, 0x7E: 44}
        if sop in lat2:
            return I(P_EFU, r0=fs, r0_m=fsm, viw=W_P, cycles=lat2[sop])
        if sop == 0x7B:
            return I(P_EFU)
        return I()
    return I()


def needs_retire(lower, upper):
    funct = upper & 0x3F
    uq = funct in (0x1C, 0x20, 0x21, 0x24, 0x25) if funct < 0x3C else \
        ((funct & 3) | ((upper >> 6) & 31) << 2) in (0x1C, 0x20, 0x21, 0x24, 0x25)
    if uq:
        return True
    if upper & 0x80000000:
        return False
    op = lower >> 25
    if op in (0x10, 0x12, 0x13, 0x14, 0x16, 0x17, 0x18, 0x1A, 0x1B, 0x1C):
        return True
    if op == 0x40 and lower & 0x3F >= 0x3C:
        sop = (lower & 3) | ((lower >> 6) & 31) << 2
        return 0x38 <= sop <= 0x3A or sop == 0x64 or 0x70 <= sop <= 0x7E
    return False


def backup_reg(w):
    it, is_, id_ = (w >> 16) & 15, (w >> 11) & 15, (w >> 6) & 15
    op = w >> 25
    if op in (0x08, 0x09):
        return it if it else None
    if op == 0x40:
        f = w & 0x3F
        if f in (0x30, 0x31, 0x34, 0x35):
            return id_ if id_ else None
        if f == 0x32:
            return it if it else None
        if f >= 0x3C:
            sop = (f & 3) | ((w >> 6) & 31) << 2
            if sop == 0x3C:
                return it
            if sop in (0x34, 0x36):
                return is_
            if sop in (0x35, 0x37):
                return it
    return None


def is_branch(lo, up):
    return up >> 31 == 0 and (lo >> 25) in (0x20, 0x21, 0x24, 0x25, 0x28, 0x29, 0x2C, 0x2D, 0x2E, 0x2F)


def fnv(bs):
    h = 0xcbf29ce484222325
    for b in bs:
        h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def shape_key(shape):
    """As Pipe::shape_key, from the text of Pipe::shape."""
    M = 0xFFFFFFFFFFFFFFFF
    h = 0x9E3779B97F4A7C15

    def m(x):
        nonlocal h
        v = ((h ^ x) * 0xFF51AFD7ED558CCD) & M
        h = ((v << 23) | (v >> 41)) & M
    for tok in shape.split():
        k, rest = tok[0], tok[1:]
        if k == "w":
            rel, reg, mask = (int(x) for x in rest.split(":"))
            m(1 << 60 | rel << 24 | reg << 8 | mask)
        elif k == "f":
            age, fr = rest.split(":")
            m(2 << 60 | int(age) << 32 | int(fr, 16))
        elif k in "qp":
            age, lat = (int(x) for x in rest.split("/"))
            m((3 if k == "q" else 4) << 60 | age << 8 | lat)
        elif k == "i":
            al, reg = rest.split(":")
            age, lat = (int(x) for x in al.split("/"))
            m(5 << 60 | age << 40 | lat << 32 | int(reg, 16))
        elif k == "b":
            cnt, reg = (int(x) for x in rest.split(":"))
            m(6 << 60 | cnt << 8 | reg)
    return h


def dest_lanes(w):
    return [(w >> 24) & 1, (w >> 23) & 1, (w >> 22) & 1, (w >> 21) & 1]


class Block:
    """Symbolic evaluation of one block from one entry shape, emitting Rust."""

    def __init__(self, pc, pairs, live, shape):
        self.pc, self.pairs, self.live, self.shape = pc, pairs, live, shape
        self.out = []
        self.tmp = 0
        # timing state (cycles relative to the entry)
        self.cycle = 0
        self.ready = {}
        self.wq = []
        self.fmac = []      # dicts: start, flagreg, val (name of (mac, status, clip) tuple)
        self.fdiv = None    # [start, lat, name of (q, status)]
        self.efu = None     # [start, lat, name of p]
        self.ialu = []      # (start, lat, regmask)
        self.vib = [0, 0, "0"]  # count, reg, value name
        self.post_status = None
        self.post_clip = None
        # Registers the block names (cached in locals for its duration).
        self.ufv, self.uvi = set(), set()
        for up, lo in pairs:
            self.ufv |= {(up >> 6) & 31, (up >> 11) & 31, (up >> 16) & 31}
            if not up & 0x80000000:
                self.ufv |= {(lo >> 11) & 31, (lo >> 16) & 31}
                self.uvi |= {(lo >> 6) & 15, (lo >> 11) & 15, (lo >> 16) & 15}
        self.ufv.discard(0)
        self.uvi.discard(0)

    def t(self, prefix="t"):
        self.tmp += 1
        return f"{prefix}{self.tmp}"

    def e(self, line):
        self.out.append("    " + line)

    # ---- registers ----
    def vf(self, n):
        return "vuc::vf0()" if n == 0 else f"v{n}"

    def vi16(self, n):
        return "0u32" if n == 0 else f"(i{n} & 0xFFFF)"

    # ---- entry ----
    def enter(self):
        self.e("let sin = p.s_in();")
        self.e("let base = sin.cycle;")
        fm_i = 0
        for tok in self.shape.split():
            k, rest = tok[0], tok[1:]
            if k == "w":
                rel, reg, mask = (int(x) for x in rest.split(":"))
                for kk in range(4):
                    if mask & (8 >> kk):
                        self.ready[(reg, kk)] = max(self.ready.get((reg, kk), -99), rel)
                self.wq.append((rel, reg, mask))
            elif k == "f":
                age, fr = rest.split(":")
                self.fmac.append(dict(start=-int(age), flagreg=int(fr, 16), val=f"sin.fm[{fm_i}]"))
                fm_i += 1
            elif k == "q":
                age, lat = (int(x) for x in rest.split("/"))
                self.fdiv = [-age, lat, "sin.fdiv"]
            elif k == "p":
                age, lat = (int(x) for x in rest.split("/"))
                self.efu = [-age, lat, "sin.efu"]
            elif k == "i":
                al, reg = rest.split(":")
                age, lat = (int(x) for x in al.split("/"))
                self.ialu.append((-age, lat, int(reg, 16)))
            elif k == "b":
                cnt, reg = (int(x) for x in rest.split(":"))
                self.vib = [cnt, reg, "sin.vib"]
        # computed flags, visible flags / Q / P / I / R
        self.e("let (mut pmac, mut pst, mut pclip) = (sin.mac, sin.status, sin.clip);")
        self.e(f"let (mut vmac, mut vst, mut vclip) = (regs.vi[{VI_MAC}], regs.vi[{VI_STATUS}], regs.vi[{VI_CLIP}]);")
        self.e(f"let (mut vq, mut vp, mut vI, mut vr) = (regs.vi[{VI_Q}], regs.vi[{VI_P}], regs.vi[{VI_I}], regs.vi[{VI_R}]);")
        self.e("let mut acc = vuc::ld(&regs.acc);")
        for n in sorted(self.ufv):
            self.e(f"let mut v{n} = vuc::ld(&regs.vf[{n}]);")
        for n in sorted(self.uvi):
            self.e(f"let mut i{n} = regs.vi[{n}];")

    def store_regs(self):
        """Write the register locals back (exit, slow path)."""
        for n in sorted(self.ufv):
            self.e(f"vuc::st(&mut regs.vf[{n}], v{n});")
        for n in sorted(self.uvi):
            self.e(f"regs.vi[{n}] = i{n};")
        self.e("vuc::st(&mut regs.acc, acc);")
        self.e(f"regs.vi[{VI_Q}] = vq; regs.vi[{VI_P}] = vp; regs.vi[{VI_I}] = vI; regs.vi[{VI_R}] = vr;")

    def load_regs(self):
        for n in range(1, 32):
            self.e(f"v{n} = regs.vf[{n}];")
        self.e("acc = regs.acc;")

    # ---- the pipeline model ----
    def fmac_stall(self, reg, mask):
        if reg == 0 or mask == 0:
            return
        for k in range(4):
            if mask & (8 >> k):
                r = self.ready.get((reg, k))
                if r is not None and r > self.cycle:
                    self.cycle = r

    def stalls(self, i):
        if i.pipe == P_FMAC:
            self.fmac_stall(i.r0, i.r0_m)
            self.fmac_stall(i.r1, i.r1_m)
        elif i.pipe == P_FDIV:
            self.fmac_stall(i.r0, i.r0_m)
            self.fmac_stall(i.r1, i.r1_m)
            if self.fdiv:
                self.cycle = max(self.cycle, self.fdiv[0] + self.fdiv[1])
        elif i.pipe == P_EFU:
            self.fmac_stall(i.r0, i.r0_m)
            self.fmac_stall(i.r1, i.r1_m)
            if self.efu:
                self.efu[1] = max(0, self.efu[1] - 1)
                self.cycle = max(self.cycle, self.efu[0] + self.efu[1])
        elif i.pipe == P_BRANCH:
            for st, c, reg in self.ialu:
                if self.cycle - st < c and reg & i.vir:
                    self.cycle = max(self.cycle, st + c)

    def apply_fmac(self, ent):
        v = ent["val"]
        if ent["flagreg"] & W_CLIP:
            self.e(f"vclip = {v}.2;")
        if ent["flagreg"] & W_STATUS:
            self.e(f"vst = (vst & 0x30) | ({v}.1 & 0xFC0) | ({v}.1 & 0xF);")
        else:
            self.e(f"vst = (vst & 0xFF0) | ({v}.1 & 0xF) | (({v}.1 & 0xF) << 6);")
        self.e(f"vmac = {v}.0;")

    def retire(self):
        if not self.fmac and not self.fdiv and not self.efu and not self.ialu:
            return
        while self.fmac and self.cycle - self.fmac[0]["start"] >= 4:
            self.apply_fmac(self.fmac.pop(0))
        if self.fdiv and self.cycle - self.fdiv[0] >= self.fdiv[1]:
            v = self.fdiv[2]
            self.e(f"vq = {v}.0; vst = (vst & 0xFCF) | ({v}.1 & 0xC30);")
            self.fdiv = None
        if self.efu and self.cycle - self.efu[0] >= self.efu[1]:
            self.e(f"vp = {self.efu[2]};")
            self.efu = None
        while self.ialu and self.cycle - self.ialu[0][0] >= self.ialu[0][1]:
            self.ialu.pop(0)

    def fmac_write(self, reg, mask):
        if reg == 0:
            return
        done = self.cycle + 4
        for k in range(4):
            if mask & (8 >> k):
                self.ready[(reg, k)] = done
        if mask:
            self.wq.append((done, reg, mask))
            self.wq = self.wq[-8:]

    def branch_vi(self, reg):
        if self.vib[0] > 0 and self.vib[1] == reg and reg != 0:
            return f"({self.vib[2]} & 0xFFFF)"
        return self.vi16(reg)

    # ---- lower instructions ----
    def lower(self, i, w):
        """Emit the lower op; returns (pending, taken): pending is None or ('vf', reg, mask, name)
        / ('vi', reg, expr-name); taken is None or a Rust expression (Option<u32>-free: a u32
        target, plus a condition) as (cond, target)."""
        d = dest_lanes(w)
        dm = (w >> 21) & 0xF
        ft, fs = (w >> 16) & 31, (w >> 11) & 31
        it, is_, id_ = ft & 15, fs & 15, ((w >> 6) & 31) & 15
        imm11 = ((w & 0x7FF) ^ 0x400) - 0x400
        imm15 = (w & 0x7FF) | ((w >> 21) & 0xF) << 11
        fsf, ftf = (w >> 21) & 3, (w >> 23) & 3
        op = w >> 25

        def tgt(off):
            return self.at(i + 1 + off)

        def addr(reg, off):
            return f"({self.vi16(reg)} as i32 + {off}) as u32"

        def first_lane():
            for k in range(4):
                if d[k]:
                    return k
            return 0

        def pend_vf(reg, expr):
            n = self.t("pv")
            self.e(f"let {n}: vuc::V = {expr};")
            return ("vf", reg, dm, n)

        def pend_vi(reg, expr):
            n = self.t("pi")
            self.e(f"let {n}: u32 = {expr};")
            return ("vi", reg, n)
        if op == 0x00:
            return pend_vf(ft, f"vuc::rq(data, {addr(is_, imm11)})"), None
        if op == 0x01:
            self.e(f"vuc::wq::<{dm}>(data, {addr(it, imm11)}, {self.vf(fs)});")
            return None, None
        if op == 0x04:
            q = self.t("q")
            self.e(f"let {q} = vuc::rq(data, {addr(is_, imm11)});")
            return pend_vi(it, f"vuc::lane::<{first_lane()}>({q}) & 0xFFFF"), None
        if op == 0x05:
            self.e(f"vuc::wq::<{dm}>(data, {addr(is_, imm11)}, vuc::splat({self.vi16(it)}));")
            return None, None
        if op == 0x08:
            return pend_vi(it, f"{self.vi16(is_)}.wrapping_add({imm15})"), None
        if op == 0x09:
            return pend_vi(it, f"{self.vi16(is_)}.wrapping_sub({imm15})"), None
        if op == 0x10:
            return pend_vi(1, f"((vclip & 0xFF_FFFF) == {w & 0xFFFFFF}) as u32"), None
        if op == 0x11:
            self.post_clip = str(w & 0xFFFFFF)
            return None, None
        if op == 0x12:
            return pend_vi(1, f"(vclip & {w & 0xFFFFFF} != 0) as u32"), None
        if op == 0x13:
            return pend_vi(1, f"((vclip | {w & 0xFFFFFF}) & 0xFF_FFFF == 0xFF_FFFF) as u32"), None
        imm12 = (w & 0x7FF) | ((w >> 21) & 1) << 11
        if op == 0x14:
            return pend_vi(1, f"((vst & 0xFFF) == {imm12}) as u32"), None
        if op == 0x15:
            self.post_status = str(imm12)
            return None, None
        if op == 0x16:
            return pend_vi(it, f"vst & {imm12}"), None
        if op == 0x17:
            return pend_vi(it, f"(vst | {imm12}) & 0xFFF"), None
        if op == 0x18:
            return pend_vi(it, f"((vmac & 0xFFFF) == {self.vi16(is_)}) as u32"), None
        if op == 0x1A:
            return pend_vi(it, f"vmac & {self.vi16(is_)}"), None
        if op == 0x1B:
            return pend_vi(it, f"(vmac | {self.vi16(is_)}) & 0xFFFF"), None
        if op == 0x1C:
            return pend_vi(it, "vclip & 0xFFF"), None
        if op == 0x20:
            return None, ("true", tgt(imm11))
        if op == 0x21:
            return pend_vi(it, self.at(i + 2)), ("true", tgt(imm11))
        if op == 0x24:
            return None, ("true", f"{self.vi16(is_)} % {ICOUNT}")
        if op == 0x25:
            p = pend_vi(it, self.at(i + 2))
            return p, ("true", f"{self.vi16(is_)} % {ICOUNT}")
        if op in (0x28, 0x29):
            a, b = self.branch_vi(it), self.branch_vi(is_)
            c = f"{a} == {b}" if op == 0x28 else f"{a} != {b}"
            return None, (c, tgt(imm11))
        if 0x2C <= op <= 0x2F:
            a = f"({self.branch_vi(is_)} as u16 as i16)"
            c = {0x2C: f"{a} < 0", 0x2D: f"{a} > 0", 0x2E: f"{a} <= 0", 0x2F: f"{a} >= 0"}[op]
            return None, (c, tgt(imm11))
        if op != 0x40:
            raise Unsupported(f"lower op {w:#010x}")
        funct = w & 0x3F
        if funct == 0x30:
            return pend_vi(id_, f"{self.vi16(is_)}.wrapping_add({self.vi16(it)})"), None
        if funct == 0x31:
            return pend_vi(id_, f"{self.vi16(is_)}.wrapping_sub({self.vi16(it)})"), None
        if funct == 0x32:
            imm5 = (((w >> 6) & 31) ^ 16) - 16
            return pend_vi(it, f"({self.vi16(is_)} as i32 + {imm5}) as u32"), None
        if funct == 0x34:
            return pend_vi(id_, f"{self.vi16(is_)} & {self.vi16(it)}"), None
        if funct == 0x35:
            return pend_vi(id_, f"{self.vi16(is_)} | {self.vi16(it)}"), None
        if funct < 0x3C:
            raise Unsupported(f"lower special {w:#010x}")
        sop = (funct & 3) | ((w >> 6) & 31) << 2
        s = self.vf(fs)
        if sop == 0x30:
            return pend_vf(ft, s), None
        if sop == 0x31:
            return pend_vf(ft, f"vuc::mr32({s})"), None
        if sop == 0x34:
            a = self.t("a")
            self.e(f"let {a} = {self.vi16(is_)};")
            p = pend_vf(ft, f"vuc::rq(data, {a})")
            if is_ != 0:
                self.e(f"i{is_} = ({a} + 1) & 0xFFFF;")
            return p, None
        if sop == 0x35:
            a = self.t("a")
            self.e(f"let {a} = {self.vi16(it)};")
            self.e(f"vuc::wq::<{dm}>(data, {a}, {s});")
            if it != 0:
                self.e(f"i{it} = ({a} + 1) & 0xFFFF;")
            return None, None
        if sop == 0x36:
            a = self.t("a")
            self.e(f"let {a} = {self.vi16(is_)}.wrapping_sub(1) & 0xFFFF;")
            if is_ != 0:
                self.e(f"i{is_} = {a};")
            return pend_vf(ft, f"vuc::rq(data, {a})"), None
        if sop == 0x37:
            a = self.t("a")
            self.e(f"let {a} = {self.vi16(it)}.wrapping_sub(1) & 0xFFFF;")
            if it != 0:
                self.e(f"i{it} = {a};")
            self.e(f"vuc::wq::<{dm}>(data, {a}, {s});")
            return None, None
        if 0x38 <= sop <= 0x3A:
            n = self.t("fd")
            self.e(f"let {n} = vuc::fdiv({sop - 0x38}, vuc::lane::<{fsf}>({self.vf(fs)}), vuc::lane::<{ftf}>({self.vf(ft)}), vst);")
            self.fdiv = [self.cycle, 13 if sop == 0x3A else 7, n]
            return None, None
        if sop == 0x3B:
            return None, None
        if sop == 0x3C:
            return pend_vi(it, f"vuc::lane::<{fsf}>({s}) & 0xFFFF"), None
        if sop == 0x3D:
            return pend_vf(ft, f"vuc::splat({self.vi16(is_)} as u16 as i16 as i32 as u32)"), None
        if sop == 0x3E:
            q = self.t("q")
            self.e(f"let {q} = vuc::rq(data, {self.vi16(is_)});")
            return pend_vi(it, f"vuc::lane::<{first_lane()}>({q}) & 0xFFFF"), None
        if sop == 0x3F:
            self.e(f"vuc::wq::<{dm}>(data, {self.vi16(is_)}, vuc::splat({self.vi16(it)}));")
            return None, None
        if sop == 0x40:
            self.e("vr = vuc::rnext(vr);")
            return pend_vf(ft, "vuc::splat(vr)"), None
        if sop == 0x41:
            return pend_vf(ft, "vuc::splat(vr)"), None
        if sop == 0x42:
            self.e(f"vr = 0x3F80_0000 | (vuc::lane::<{fsf}>({s}) & 0x7F_FFFF);")
            return None, None
        if sop == 0x43:
            self.e(f"vr = 0x3F80_0000 | ((vr ^ vuc::lane::<{fsf}>({s})) & 0x7F_FFFF);")
            return None, None
        if sop == 0x64:
            return pend_vf(ft, "vuc::splat(vp)"), None
        if sop == 0x68:
            return pend_vi(it, "vuc::xtop(mp)"), None
        if sop == 0x69:
            return pend_vi(it, "vuc::xitop(mp)"), None
        if sop == 0x6C:
            self.e(f"vuc::xgkick(mp, data, {self.vi16(is_)});")
            return None, None
        lat = {0x70: 11, 0x71: 18, 0x72: 18, 0x73: 24, 0x74: 54, 0x75: 54, 0x76: 12,
               0x78: 12, 0x79: 18, 0x7A: 12, 0x7C: 29, 0x7D: 54, 0x7E: 44}
        if sop in lat:
            n = self.t("ef")
            self.e(f"let {n} = vuc::efu({sop}, {s}, {fsf});")
            self.efu = [self.cycle, lat[sop], n]
            return None, None
        if sop == 0x7B:
            return None, None
        raise Unsupported(f"lower special2 {w:#010x}")

    # ---- upper instructions ----
    def slow_upper(self, up, flags):
        """The exact path (ps2_runtime::vuc::upper_vals) on the operand values."""
        w = up & 0x01FFFFFF
        ft, fs, fd = (w >> 16) & 31, (w >> 11) & 31, (w >> 6) & 31
        special = (w & 0x3F) >= 0x3C
        wr = (w & ~((31 << 11) | (31 << 16))) | (1 << 11) | (2 << 16)
        if not special:
            wr = (wr & ~(31 << 6)) | (3 << 6)
        o = self.t("o")
        fdv = self.vf(fd) if not special else "vuc::vf0()"
        self.e(f"let {o} = vuc::upper_vals::<{wr:#x}, {'true' if flags else 'false'}>({self.vf(fs)}, {self.vf(ft)}, {fdv}, acc, vq, vI, pmac, pst, pclip);")
        if flags:
            self.e(f"pmac = {o}.mac; pst = {o}.status;")
        self.e(f"pclip = {o}.clip; acc = {o}.acc;")
        if not special and fd != 0:
            self.e(f"v{fd} = {o}.fd;")
        if special and ft != 0:
            self.e(f"v{ft} = {o}.ft;")

    def upper(self, up, flags):
        w = up & 0x01FFFFFF
        dm = (w >> 21) & 0xF
        ft, fs, fd, bc = (w >> 16) & 31, (w >> 11) & 31, (w >> 6) & 31, w & 3
        funct = w & 0x3F
        sop = (funct & 3) | ((w >> 6) & 31) << 2
        special = funct >= 0x3C
        if special and sop == 0x2F:
            return  # NOP
        # The non-MAC operations (as vu::upper_int).
        if not special and funct in (0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x1D, 0x1F, 0x2B, 0x2F):
            mx = funct in (0x10, 0x11, 0x12, 0x13, 0x1D, 0x2B)
            src = 0 if funct <= 0x17 else (2 if funct in (0x1D, 0x1F) else 3)
            if fd == 0:
                return
            t = f"vuc::bcast::<{bc}>({self.vf(ft)})" if src == 0 else ("vuc::splat(vI)" if src == 2 else self.vf(ft))
            self.e(f"v{fd} = vuc::minmax::<{dm}, {'true' if mx else 'false'}>(v{fd}, {self.vf(fs)}, {t});")
            return
        if special and sop == 0x1D:
            if ft != 0:
                self.e(f"v{ft} = vuc::abs::<{dm}>(v{ft}, {self.vf(fs)});")
            return
        if special and 0x10 <= sop <= 0x13:
            if ft != 0:
                sh = [0, 4, 12, 15][sop & 3]
                self.e(f"v{ft} = vuc::itof::<{dm}, {sh}>(v{ft}, {self.vf(fs)});")
            return
        if special and 0x14 <= sop <= 0x17:
            if ft == 0:
                return
            sh = [0, 4, 12, 15][sop & 3]
            self.e(f"match vuc::ftoi::<{dm}, {sh}>(v{ft}, {self.vf(fs)}) {{")
            self.e(f"    Some(x) => v{ft} = x,")
            self.e("    None => {")
            self.slow_upper(up, flags)
            self.e("    }")
            self.e("}")
            return
        if special and sop == 0x1F:
            self.e(f"match vuc::clip({self.vf(fs)}, vuc::lane::<3>({self.vf(ft)}), pclip) {{")
            self.e("    Some(c) => pclip = c,")
            self.e("    None => {")
            self.slow_upper(up, flags)
            self.e("    }")
            self.e("}")
            return
        # FMAC
        regular = {0x00: 0, 0x01: 0, 0x02: 0, 0x03: 0, 0x04: 0, 0x05: 0, 0x06: 0, 0x07: 0, 0x08: 0, 0x09: 0, 0x0A: 0, 0x0B: 0,
                   0x0C: 0, 0x0D: 0, 0x0E: 0, 0x0F: 0, 0x18: 0, 0x19: 0, 0x1A: 0, 0x1B: 0, 0x1C: 1, 0x1E: 2,
                   0x20: 1, 0x21: 1, 0x22: 2, 0x23: 2, 0x24: 1, 0x25: 1, 0x26: 2, 0x27: 2,
                   0x28: 3, 0x29: 3, 0x2A: 3, 0x2C: 3, 0x2D: 3, 0x2E: 3}
        code = sop if special else funct
        if code not in regular:
            self.slow_upper(up, flags)
            return
        src = regular[code]
        acc_dst = special
        if not flags and not acc_dst and fd == 0:
            return
        t = {0: f"vuc::bcast::<{bc}>({self.vf(ft)})", 1: "vuc::splat(vq)", 2: "vuc::splat(vI)", 3: self.vf(ft)}[src]
        dst = "acc" if acc_dst else (f"v{fd}" if fd != 0 else "vuc::vf0()")
        r = self.t("u")
        self.e(f"match vuc::fmac::<{w:#x}, {'true' if flags else 'false'}>({self.vf(fs)}, {t}, acc, {dst}, pst) {{")
        self.e(f"    Some({r}) => {{")
        if acc_dst:
            self.e(f"        acc = {r}.0;")
        elif fd != 0:
            self.e(f"        v{fd} = {r}.0;")
        if flags:
            self.e(f"        pmac = {r}.1; pst = {r}.2;")
        self.e("    }")
        self.e("    None => {")
        self.slow_upper(up, flags)
        self.e("    }")
        self.e("}")

    # ---- one pair (as pair_lean) ----
    def pair(self, i):
        up, lo = self.pairs[i]
        flags = self.live[i] == "1"
        ibit = bool(up & 0x80000000)
        ebit = bool(up & 0x40000000)
        self.e(f"// +{i}: {up:08x} {lo:08x}{' flags' if flags else ''}")
        self.cycle += 1
        before = self.cycle - 1
        ui = upper_info(up)
        self.stalls(ui)
        li = Info() if ibit else lower_info(lo)
        if not ibit:
            self.stalls(li)
        if needs_retire(lo, up):
            self.retire()
        if self.vib[0] > 0:
            self.vib[0] -= min(self.cycle - before, self.vib[0])
        backup = None
        if not ibit:
            br = backup_reg(lo)
            if br is not None:
                n = self.t("bk")
                self.e(f"let {n} = i{br};" if br else f"let {n} = 0u32;")
                backup = (br, n)
        pending, taken = None, None
        if not ibit and lo != 0x8000033C:
            pending, taken = self.lower(i, lo)
        if taken is not None:
            # Evaluate the branch now: the delay slot may change its registers.
            self.e(f"let tkc: bool = {taken[0]};")
            self.e(f"let tkt: u32 = {taken[1]};")
            taken = ("tkc", "tkt")
        self.upper(up, flags)
        if ibit:
            self.e(f"vI = {lo:#x};")
        if self.post_status is not None:
            st = self.post_status
            self.post_status = None
            if flags:
                self.e(f"pst = ({st} & 0xFC0) | (pst & 0x3F);")
        if self.post_clip is not None:
            c = self.post_clip
            self.post_clip = None
            if ui.viw & W_CLIP == 0:
                self.e(f"pclip = {c};")
        upper_fd = ui.vfw if (ui.pipe == P_FMAC and ui.vfw != 0) else None
        if pending:
            if pending[0] == "vf":
                _, rr, dm, n = pending
                if rr != 0 and upper_fd != rr:
                    self.e(f"v{rr} = vuc::blend::<{dm}>(v{rr}, {n});")
            else:
                _, rr, n = pending
                if rr != 0 and rr < 16:
                    self.e(f"i{rr} = {n} & 0xFFFF;")
        if ui.pipe == P_FMAC or li.pipe == P_FMAC:
            reg_u, mask_u, fu = (ui.vfw, ui.vfw_m, ui.viw) if ui.pipe == P_FMAC else (0, 0, 0)
            reg_l, mask_l, fl = (li.vfw, li.vfw_m, li.viw) if li.pipe == P_FMAC else (0, 0, 0)
            if flags or (fu | fl) & W_CLIP:
                if len(self.fmac) == 8:
                    self.apply_fmac(self.fmac.pop(0))
                n = self.t("fm")
                self.e(f"let {n} = (pmac, pst, pclip);")
                self.fmac.append(dict(start=self.cycle, flagreg=fu | fl, val=n))
            self.fmac_write(reg_u, mask_u)
            self.fmac_write(reg_l, mask_l)
        if li.pipe == P_IALU and li.cycles > 0:
            self.ialu.append((self.cycle, li.cycles, li.viw & 0xFFFF))
            self.ialu = self.ialu[-8:]
        if backup:
            reg, n = backup
            if self.vib[0] > 0 and self.vib[1] == reg:
                self.vib[0] = 2
            else:
                self.vib = [2, reg, n]
        return taken, ebit

    def exit_shape(self):
        """Pipe::shape of the state left (after `canon`)."""
        out = [f"w{t - self.cycle}:{r}:{m}" for (t, r, m) in self.wq if t > self.cycle]
        out += [f"f{min(self.cycle - e['start'], 4)}:{e['flagreg']:x}" for e in self.fmac]
        if self.fdiv:
            out.append(f"q{min(self.cycle - self.fdiv[0], self.fdiv[1])}/{self.fdiv[1]}")
        if self.efu:
            out.append(f"p{min(self.cycle - self.efu[0], self.efu[1])}/{self.efu[1]}")
        out += [f"i{self.cycle - st}/{c}:{reg:x}" for st, c, reg in self.ialu if self.cycle - st < c]
        if self.vib[0] > 0:
            out.append(f"b{self.vib[0]}:{self.vib[1]}")
        return " ".join(out)

    def exit_state(self):
        # Pipe::canon here, so the next block's entry key is known now.
        while self.fmac and self.cycle - self.fmac[0]["start"] >= 4:
            self.apply_fmac(self.fmac.pop(0))
        # The next block's entry shape (gen_vu.py --collect-elf follows it statically).
        self.exit = self.exit_shape()
        key = shape_key(self.exit)
        writes = [(t, r, m) for (t, r, m) in self.wq if t > self.cycle]
        w = ", ".join(f"({t}, {r}, {m})" for t, r, m in writes)
        fm = ", ".join(f"({e['start']}, {e['flagreg']:#x}, {e['val']}.0, {e['val']}.1, {e['val']}.2)" for e in self.fmac)
        fdiv = f"Some(({self.fdiv[0]}, {self.fdiv[1]}, {self.fdiv[2]}.0, {self.fdiv[2]}.1))" if self.fdiv else "None"
        efu = f"Some(({self.efu[0]}, {self.efu[1]}, {self.efu[2]}))" if self.efu else "None"
        ialu = ", ".join(f"({st}, {c}, {reg:#x})" for st, c, reg in self.ialu)
        vib = f"({self.vib[0]}, {self.vib[1]}, {self.vib[2]})" if self.vib[0] > 0 else "(0, 0, 0)"
        self.store_regs()
        self.e(f"regs.vi[{VI_MAC}] = vmac; regs.vi[{VI_STATUS}] = vst; regs.vi[{VI_CLIP}] = vclip;")
        self.e(f"p.s_out(base, {self.cycle}, (pmac, pst, pclip), &[{w}], &[{fm}], {fdiv}, {efu}, &[{ialu}], {vib}, {key:#x});")

    def at(self, k):
        """Rust expression: the address k pairs after the block start."""
        return f"((pc0 + {k % ICOUNT}) % {ICOUNT})"

    def compile(self, fname):
        n = len(self.pairs)
        up_last, lo_last = self.pairs[-1]
        if (up_last & 0x40000000) or is_branch(lo_last, up_last):
            raise Unsupported("control flow in the delay slot")
        for up, lo in self.pairs[:-2]:
            if up & 0x40000000 or is_branch(lo, up):
                raise Unsupported("control flow inside the block")
        self.enter()
        taken = None
        ebit = False
        for i in range(n):
            tk, eb = self.pair(i)
            if i == n - 2:
                taken, ebit = tk, eb
        nxt = self.at(n)
        self.exit_state()
        if taken is None:
            self.e(f"c.pc = {nxt};")
        else:
            self.e(f"c.pc = if {taken[0]} {{ {taken[1]} }} else {{ {nxt} }};")
        self.e("c.branch_to = None;")
        if ebit:
            self.e("c.end_after = true;")
            self.e("true")
        else:
            self.e("false")
        head = [f"#[inline(never)]",
                f"fn {fname}(regs: &mut VuRegs, data: *mut u8, p: &mut Pipe, c: &mut Ctl, mp: *mut Machine) -> bool {{",
                "    let _ = (data, mp);",
                # The block's address at run time (the same code is loaded at different pcs).
                "    let pc0 = c.pc;"]
        return "\n".join(head + self.out + ["}"])


def compile_block(fname, pc, pairs, live, shape):
    """Rust source of the block for one entry shape, or None if not translatable."""
    try:
        return Block(pc, pairs, live, shape).compile(fname)
    except Unsupported:
        return None


def exit_shape(pc, pairs, live, shape):
    """The shape a block leaves for its successor when entered with `shape`; None if the block
    is not translatable."""
    b = Block(pc, pairs, live, shape)
    try:
        b.compile("_")
    except Unsupported:
        return None
    return b.exit
