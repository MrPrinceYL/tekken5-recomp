// The GS rasterizer on the GPU. Every function mirrors raster.rs / gs.rs exactly (integer
// arithmetic only), so the GPU's local memory is bit-identical to the CPU reference's.

struct Params {
    a: vec4<u32>,
    b: vec4<u32>,
    c: vec4<u32>,
    d: vec4<u32>,
}

// A primitive (raster::Prim, packed by raster::gpu_prim).
struct Prim {
    // kind, state, clut (slot | clut16 << 31), qs; x0, y0, x1, y1; ox, oy, -, -; -
    h: array<vec4<i32>, 4>,
    lx: vec4<i64>,
    misc: vec4<i64>,
    e: array<vec4<i64>, 3>,
    p: array<vec4<i64>, 9>,
}

@group(0) @binding(0) var<storage, read_write> vram: array<atomic<u32>>;
@group(0) @binding(1) var<storage, read> tabs: array<u32>;
@group(0) @binding(2) var<storage, read> states: array<u32>;
@group(0) @binding(3) var<storage, read> prims: array<Prim>;
@group(0) @binding(4) var<storage, read> tiles: array<vec4<u32>>;
@group(0) @binding(5) var<storage, read> lists: array<u32>;
@group(0) @binding(6) var<storage, read_write> cluts: array<u32>;
@group(0) @binding(7) var<storage, read> data: array<u32>;
@group(0) @binding(8) var<uniform> P: Params;

const PSMCT32: u32 = 0x00u;
const PSMCT24: u32 = 0x01u;
const PSMCT16: u32 = 0x02u;
const PSMCT16S: u32 = 0x0Au;
const PSMT8: u32 = 0x13u;
const PSMT4: u32 = 0x14u;
const PSMT8H: u32 = 0x1Bu;
const PSMT4HL: u32 = 0x24u;
const PSMT4HH: u32 = 0x2Cu;
const PSMZ32: u32 = 0x30u;
const PSMZ24: u32 = 0x31u;
const PSMZ16: u32 = 0x32u;
const PSMZ16S: u32 = 0x3Au;

// Table offsets in `tabs` (gpu::tables).
const T32: u32 = 0u;
const T32Z: u32 = 2048u;
const T16: u32 = 4096u;
const T8: u32 = 20480u;
const T4: u32 = 28672u;
const LOG2: u32 = 45056u;

// State words (raster::gpu_state).
const S_FBP: u32 = 0u;
const S_FBW: u32 = 1u;
const S_FPSM: u32 = 2u;
const S_FBMSK: u32 = 3u;
const S_ZBP: u32 = 4u;
const S_ZPSM: u32 = 5u;
const S_FLAGS: u32 = 6u;
const S_ATST: u32 = 7u;
const S_AREF: u32 = 8u;
const S_AFAIL: u32 = 9u;
const S_ZTST: u32 = 10u;
const S_AA: u32 = 11u;
const S_AB: u32 = 12u;
const S_AC: u32 = 13u;
const S_AD: u32 = 14u;
const S_AFIX: u32 = 15u;
const S_FOGCOL: u32 = 16u;
const S_DIMX: u32 = 17u; // 2 words: 16 nibbles
const S_ZMAX: u32 = 19u;
const S_TPSM: u32 = 24u;
const S_TW: u32 = 25u;
const S_TH: u32 = 26u;
const S_TFX: u32 = 27u;
const S_WMS: u32 = 28u;
const S_WMT: u32 = 29u;
const S_MINU: u32 = 30u;
const S_MAXU: u32 = 31u;
const S_MINV: u32 = 32u;
const S_MAXV: u32 = 33u;
const S_TA0: u32 = 34u;
const S_TA1: u32 = 35u;
const S_MMAG: u32 = 36u;
const S_MMIN: u32 = 37u;
const S_MXL: u32 = 38u;
const S_L: u32 = 39u;
const S_K16: u32 = 40u;
const S_FIXED: u32 = 41u; // 0: depends on Q; else 1 | filter << 1 | level << 2
const S_TBP: u32 = 42u; // 7 words
const S_TBW: u32 = 49u; // 7 words
const STATE_WORDS: u32 = 64u;

// Flags.
const F_TME: u32 = 1u;
const F_FGE: u32 = 4u;
const F_ABE: u32 = 8u;
const F_FST: u32 = 16u;
const F_ZMSK: u32 = 32u;
const F_ATE: u32 = 64u;
const F_DATE: u32 = 128u;
const F_DATM: u32 = 256u;
const F_ZTE: u32 = 512u;
const F_PABE: u32 = 1024u;
const F_FBA: u32 = 2048u;
const F_COLCLAMP: u32 = 4096u;
const F_DTHE: u32 = 8192u;
const F_TCC: u32 = 16384u;
const F_AEM: u32 = 32768u;
const F_LCM: u32 = 65536u;

// ---- local memory ----

fn rd32(w: u32) -> u32 {
    return atomicLoad(&vram[w & 0xFFFFFu]);
}

// Write the bits `mask` of word `w` (other bits are left; other threads may own them).
fn wr(w: u32, mask: u32, v: u32) {
    let i = w & 0xFFFFFu;
    if mask == 0xFFFFFFFFu {
        atomicStore(&vram[i], v);
    } else {
        atomicAnd(&vram[i], ~mask);
        atomicOr(&vram[i], v & mask);
    }
}

fn addr32(bp: u32, bw: u32, x: u32, y: u32, z: bool) -> u32 {
    let page = (x >> 6u) + (y >> 5u) * max(bw, 1u);
    let t = select(T32, T32Z, z);
    return (bp * 64u + page * 2048u + tabs[t + (y & 31u) * 64u + (x & 63u)]) & 0xFFFFFu;
}

fn addr16(bp: u32, bw: u32, x: u32, y: u32, psm: u32) -> u32 {
    let page = (x >> 6u) + (y >> 6u) * max(bw, 1u);
    let v = select(0u, 1u, (psm & 8u) != 0u) + select(0u, 2u, (psm & 0x30u) == 0x30u);
    return (bp * 128u + page * 4096u + tabs[T16 + v * 4096u + (y & 63u) * 64u + (x & 63u)]) & 0x1FFFFFu;
}

fn addr8(bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    let page = (x >> 7u) + (y >> 6u) * max(bw >> 1u, 1u);
    return (bp * 256u + page * 8192u + tabs[T8 + (y & 63u) * 128u + (x & 127u)]) & 0x3FFFFFu;
}

fn addr4(bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    let page = (x >> 7u) + (y >> 7u) * max(bw >> 1u, 1u);
    return (bp * 512u + page * 16384u + tabs[T4 + (y & 127u) * 128u + (x & 127u)]) & 0x7FFFFFu;
}

fn read_pixel(psm: u32, bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    switch psm {
        case 0x00u, 0x30u: {
            return rd32(addr32(bp, bw, x, y, psm == PSMZ32));
        }
        case 0x01u, 0x31u: {
            return rd32(addr32(bp, bw, x, y, psm == PSMZ24)) & 0xFFFFFFu;
        }
        case 0x02u, 0x0Au, 0x32u, 0x3Au: {
            let h = addr16(bp, bw, x, y, psm);
            return (rd32(h >> 1u) >> ((h & 1u) * 16u)) & 0xFFFFu;
        }
        case 0x13u: {
            let b = addr8(bp, bw, x, y);
            return (rd32(b >> 2u) >> ((b & 3u) * 8u)) & 0xFFu;
        }
        case 0x14u: {
            let n = addr4(bp, bw, x, y);
            return (rd32(n >> 3u) >> ((n & 7u) * 4u)) & 15u;
        }
        case 0x1Bu: {
            return rd32(addr32(bp, bw, x, y, false)) >> 24u;
        }
        case 0x24u: {
            return (rd32(addr32(bp, bw, x, y, false)) >> 24u) & 15u;
        }
        case 0x2Cu: {
            return rd32(addr32(bp, bw, x, y, false)) >> 28u;
        }
        default: {
            return rd32(addr32(bp, bw, x, y, false));
        }
    }
}

fn store_pixel(psm: u32, bp: u32, bw: u32, x: u32, y: u32, v: u32) {
    switch psm {
        case 0x00u, 0x30u: {
            wr(addr32(bp, bw, x, y, psm == PSMZ32), 0xFFFFFFFFu, v);
        }
        case 0x01u, 0x31u: {
            wr(addr32(bp, bw, x, y, psm == PSMZ24), 0xFFFFFFu, v);
        }
        case 0x02u, 0x0Au, 0x32u, 0x3Au: {
            let h = addr16(bp, bw, x, y, psm);
            let sh = (h & 1u) * 16u;
            wr(h >> 1u, 0xFFFFu << sh, (v & 0xFFFFu) << sh);
        }
        case 0x13u: {
            let b = addr8(bp, bw, x, y);
            let sh = (b & 3u) * 8u;
            wr(b >> 2u, 0xFFu << sh, (v & 0xFFu) << sh);
        }
        case 0x14u: {
            let n = addr4(bp, bw, x, y);
            let sh = (n & 7u) * 4u;
            wr(n >> 3u, 15u << sh, (v & 15u) << sh);
        }
        case 0x1Bu: {
            wr(addr32(bp, bw, x, y, false), 0xFF000000u, v << 24u);
        }
        case 0x24u: {
            wr(addr32(bp, bw, x, y, false), 0x0F000000u, (v & 15u) << 24u);
        }
        case 0x2Cu: {
            wr(addr32(bp, bw, x, y, false), 0xF0000000u, (v & 15u) << 28u);
        }
        default: {}
    }
}

// ---- integer helpers ----

fn div_floor64(n: i64, d: i64) -> i64 {
    // Not `%`: i64 remainder is wrong on some backends; r = n - q·d is exact.
    let q = n / d;
    let r = n - q * d;
    if (r != 0li) && ((r < 0li) != (d < 0li)) {
        return q - 1li;
    }
    return q;
}

fn clamp64(v: i64, lo: i64, hi: i64) -> i64 {
    return min(max(v, lo), hi);
}

fn lo32(v: i64) -> u32 {
    return u32(v & 0xFFFFFFFFli);
}

// 16 · log2(x) for x > 0 (raster::log2_16).
fn log2_16(x: u64) -> i32 {
    let hi = u32(x >> 32u);
    let lo = u32(x & 0xFFFFFFFFlu);
    var lz: u32;
    if hi != 0u {
        lz = countLeadingZeros(hi);
    } else {
        lz = 32u + countLeadingZeros(lo);
    }
    let m = 63u - lz;
    let frac = u32(((x << (63u - m)) >> 55u) & 0xFFlu);
    return 16 * i32(m) + i32(tabs[LOG2 + frac]);
}

// ---- texturing ----

fn st(s: u32, f: u32) -> u32 {
    return states[s * STATE_WORDS + f];
}

fn pack(c: vec4<u32>) -> u32 {
    return c.x | (c.y << 8u) | (c.z << 16u) | (c.w << 24u);
}

fn c16(c: u32, s: u32) -> vec4<u32> {
    let r = (c & 31u) << 3u;
    let g = ((c >> 5u) & 31u) << 3u;
    let b = ((c >> 10u) & 31u) << 3u;
    var a: u32;
    if (c & 0x8000u) != 0u {
        a = st(s, S_TA1);
    } else if (st(s, S_FLAGS) & F_AEM) != 0u && (c & 0x7FFFu) == 0u {
        a = 0u;
    } else {
        a = st(s, S_TA0);
    }
    return vec4<u32>(r, g, b, a);
}

fn wrap(c: i32, size: i32, mode: u32, mn: i32, mx: i32) -> i32 {
    switch mode {
        case 0u: {
            return c & (size - 1);
        }
        case 1u: {
            return clamp(c, 0, size - 1);
        }
        case 2u: {
            return clamp(clamp(c, mn, mx), 0, size - 1);
        }
        default: {
            return ((c & mn) | mx) & (size - 1);
        }
    }
}

// A texel as packed RGBA8 (raster::texel_raw / cached_level).
fn texel(s: u32, clut: u32, level: u32, u: i32, v: i32) -> u32 {
    let tw = st(s, S_TW);
    let th = st(s, S_TH);
    let w = 1 << min(select(0u, tw - level, tw > level), 10u);
    let h = 1 << min(select(0u, th - level, th > level), 10u);
    let l = i32(level);
    let x = u32(wrap(u, w, st(s, S_WMS), i32(st(s, S_MINU)) >> u32(l), i32(st(s, S_MAXU)) >> u32(l))) & 2047u;
    let y = u32(wrap(v, h, st(s, S_WMT), i32(st(s, S_MINV)) >> u32(l), i32(st(s, S_MAXV)) >> u32(l))) & 2047u;
    let psm = st(s, S_TPSM);
    let raw = read_pixel(psm, st(s, S_TBP + level), st(s, S_TBW + level), x, y);
    switch psm {
        case 0x13u, 0x1Bu, 0x14u, 0x24u, 0x2Cu: {
            let e = cluts[(clut & 0xFFFFu) * 256u + (raw & 255u)];
            if (clut >> 31u) != 0u {
                return pack(c16(e, s));
            }
            return e;
        }
        case 0x02u, 0x0Au: {
            return pack(c16(raw, s));
        }
        case 0x01u: {
            let rgb = raw & 0xFFFFFFu;
            var a = st(s, S_TA0);
            if (st(s, S_FLAGS) & F_AEM) != 0u && rgb == 0u {
                a = 0u;
            }
            return rgb | (a << 24u);
        }
        default: {
            return raw;
        }
    }
}

fn unpack(c: u32) -> vec4<u32> {
    return vec4<u32>(c & 0xFFu, (c >> 8u) & 0xFFu, (c >> 16u) & 0xFFu, c >> 24u);
}

// (filter, level) for LOD lod16 (raster::lod_select with a Q).
fn lod_level(s: u32, q: i64, qs: i32) -> vec2<u32> {
    let fl = st(s, S_FLAGS);
    let mxl = st(s, S_MXL);
    let mmin = st(s, S_MMIN);
    let mmag = st(s, S_MMAG) != 0u;
    let k16 = bitcast<i32>(st(s, S_K16));
    var lod: i32;
    if (fl & F_LCM) != 0u {
        lod = k16;
    } else if mxl == 0u && mmag == ((mmin & 1u) == 1u) {
        lod = 0;
    } else {
        var aq = u64(q);
        if q < 0li {
            aq = u64(-q);
        }
        aq = max(aq, 1lu);
        lod = (qs * 16 - log2_16(aq)) * (1 << st(s, S_L)) + k16;
    }
    if lod <= 0 || mxl == 0u {
        var f = (mmin & 1u) == 1u;
        if lod <= 0 {
            f = mmag;
        }
        return vec2<u32>(select(0u, 1u, f), 0u);
    }
    let lvl = min(u32((lod + 8) >> 4u), mxl);
    let f = mmin == 1u || mmin >= 4u;
    return vec2<u32>(select(0u, 1u, f), select(0u, lvl, mmin >= 2u));
}

fn sample(s: u32, clut: u32, fs: i64, ft: i64, fq: i64, qs: i32) -> vec4<u32> {
    let fst = (st(s, S_FLAGS) & F_FST) != 0u;
    var u16: i32 = 0;
    var v16: i32 = 0;
    let lim_lo = -2147483648li;
    let lim_hi = 2147483647li;
    if fst {
        u16 = i32(clamp64(fs >> 16u, lim_lo, lim_hi));
        v16 = i32(clamp64(ft >> 16u, lim_lo, lim_hi));
    } else if fq != 0li {
        u16 = i32(clamp64(div_floor64(fs * (16li << min(st(s, S_TW), 10u)), fq), lim_lo, lim_hi));
        v16 = i32(clamp64(div_floor64(ft * (16li << min(st(s, S_TH), 10u)), fq), lim_lo, lim_hi));
    }
    var fl: vec2<u32>;
    let fx = st(s, S_FIXED);
    if fx != 0u {
        fl = vec2<u32>((fx >> 1u) & 1u, fx >> 2u);
    } else if fst {
        fl = lod_level(s, 1li, 0);
    } else {
        fl = lod_level(s, fq, qs);
    }
    let level = fl.y;
    var uu = u16 >> level;
    var vv = v16 >> level;
    if fl.x == 0u {
        return unpack(texel(s, clut, level, uu >> 4u, vv >> 4u));
    }
    uu = uu - 8;
    vv = vv - 8;
    let iu = uu >> 4u;
    let iv = vv >> 4u;
    let fu = u32(uu & 15);
    let fv = u32(vv & 15);
    if fu == 0u && fv == 0u {
        return unpack(texel(s, clut, level, iu, iv));
    }
    let c00 = texel(s, clut, level, iu, iv);
    let c10 = texel(s, clut, level, iu + 1, iv);
    let c01 = texel(s, clut, level, iu, iv + 1);
    let c11 = texel(s, clut, level, iu + 1, iv + 1);
    let w00 = (16u - fu) * (16u - fv);
    let w10 = fu * (16u - fv);
    let w01 = (16u - fu) * fv;
    let w11 = fu * fv;
    let M = 0x00FF00FFu;
    let rb = (((c00 & M) * w00 + (c10 & M) * w10 + (c01 & M) * w01 + (c11 & M) * w11) >> 8u) & M;
    let ga = ((((c00 >> 8u) & M) * w00 + ((c10 >> 8u) & M) * w10 + ((c01 >> 8u) & M) * w01 + ((c11 >> 8u) & M) * w11) >> 8u) & M;
    return unpack(rb | (ga << 8u));
}

// ---- the pixel pipeline (raster::plot_g) ----

fn plane(pr: u32, i: u32, dx: i64, dy: i64) -> i64 {
    let p = prims[pr].p[i];
    return p.x + p.y * dx + p.z * dy;
}

fn ch(v: i64) -> u32 {
    return u32(clamp64(v >> 16u, 0li, 255li));
}

fn pick(sel: u32, cs: i32, cd: i32) -> i32 {
    switch sel {
        case 0u: {
            return cs;
        }
        case 1u: {
            return cd;
        }
        default: {
            return 0;
        }
    }
}

fn clampc(v: i32, colclamp: bool) -> u32 {
    if colclamp {
        return u32(clamp(v, 0, 255));
    }
    return u32(v & 0xFF);
}

fn plot(pr: u32, x: i32, y: i32, dx: i64, dy: i64) {
    let h = prims[pr].h;
    let s = u32(h[0].y);
    let clut = u32(h[0].z);
    let qs = h[0].w;
    let fl = st(s, S_FLAGS);
    let ux = u32(x) & 2047u;
    let uy = u32(y) & 2047u;
    var c = vec4<u32>(ch(plane(pr, 1u, dx, dy)), ch(plane(pr, 2u, dx, dy)), ch(plane(pr, 3u, dx, dy)), ch(plane(pr, 4u, dx, dy)));
    if (fl & F_TME) != 0u {
        let tx = sample(s, clut, plane(pr, 6u, dx, dy), plane(pr, 7u, dx, dy), plane(pr, 8u, dx, dy), qs);
        let tcc = (fl & F_TCC) != 0u;
        switch st(s, S_TFX) {
            case 0u: {
                c.x = min((tx.x * c.x) >> 7u, 255u);
                c.y = min((tx.y * c.y) >> 7u, 255u);
                c.z = min((tx.z * c.z) >> 7u, 255u);
                if tcc {
                    c.w = min((tx.w * c.w) >> 7u, 255u);
                }
            }
            case 1u: {
                c.x = tx.x;
                c.y = tx.y;
                c.z = tx.z;
                if tcc {
                    c.w = tx.w;
                }
            }
            default: {
                let fa = c.w;
                c.x = min(((tx.x * c.x) >> 7u) + fa, 255u);
                c.y = min(((tx.y * c.y) >> 7u) + fa, 255u);
                c.z = min(((tx.z * c.z) >> 7u) + fa, 255u);
                if tcc {
                    if st(s, S_TFX) == 2u {
                        c.w = min(tx.w + fa, 255u);
                    } else {
                        c.w = tx.w;
                    }
                }
            }
        }
    }
    if (fl & F_FGE) != 0u {
        let fv = ch(plane(pr, 5u, dx, dy));
        let fc = st(s, S_FOGCOL);
        c.x = (c.x * fv + (fc & 0xFFu) * (255u - fv)) >> 8u;
        c.y = (c.y * fv + ((fc >> 8u) & 0xFFu) * (255u - fv)) >> 8u;
        c.z = (c.z * fv + ((fc >> 16u) & 0xFFu) * (255u - fv)) >> 8u;
    }
    // Alpha test.
    var write_fb = true;
    var write_rgb_only = false;
    var write_z = (fl & F_ZMSK) == 0u;
    if (fl & F_ATE) != 0u {
        let a = c.w;
        let aref = st(s, S_AREF);
        var ok = false;
        switch st(s, S_ATST) {
            case 0u: {
                ok = false;
            }
            case 1u: {
                ok = true;
            }
            case 2u: {
                ok = a < aref;
            }
            case 3u: {
                ok = a <= aref;
            }
            case 4u: {
                ok = a == aref;
            }
            case 5u: {
                ok = a >= aref;
            }
            case 6u: {
                ok = a > aref;
            }
            default: {
                ok = a != aref;
            }
        }
        if !ok {
            switch st(s, S_AFAIL) {
                case 0u: {
                    return;
                }
                case 1u: {
                    write_z = false;
                }
                case 2u: {
                    write_fb = false;
                }
                default: {
                    write_rgb_only = true;
                    write_z = false;
                }
            }
        }
    }
    let fpsm = st(s, S_FPSM);
    let fbp = st(s, S_FBP);
    let fbw = st(s, S_FBW);
    let blend = (fl & F_ABE) != 0u && !((fl & F_PABE) != 0u && (c.w & 0x80u) == 0u);
    let need_dst = blend || (fl & F_DATE) != 0u || st(s, S_FBMSK) != 0u || write_rgb_only || fpsm != PSMCT32 || !write_fb;
    var dst = 0u;
    if need_dst {
        dst = read_pixel(fpsm, fbp, fbw, ux, uy);
    }
    var d = vec4<u32>(dst & 0xFFu, (dst >> 8u) & 0xFFu, (dst >> 16u) & 0xFFu, dst >> 24u);
    let is16 = fpsm == PSMCT16 || fpsm == PSMCT16S || fpsm == PSMZ16 || fpsm == PSMZ16S;
    if is16 {
        d = vec4<u32>((dst & 31u) << 3u, ((dst >> 5u) & 31u) << 3u, ((dst >> 10u) & 31u) << 3u, select(0u, 0x80u, (dst & 0x8000u) != 0u));
    } else if fpsm == PSMCT24 || fpsm == PSMZ24 {
        d.w = 0x80u;
    }
    // Destination alpha test.
    if (fl & F_DATE) != 0u {
        var bit = false;
        if fpsm == PSMCT16 || fpsm == PSMCT16S {
            bit = (dst & 0x8000u) != 0u;
        } else if fpsm == PSMCT32 {
            bit = (dst & 0x80000000u) != 0u;
        }
        if bit != ((fl & F_DATM) != 0u) {
            return;
        }
    }
    // Depth test.
    let z = u32(clamp64(plane(pr, 0u, dx, dy) >> 16u, 0li, i64(st(s, S_ZMAX))));
    let zte = (fl & F_ZTE) != 0u;
    let zpsm = st(s, S_ZPSM);
    let zbp = st(s, S_ZBP);
    if zte {
        let dz = read_pixel(zpsm, zbp, fbw, ux, uy);
        var ok = false;
        switch st(s, S_ZTST) {
            case 0u: {
                ok = false;
            }
            case 1u: {
                ok = true;
            }
            case 2u: {
                ok = z >= dz;
            }
            default: {
                ok = z > dz;
            }
        }
        if !ok {
            return;
        }
    }
    let colclamp = (fl & F_COLCLAMP) != 0u;
    // Alpha blend.
    if blend {
        var cc: i32;
        switch st(s, S_AC) {
            case 0u: {
                cc = i32(c.w);
            }
            case 1u: {
                cc = i32(d.w);
            }
            default: {
                cc = i32(st(s, S_AFIX));
            }
        }
        let aa = st(s, S_AA);
        let ab = st(s, S_AB);
        let ad = st(s, S_AD);
        let cs = vec3<i32>(i32(c.x), i32(c.y), i32(c.z));
        let cd = vec3<i32>(i32(d.x), i32(d.y), i32(d.z));
        c.x = clampc((((pick(aa, cs.x, cd.x) - pick(ab, cs.x, cd.x)) * cc) >> 7u) + pick(ad, cs.x, cd.x), colclamp);
        c.y = clampc((((pick(aa, cs.y, cd.y) - pick(ab, cs.y, cd.y)) * cc) >> 7u) + pick(ad, cs.y, cd.y), colclamp);
        c.z = clampc((((pick(aa, cs.z, cd.z) - pick(ab, cs.z, cd.z)) * cc) >> 7u) + pick(ad, cs.z, cd.z), colclamp);
    }
    if (fl & F_DTHE) != 0u && (fpsm == PSMCT16 || fpsm == PSMCT16S) {
        let i = (uy & 3u) * 4u + (ux & 3u);
        let nib = (st(s, S_DIMX + (i >> 3u)) >> ((i & 7u) * 4u)) & 7u;
        var dv = i32(nib);
        if (nib & 4u) != 0u {
            dv = dv - 8;
        }
        c.x = clampc(i32(c.x) + dv, colclamp);
        c.y = clampc(i32(c.y) + dv, colclamp);
        c.z = clampc(i32(c.z) + dv, colclamp);
    }
    if (fl & F_FBA) != 0u {
        c.w = c.w | 0x80u;
    }
    if write_fb {
        var out: u32;
        if is16 {
            out = (c.x >> 3u) | ((c.y >> 3u) << 5u) | ((c.z >> 3u) << 10u) | (((c.w >> 7u) & 1u) << 15u);
        } else {
            out = c.x | (c.y << 8u) | (c.z << 16u) | (c.w << 24u);
        }
        var msk = st(s, S_FBMSK);
        if fpsm == PSMCT16 || fpsm == PSMCT16S {
            msk = ((msk >> 3u) & 0x1Fu) | ((msk >> 6u) & 0x3E0u) | ((msk >> 9u) & 0x7C00u) | ((msk >> 16u) & 0x8000u);
        }
        if write_rgb_only {
            msk = msk | select(0xFF000000u, 0x8000u, fpsm == PSMCT16 || fpsm == PSMCT16S);
        }
        out = (out & ~msk) | (dst & msk);
        store_pixel(fpsm, fbp, fbw, ux, uy, out);
    }
    if write_z && zte {
        store_pixel(zpsm, zbp, fbw, ux, uy, z);
    }
}

// Draw primitive `pr` at pixel (x, y) if it covers it (raster::draw_prim).
fn draw_px(pr: u32, x: i32, y: i32) {
    let h = prims[pr].h;
    if x < h[1].x || y < h[1].y || x >= h[1].z || y >= h[1].w {
        return;
    }
    let kind = u32(h[0].x);
    let dx = i64(x - h[2].x);
    let dy = i64(y - h[2].y);
    if kind == 1u || kind == 2u {
        let l = prims[pr].lx;
        let n = prims[pr].misc.x;
        for (var i = 0li; i < n; i = i + 1li) {
            let px = div_floor64(l.x * n + (l.z - l.x) * i + 8li * n, 16li * n);
            let py = div_floor64(l.y * n + (l.w - l.y) * i + 8li * n, 16li * n);
            if px == i64(x) && py == i64(y) {
                plot(pr, x, y, i, 0li);
            }
        }
        return;
    }
    if kind >= 3u && kind <= 5u {
        let X = i64(x);
        let Y = i64(y);
        for (var k = 0u; k < 3u; k = k + 1u) {
            let e = prims[pr].e[k];
            if e.x + e.z * Y + e.y * X + e.w < 0li {
                return;
            }
        }
    }
    plot(pr, x, y, dx, dy);
}

// One workgroup per 16x16 tile; each thread one pixel, the tile's primitives in order.
@compute @workgroup_size(16, 16)
fn raster(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>) {
    let t = tiles[P.a.y + wid.x];
    let x = i32((t.x & 0xFFFFu) * 16u + lid.x);
    let y = i32((t.x >> 16u) * 16u + lid.y);
    for (var i = 0u; i < t.z; i = i + 1u) {
        draw_px(lists[t.y + i], x, y);
    }
}

// Frame and depth buffer overlap: one workgroup, primitive by primitive.
@compute @workgroup_size(16, 16)
fn raster_serial(@builtin(local_invocation_id) lid: vec3<u32>) {
    let first = P.a.z;
    let count = P.a.w;
    for (var i = 0u; i < count; i = i + 1u) {
        let pr = first + i;
        let h = prims[pr].h;
        let tx0 = h[1].x >> 4u;
        let ty0 = h[1].y >> 4u;
        let tx1 = (h[1].z + 15) >> 4u;
        let ty1 = (h[1].w + 15) >> 4u;
        for (var ty = ty0; ty < ty1; ty = ty + 1) {
            for (var tx = tx0; tx < tx1; tx = tx + 1) {
                draw_px(pr, tx * 16 + i32(lid.x), ty * 16 + i32(lid.y));
            }
        }
        storageBarrier();
        workgroupBarrier();
    }
}

// ---- transfers ----

// Host -> local (gs::Gs::run_upload): one pixel per thread.
// a: dbp, dbw, dpsm, bits; b: dsax, dsay, rrw, rrh; c: x, y, data word offset, pixels.
@compute @workgroup_size(256)
fn upload(@builtin(global_invocation_id) gid: vec3<u32>) {
    let k = gid.x;
    if k >= P.c.w {
        return;
    }
    let rrw = P.b.z;
    let off = (P.c.x - P.b.x) + k;
    let yy = P.c.y + off / rrw;
    let xx = P.b.x + off % rrw;
    if yy >= P.b.y + P.b.w {
        return;
    }
    let base = P.c.z;
    var v: u32;
    switch P.a.w {
        case 32u: {
            v = data[base + k];
        }
        case 24u: {
            let b0 = k * 3u;
            v = 0u;
            for (var i = 0u; i < 3u; i = i + 1u) {
                let b = b0 + i;
                v = v | (((data[base + (b >> 2u)] >> ((b & 3u) * 8u)) & 0xFFu) << (8u * i));
            }
        }
        case 16u: {
            v = (data[base + (k >> 1u)] >> ((k & 1u) * 16u)) & 0xFFFFu;
        }
        case 8u: {
            v = (data[base + (k >> 2u)] >> ((k & 3u) * 8u)) & 0xFFu;
        }
        default: {
            v = (data[base + (k >> 3u)] >> ((k & 7u) * 4u)) & 15u;
        }
    }
    store_pixel(P.a.z, P.a.x, P.a.y, xx & 2047u, yy & 2047u, v);
}

// Local -> local (gs::Gs::local_to_local).
// a: sbp, sbw, spsm, dir; b: dbp, dbw, dpsm, serial; c: sx, sy, dx, dy; d: rrw, rrh.
fn copy_px(x: u32, y: u32) {
    let v = read_pixel(P.a.z, P.a.x, P.a.y, (P.c.x + x) & 2047u, (P.c.y + y) & 2047u);
    store_pixel(P.b.z, P.b.x, P.b.y, (P.c.z + x) & 2047u, (P.c.w + y) & 2047u, v);
}

@compute @workgroup_size(256)
fn copy(@builtin(global_invocation_id) gid: vec3<u32>) {
    let rrw = P.d.x;
    let rrh = P.d.y;
    if P.b.w == 0u {
        let k = gid.x;
        if k < rrw * rrh {
            copy_px(k % rrw, k / rrw);
        }
        return;
    }
    if gid.x != 0u {
        return;
    }
    for (var j = 0u; j < rrh; j = j + 1u) {
        let y = select(j, rrh - 1u - j, (P.a.w & 2u) != 0u);
        for (var i = 0u; i < rrw; i = i + 1u) {
            let x = select(i, rrw - 1u - i, (P.a.w & 1u) != 0u);
            copy_px(x, y);
        }
    }
}

// CLUT load (gs::Gs::read_clut): slot b.y = slot b.x with the loaded entries replaced.
// a: tex0 lo, tex0 hi, texclut lo, texclut hi; b: src slot, dst slot.
@compute @workgroup_size(256)
fn clut(@builtin(local_invocation_id) lid: vec3<u32>) {
    let sl = lid.x;
    let thi = P.a.y;
    let psm = (P.a.x >> 20u) & 0x3Fu;
    let cbp = (thi >> 5u) & 0x3FFFu;
    var cpsm = (thi >> 19u) & 0xFu;
    let csm = (thi >> 23u) & 1u;
    let csa = (thi >> 24u) & 0x1Fu;
    let eight = psm == PSMT8 || psm == PSMT8H;
    let n = select(16u, 256u, eight);
    if cpsm == 0u {
        cpsm = PSMCT32;
    }
    var i = sl;
    if !eight {
        i = (sl - csa * 16u) & 255u;
    }
    var val = cluts[P.b.x * 256u + sl];
    if i < n {
        var x: u32;
        var y: u32;
        if csm == 0u {
            if eight {
                x = (i & 7u) | ((i & 0x10u) >> 1u);
                y = ((i >> 5u) << 1u) | ((i >> 3u) & 1u);
            } else {
                x = i & 7u;
                y = i >> 3u;
            }
        } else {
            let tc = P.a.z;
            x = ((tc >> 6u) & 0x3Fu) * 16u + i;
            y = (tc >> 12u) & 0x3FFu;
        }
        let cbw = select(P.a.z & 0x3Fu, 1u, csm == 0u);
        let c = read_pixel(cpsm, cbp, cbw, x, y);
        val = select(c | 0x80000000u, c, cpsm == PSMCT32 || cpsm == PSMCT24);
        if cpsm == PSMCT16 || cpsm == PSMCT16S {
            val = c & 0xFFFFu;
        }
    }
    cluts[P.b.y * 256u + sl] = val;
}
