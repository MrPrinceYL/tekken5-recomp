//! GS drawing: rasterization of the kicked primitive and the per-pixel pipeline
//! (texture -> texture function -> fog -> alpha test -> destination alpha test -> depth test ->
//! alpha blend -> dither / clamp -> frame and depth writes, with FBMSK / ZMSK).
//!
//! Coordinates are 12.4 fixed point after XYOFFSET; a pixel is covered when its top-left corner
//! (integer coordinate) lies inside the primitive, with the top-left fill rule.

use crate::gs::*;
use rayon::prelude::*;

/// Most primitives queued before a flush.
const MAX_BATCH: usize = 4096;

#[derive(Clone, Copy)]
struct GsPtr(*mut Gs);
unsafe impl Send for GsPtr {}
unsafe impl Sync for GsPtr {}

impl GsPtr {
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut Gs {
        unsafe { &mut *self.0 }
    }
}

/// Pixel rectangle [x0, x1) x [y0, y1) a primitive is drawn into.
#[derive(Clone, Copy)]
struct Clip {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

const NO_CLIP: Clip = Clip { x0: i32::MIN, y0: i32::MIN, x1: i32::MAX, y1: i32::MAX };

/// Run `row` for every row in [y0, y1).
#[inline(always)]
fn rows(gs: &mut Gs, y0: i64, y1: i64, width: i64, mut row: impl FnMut(&mut Gs, i64)) {
    let (k, n) = OWN.with(|o| o.get());
    if k == 0 {
        PIXELS.fetch_add(((y1 - y0).max(0) * width.max(0)) as u64, std::sync::atomic::Ordering::Relaxed);
    }
    if n == 1 {
        for y in y0..y1 {
            row(gs, y);
        }
        return;
    }
    // This worker's bands of BAND rows (band b belongs to worker b % n).
    let band = band() as i64;
    let mut b = y0.max(0) / band;
    while b * band < y1 {
        if b as u64 % n as u64 == k as u64 {
            for y in (b * band).max(y0)..((b + 1) * band).min(y1) {
                row(gs, y);
            }
            b += n as i64;
        } else {
            b += 1;
        }
    }
}

/// Batches covering fewer pixels (summed boxes) are drawn on the GS thread (`flush`).
const SMALL_BATCH: i64 = 2048;

/// Rows per band of the interleaved row ownership (PCSX2 GSRasterizerList thread height).
const BAND_DEFAULT: i32 = 8;

/// T5_SAFE=1: the reference behaviour for parity checks: no direct sampling, no transfer /
/// CLUT / decode / read-out jobs (the GS thread waits instead), batches drawn in order.
pub fn safe() -> bool {
    static S: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *S.get_or_init(|| crate::diag::enabled("T5_SAFE"))
}

/// Call `f` for each page set in `mask`.
#[inline(always)]
fn each_page(mask: &[u64; 8], mut f: impl FnMut(usize)) {
    for (i, &word) in mask.iter().enumerate() {
        let mut b = word;
        while b != 0 {
            f(i * 64 + b.trailing_zeros() as usize);
            b &= b - 1;
        }
    }
}

/// Whether the queued batch draws into (or samples, with `reads`) any of `mask`'s pages.
pub fn batch_touches(gs: &Gs, mask: &[u64; 8], reads: bool) -> bool {
    !gs.batch.items.is_empty() && (0..8).any(|i| mask[i] & (gs.batch.pages[i] | if reads { gs.batch.reads[i] } else { 0 }) != 0)
}

/// Send a batch to the GPU (`gpu.rs`): its primitives as the CPU reference would draw them.
fn gpu_flush(gs: &mut Gs, batch: &Batch) {
    let g = gs.gpu.as_mut().unwrap();
    g.reserve(batch.items.len(), 0);
    let first = g.prim_count();
    // Setup in parallel (pure per primitive), then states and order on this thread.
    let prims: Vec<Option<Prim>> = if batch.items.len() >= 64 {
        batch.items.par_iter().map(|q| prim_setup(&q.st, q.kind, &q.vs)).collect()
    } else {
        batch.items.iter().map(|q| prim_setup(&q.st, q.kind, &q.vs)).collect()
    };
    for (q, pr) in batch.items.iter().zip(prims) {
        let Some(pr) = pr else { continue };
        let key = std::sync::Arc::as_ptr(&q.st) as usize;
        let s = g.state(key, q.st.clone(), || gpu_state(&q.st));
        g.push_prim(gpu_prim(&pr, s, q.clut));
    }
    g.raster(first, batch.serial);
}

/// A primitive for the GPU (`gs.wgsl` struct Prim).
fn gpu_prim(pr: &Prim, state: u32, clut: u32) -> crate::gpu::GPrim {
    let mut h = [0i32; 16];
    h[..10].copy_from_slice(&[pr.kind as i32, state as i32, clut as i32, pr.ip.qs, pr.x0, pr.y0, pr.x1, pr.y1, pr.ox, pr.oy]);
    crate::gpu::GPrim { h, lx: pr.lx, misc: [pr.n, 0, 0, 0], e: pr.e, p: pr.ip.p.map(|p| [p.c0, p.cx, p.cy, 0]) }
}

/// A primitive state for the GPU (`gs.wgsl` S_* words).
fn gpu_state(st: &Setup) -> [u32; crate::gpu::STATE_WORDS] {
    let mut w = [0u32; crate::gpu::STATE_WORDS];
    let b = |v: bool, bit: u32| if v { bit } else { 0 };
    let mut fl = b(st.tme, 1) | b(st.fge, 4) | b(st.abe, 8) | b(st.fst, 16) | b(st.zmsk, 32) | b(st.ate, 64) | b(st.date, 128) | b(st.datm, 256);
    fl |= b(st.zte, 512) | b(st.pabe, 1024) | b(st.fba, 2048) | b(st.colclamp, 4096) | b(st.dthe, 8192);
    let mut dimx = 0u64;
    for y in 0..4 {
        for x in 0..4 {
            dimx |= ((st.dimx[y][x] & 7) as u64) << (16 * y + 4 * x);
        }
    }
    w[..20].copy_from_slice(&[
        st.fbp, st.fbw, st.fpsm, st.fbmsk, st.zbp, st.zpsm, 0, st.atst, st.aref, st.afail, st.ztst, st.aa, st.ab, st.ac, st.ad, st.afix,
        st.fogcol[0] | st.fogcol[1] << 8 | st.fogcol[2] << 16, dimx as u32, (dimx >> 32) as u32, st.zmax,
    ]);
    if let Some(t) = st.tex.as_ref() {
        fl |= b(t.tcc, 16384) | b(t.aem, 32768) | b(t.lcm, 65536);
        let fixed = t.fixed_lod.map_or(0, |(f, l)| 1 | (f as u32) << 1 | (l as u32) << 2);
        w[24..42].copy_from_slice(&[
            t.psm, t.tw, t.th, t.tfx, t.wms, t.wmt, t.minu as u32, t.maxu as u32, t.minv as u32, t.maxv as u32, t.ta0, t.ta1, t.mmag as u32, t.mmin, t.mxl, t.l, t.k16 as u32, fixed,
        ]);
        for l in 0..7 {
            let (tbp, tbw) = if l == 0 { (t.tbp, t.tbw) } else { t.mip[l] };
            w[42 + l] = tbp;
            w[49 + l] = tbw;
        }
    }
    w[6] = fl;
    w
}

/// Rows per band (T5_BAND for measurements).
#[inline(always)]
fn band() -> i32 {
    static B: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *B.get_or_init(|| std::env::var("T5_BAND").ok().and_then(|v| v.parse().ok()).unwrap_or(BAND_DEFAULT))
}

thread_local! {
    /// (worker index, worker count) of a raster worker thread; (0, 1) elsewhere = all rows.
    static OWN: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 1)) };
}

#[inline(always)]
fn owns_row(y: i32) -> bool {
    let (k, n) = OWN.with(|o| o.get());
    n == 1 || (y.max(0) / band()) as u32 % n == k
}

/// Persistent raster workers (PCSX2 GSRasterizerList): every worker draws every batch, in
/// order, but only its own interleaved bands of rows, so batches need no barrier between them.
/// The GS thread waits only when something must see local memory (`wait`), or when a batch
/// for another frame / depth buffer would touch pages a batch in flight uses.
pub struct Workers {
    txs: Vec<std::sync::mpsc::Sender<std::sync::Arc<Work>>>,
    done: std::sync::Arc<Vec<std::sync::atomic::AtomicU64>>,
    sent: u64,
    /// Batches in flight: (sequence number, frame / depth pages), until all workers passed it.
    flights: std::collections::VecDeque<(u64, [u64; 8], [u64; 8], Option<std::sync::Arc<Work>>)>,
    /// Per page: the newest job that writes it / reads it (dependency queries in O(pages)).
    last_w: Box<[u64; 512]>,
    last_r: Box<[u64; 512]>,
    /// Per flight (diagnostics): buffers and the union of the primitive boxes.
    flight_info: std::collections::VecDeque<(u64, (u32, u32, u32, u32, u32, bool), (i32, i32, i32, i32))>,
    /// Pages (frame / depth) of the batches in flight, and the buffers of the last one.
    inflight: [u64; 8],
    last_key: Option<(u32, u32, u32, u32, u32, bool)>,
}

impl Workers {
    fn start(gs: &mut Gs) -> Workers {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        // T5_RASTER_THREADS=<n>: pool size for measurements.
        let pin = crate::raster_mask();
        let dflt = pin.map(|m| m.count_ones() as usize).unwrap_or(cores.saturating_sub(3).max(2));
        let n = std::env::var("T5_RASTER_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(dflt);
        let done: std::sync::Arc<Vec<std::sync::atomic::AtomicU64>> = std::sync::Arc::new((0..n).map(|_| std::sync::atomic::AtomicU64::new(0)).collect());
        let mut txs = Vec::new();
        for k in 0..n {
            let (tx, rx) = std::sync::mpsc::channel::<std::sync::Arc<Work>>();
            txs.push(tx);
            let p = GsPtr(gs);
            let done = done.clone();
            std::thread::Builder::new()
                .name(format!("raster{k}"))
                .spawn(move || {
                    #[cfg(windows)]
                    if k == 0 {
                        #[link(name = "kernel32")]
                        unsafe extern "system" {
                            fn GetCurrentThreadId() -> u32;
                        }
                        crate::gs::RASTER_OS_TID.store(unsafe { GetCurrentThreadId() }, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(m) = pin {
                        crate::set_affinity(m);
                    }
                    OWN.with(|o| o.set((k as u32, n as u32)));
                    let p = p;
                    let mut seq = 0u64;
                    while let Ok(work) = rx.recv() {
                        // Texture reads of drawing in progress: every worker past that batch.
                        // Worker-0 jobs (transfer, CLUT) need only worker 0 to wait.
                        let mine = !work.items.is_empty() || work.decode.is_some() || k == 0;
                        if work.after > 0 && mine {
                            while done.iter().any(|d| d.load(std::sync::atomic::Ordering::Acquire) < work.after) {
                                std::hint::spin_loop();
                            }
                        }
                        if let Some(u) = work.upload.as_ref() {
                            if k == 0 {
                                p.get().run_upload(u);
                            }
                        }
                        if let Some(c) = work.clut.as_ref() {
                            if k == 0 {
                                let g = p.get();
                                let mut v = match c.base.as_ref() {
                                    Some(b) => *b.get(),
                                    None => *c.out.get(),
                                };
                                g.read_clut_into(c.t, c.texclut, &mut v);
                                *c.out.get_mut() = v;
                            }
                        }
                        if let Some(d) = work.decode.as_ref() {
                            run_decode(p.get(), d, k as u32, n as u32);
                        }
                        if let Some(r) = work.readout.as_ref() {
                            if k == 0 {
                                r.run(p.get());
                            }
                        }
                        let t1 = std::time::Instant::now();
                        for q in work.items.iter() {
                            // Skip primitives without a row of this worker.
                            let (y0, y1) = (q.bbox.y0.max(0), q.bbox.y1);
                            let bd = band();
                            if y1 - y0 < bd * n as i32 {
                                let mut any = false;
                                let mut b = y0 / bd;
                                while b * bd < y1 {
                                    if b as u32 % n as u32 == k as u32 {
                                        any = true;
                                        break;
                                    }
                                    b += 1;
                                }
                                if !any {
                                    continue;
                                }
                            }
                            draw_queued(p.get(), q, &NO_CLIP);
                        }
                        TILE_US.fetch_add(t1.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
                        seq += 1;
                        done[k].store(seq, std::sync::atomic::Ordering::Release);
                    }
                })
                .expect("raster worker");
        }
        Workers { txs, done, sent: 0, flights: Default::default(), last_w: Box::new([0; 512]), last_r: Box::new([0; 512]), flight_info: Default::default(), inflight: [0; 8], last_key: None }
    }

    /// Record job `seq` (writes `w`, reads `r`); completed jobs are dropped from `flights`.
    fn record(&mut self, seq: u64, w: &[u64; 8], r: &[u64; 8], work: Option<std::sync::Arc<Work>>) {
        each_page(w, |p| self.last_w[p] = seq);
        each_page(r, |p| self.last_r[p] = seq);
        let done = self.min_done();
        while self.flights.front().is_some_and(|f| f.0 <= done) {
            self.flights.pop_front();
        }
        self.flights.push_back((seq, *w, *r, work));
    }

    /// The newest job writing (`w`) and / or reading (`r`) any of `pages` (0: none).
    fn newest(&self, pages: &[u64; 8], w: bool, r: bool) -> u64 {
        let mut m = 0;
        each_page(pages, |p| {
            if w {
                m = m.max(self.last_w[p]);
            }
            if r {
                m = m.max(self.last_r[p]);
            }
        });
        m
    }

    /// The last batch every worker has finished.
    fn min_done(&self) -> u64 {
        self.done.iter().map(|d| d.load(std::sync::atomic::Ordering::Acquire)).min().unwrap_or(0)
    }

    fn idle(&self) -> bool {
        self.done.iter().all(|d| d.load(std::sync::atomic::Ordering::Acquire) >= self.sent)
    }
}

/// A primitive waiting in the batch: its resolved state, vertices (after XYOFFSET) and the
/// pixel bounding box it can touch (scissored).
struct Queued {
    st: std::sync::Arc<Setup>,
    kind: u32,
    vs: [Vertex; 3],
    bbox: Clip,
    /// GPU: the CLUT slot it reads | clut16 << 31.
    clut: u32,
}

/// Primitives queued for drawing. All write the same frame / depth buffers (`key`), so pixels of
/// different screen tiles are different memory and the tiles can be drawn in parallel. The batch
/// is flushed before anything else touches local memory (transfers, CLUT loads, the display
/// read-out, decoding a texture whose pages it wrote).
#[derive(Default)]
pub struct Batch {
    items: Vec<Queued>,
    key: (u32, u32, u32, u32, u32, bool),
    area: i64,
    /// The frame and depth buffers overlap: draw in order on one core.
    serial: bool,
    /// Pages (of 512) the batch reads or writes as frame / depth buffer.
    pages: [u64; 8],
    /// Pages its primitives sample directly from local memory (textures being drawn).
    reads: [u64; 8],
    /// Raster batch every worker must have finished before this one starts (0: none).
    after: u64,
}

/// A batch as the workers get it.
pub struct Work {
    after: u64,
    items: Vec<Queued>,
    /// A transfer chunk worker 0 stores (no primitives then).
    upload: Option<crate::gs::Upload>,
    /// A CLUT load worker 0 does.
    clut: Option<crate::gs::ClutJob>,
    /// A texture level all workers decode (their own rows).
    decode: Option<DecodeJob>,
    /// The display read-out worker 0 does.
    readout: Option<crate::gs::ReadoutJob>,
}

/// Per-primitive state resolved from the registers.
pub(crate) struct Setup {
    tme: bool,
    gouraud: bool,
    fge: bool,
    abe: bool,
    fst: bool,
    ctx: Context,
    // FRAME / ZBUF
    fbp: u32,
    fbw: u32,
    fpsm: u32,
    fbmsk: u32,
    zbp: u32,
    zpsm: u32,
    zmsk: bool,
    // TEST
    ate: bool,
    atst: u32,
    aref: u32,
    afail: u32,
    date: bool,
    datm: bool,
    zte: bool,
    ztst: u32,
    // ALPHA
    aa: u32,
    ab: u32,
    ac: u32,
    ad: u32,
    afix: u32,
    pabe: bool,
    fba: bool,
    colclamp: bool,
    dthe: bool,
    dimx: [[i32; 4]; 4],
    fogcol: [u32; 3],
    // scissor (pixels)
    sx0: i32,
    sx1: i32,
    sy0: i32,
    sy1: i32,
    tex: Option<Tex>,
    /// Largest depth value of the Z format (for clamping).
    zmax: u32,
    /// The pixel function for this primitive's state (see `plot_fn`).
    plot: PlotFn,
}

#[derive(Clone)]
struct Tex {
    tbp: u32,
    tbw: u32,
    psm: u32,
    tw: u32,
    th: u32,
    tcc: bool,
    tfx: u32,
    wms: u32,
    wmt: u32,
    minu: i32,
    maxu: i32,
    minv: i32,
    maxv: i32,
    aem: bool,
    ta0: u32,
    ta1: u32,
    mmag: bool,
    mmin: u32,
    mxl: u32,
    lcm: bool,
    l: u32,
    /// K (signed 7.4).
    k16: i32,
    mip: [(u32, u32); 7],
    /// Decoded levels (RGBA8 after CLUT / TEXA), from the texture cache.
    img: [Option<std::sync::Arc<TexImg>>; 7],
    /// (filter, level) when the LOD does not depend on Q (LCM, or no mipmaps and the same
    /// filter both ways): fixed for the primitive.
    fixed_lod: Option<(bool, usize)>,
    /// Sampled straight from local memory (a CT32 / CT24 / CT16 texture whose pages are still
    /// being drawn): its pages; `mem` = the address of local memory. The workers order the
    /// reads after the drawing (`Work::after`), the GS thread does not wait.
    direct: Option<[u64; 8]>,
    mem: usize,
    /// Raster job that decodes its levels (0: decoded already).
    dep: u64,
}

/// The state for the kicked primitive: the previous one's while every register it depends on is
/// unchanged and its textures' pages were not written since.
fn cached_setup(gs: &mut Gs) -> std::sync::Arc<Setup> {
    let a = gs.attrs();
    let c = &gs.ctx[((a >> 9) & 1) as usize];
    let key = [
        a, c.tex0, c.clamp, c.tex1, c.xyoffset, c.miptbp1, c.miptbp2, c.scissor, c.alpha, c.test, c.fba, c.frame, c.zbuf, gs.dimx, gs.fogcol,
        gs.pabe | gs.colclamp << 1 | gs.dthe << 2, gs.texa, gs.clut_hash, 0,
    ];
    if let Some((k, st)) = gs.st_cache.as_ref() {
        if *k == key && gs.st_valid.load(std::sync::atomic::Ordering::Relaxed) {
            return st.clone();
        }
    }
    let st = std::sync::Arc::new(setup(gs));
    // The pages its texture levels cover: a write there invalidates the cached state.
    let mut pages = [0u64; 8];
    if let Some(t) = st.tex.as_ref() {
        let levels = if t.mxl > 0 && t.mmin >= 2 { (t.mxl as usize).min(6) + 1 } else { 1 };
        for l in 0..levels {
            let (first, n) = tex_pages(t, l);
            for i in 0..n.min(512) {
                let p = (first + i) & 511;
                pages[p >> 6] |= 1 << (p & 63);
            }
        }
    }
    gs.st_pages = pages;
    gs.st_valid.store(true, std::sync::atomic::Ordering::Relaxed);
    gs.st_cache = Some((key, st.clone()));
    st
}

/// First page and page count a texture level covers (as `cached_level` checks them).
fn tex_pages(t: &Tex, l: usize) -> (usize, usize) {
    let (tbp, tbw) = if l == 0 { (t.tbp, t.tbw) } else { t.mip[l] };
    let w = 1u32 << t.tw.saturating_sub(l as u32).min(10);
    let h = 1u32 << t.th.saturating_sub(l as u32).min(10);
    let (pw, ph) = page_dims(t.psm);
    let stride = match t.psm { PSMT8 | PSMT4 => (tbw / 2).max(1), _ => tbw.max(1) }.max(w.div_ceil(pw));
    ((tbp / 32) as usize, (stride * h.div_ceil(ph)) as usize)
}

fn setup(gs: &mut Gs) -> Setup {
    let a = gs.attrs();
    let ctx = gs.ctx[((a >> 9) & 1) as usize];
    let frame = ctx.frame;
    let zbuf = ctx.zbuf;
    let test = ctx.test;
    let alpha = ctx.alpha;
    let sc = ctx.scissor;
    let mut dimx = [[0i32; 4]; 4];
    for y in 0..4 {
        for x in 0..4 {
            let v = ((gs.dimx >> (16 * y + 4 * x)) & 7) as i32;
            dimx[y][x] = if v & 4 != 0 { v - 8 } else { v };
        }
    }
    let tme = a & 0x10 != 0;
    let tex = tme.then(|| {
        let t0 = ctx.tex0;
        let t1 = ctx.tex1;
        let cl = ctx.clamp;
        let mut mip = [(0u32, 0u32); 7];
        mip[0] = ((t0 & 0x3FFF) as u32, ((t0 >> 14) & 0x3F) as u32);
        for i in 0..3 {
            mip[1 + i] = (((ctx.miptbp1 >> (20 * i)) & 0x3FFF) as u32, ((ctx.miptbp1 >> (20 * i + 14)) & 0x3F) as u32);
            mip[4 + i] = (((ctx.miptbp2 >> (20 * i)) & 0x3FFF) as u32, ((ctx.miptbp2 >> (20 * i + 14)) & 0x3F) as u32);
        }
        let k = ((t1 >> 32) & 0xFFF) as i32;
        let k = if k & 0x800 != 0 { k - 0x1000 } else { k };
        Tex {
            tbp: (t0 & 0x3FFF) as u32,
            tbw: ((t0 >> 14) & 0x3F) as u32,
            psm: ((t0 >> 20) & 0x3F) as u32,
            tw: ((t0 >> 26) & 0xF) as u32,
            th: ((t0 >> 30) & 0xF) as u32,
            tcc: (t0 >> 34) & 1 != 0,
            tfx: ((t0 >> 35) & 3) as u32,
            wms: (cl & 3) as u32,
            wmt: ((cl >> 2) & 3) as u32,
            minu: ((cl >> 4) & 0x3FF) as i32,
            maxu: ((cl >> 14) & 0x3FF) as i32,
            minv: ((cl >> 24) & 0x3FF) as i32,
            maxv: ((cl >> 34) & 0x3FF) as i32,
            aem: (gs.texa >> 15) & 1 != 0,
            ta0: (gs.texa & 0xFF) as u32,
            ta1: ((gs.texa >> 32) & 0xFF) as u32,
            mmag: (t1 >> 5) & 1 != 0,
            mmin: ((t1 >> 6) & 7) as u32,
            mxl: ((t1 >> 2) & 7) as u32,
            lcm: t1 & 1 != 0,
            l: ((t1 >> 19) & 3) as u32,
            k16: k,
            mip,
            img: Default::default(),
            fixed_lod: None,
            direct: None,
            mem: 0,
            dep: 0,
        }
    });
    let mut tex = tex;
    if let Some(t) = tex.as_mut() {
        t.fixed_lod = lod_select(t, None);
        let levels = if t.mxl > 0 && t.mmin >= 2 { (t.mxl as usize).min(6) + 1 } else { 1 };
        // Small render targets (a 256x256 shadow map) are decoded: the snapshot waits only for
        // their short pass and leaves later writes to their memory free; large ones (the frame
        // buffer read back) are sampled directly.
        static MIN: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        let min = *MIN.get_or_init(|| std::env::var("T5_DIRECTMIN").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
        let big = (1u32 << t.tw.min(10)) * (1u32 << t.th.min(10)) >= min;
        if gs.gpu.is_some() {
            // The GPU samples every texture straight from local memory.
            let mut dp = [0u64; 8];
            for l in 0..levels {
                let (first, n) = tex_pages(t, l);
                for i in 0..n.min(512) {
                    let p = (first + i) & 511;
                    dp[p >> 6] |= 1 << (p & 63);
                }
            }
            t.direct = Some(dp);
        } else if big && matches!(t.psm, PSMCT32 | PSMCT24 | PSMCT16 | PSMCT16S) && !crate::diag::enabled("T5_NODIRECT") && !safe() {
            let mut dp = [0u64; 8];
            for l in 0..levels {
                let (first, n) = tex_pages(t, l);
                for i in 0..n.min(512) {
                    let p = (first + i) & 511;
                    dp[p >> 6] |= 1 << (p & 63);
                }
            }
            if pages_drawn(gs, &dp) {
                t.direct = Some(dp);
                t.mem = gs.mem.as_ptr() as usize;
            }
        }
        if t.direct.is_none() {
            for l in 0..levels {
                let (img, dep) = cached_level(gs, t, l);
                t.img[l] = Some(img);
                t.dep = t.dep.max(dep);
            }
        }
    }
    Setup {
        tme,
        gouraud: a & 8 != 0,
        fge: a & 0x20 != 0,
        abe: a & 0x40 != 0,
        fst: a & 0x100 != 0,
        ctx,
        fbp: ((frame & 0x1FF) * 32) as u32,
        fbw: ((frame >> 16) & 0x3F) as u32,
        fpsm: ((frame >> 24) & 0x3F) as u32,
        fbmsk: (frame >> 32) as u32,
        zbp: ((zbuf & 0x1FF) * 32) as u32,
        zpsm: (((zbuf >> 24) & 0xF) | 0x30) as u32,
        zmsk: (zbuf >> 32) & 1 != 0,
        ate: test & 1 != 0,
        atst: ((test >> 1) & 7) as u32,
        aref: ((test >> 4) & 0xFF) as u32,
        afail: ((test >> 12) & 3) as u32,
        date: (test >> 14) & 1 != 0,
        datm: (test >> 15) & 1 != 0,
        zte: (test >> 16) & 1 != 0,
        ztst: ((test >> 17) & 3) as u32,
        aa: (alpha & 3) as u32,
        ab: ((alpha >> 2) & 3) as u32,
        ac: ((alpha >> 4) & 3) as u32,
        ad: ((alpha >> 6) & 3) as u32,
        afix: ((alpha >> 32) & 0xFF) as u32,
        pabe: gs.pabe & 1 != 0,
        fba: ctx.fba & 1 != 0,
        colclamp: gs.colclamp & 1 != 0,
        dthe: gs.dthe & 1 != 0,
        dimx,
        fogcol: [(gs.fogcol & 0xFF) as u32, ((gs.fogcol >> 8) & 0xFF) as u32, ((gs.fogcol >> 16) & 0xFF) as u32],
        sx0: (sc & 0x7FF) as i32,
        sx1: ((sc >> 16) & 0x7FF) as i32,
        sy0: ((sc >> 32) & 0x7FF) as i32,
        sy1: ((sc >> 48) & 0x7FF) as i32,
        tex,
        zmax: match (((zbuf >> 24) & 0xF) | 0x30) as u32 {
            PSMZ32 => u32::MAX,
            PSMZ24 => 0xFF_FFFF,
            _ => 0xFFFF,
        },
        plot: plot_fn(tme, test & 1 != 0, (test >> 16) & 1 != 0, a & 0x40 != 0, a & 0x20 != 0),
    }
}

/// Interpolated attributes at a pixel. All integer: the GPU rasterizer computes the same values.
#[derive(Clone, Copy, Default)]
struct Frag {
    z: u32,
    c: [u32; 4],
    f: u32,
    /// STQ: S, T, Q in fixed point with `qs` fraction bits. UV (FST): U, V (1/16 texel) << PF.
    s: i64,
    t: i64,
    q: i64,
    qs: i32,
}

/// Fraction bits of the attribute planes.
const PF: u32 = 16;

/// A linear attribute: raw(dx, dy) = c0 + cx·dx + cy·dy (wrapping), dx / dy relative to the
/// primitive's origin. Colours, fog, depth, UV are raw >> PF; S, T, Q stay raw.
#[derive(Clone, Copy, Default)]
pub(crate) struct Plane {
    c0: i64,
    cx: i64,
    cy: i64,
}

impl Plane {
    fn flat(v: i64) -> Plane {
        Plane { c0: v << PF, cx: 0, cy: 0 }
    }

    #[inline(always)]
    fn at(&self, dx: i64, dy: i64) -> i64 {
        self.c0.wrapping_add(self.cx.wrapping_mul(dx)).wrapping_add(self.cy.wrapping_mul(dy))
    }
}

/// The attribute planes of one primitive: z, r, g, b, a, fog, s, t, q.
#[derive(Clone, Copy, Default)]
pub(crate) struct Interp {
    pub p: [Plane; 9],
    qs: i32,
}

impl Interp {
    #[inline(always)]
    fn raw(&self, dx: i64, dy: i64) -> [i64; 9] {
        self.p.map(|p| p.at(dx, dy))
    }

    #[inline(always)]
    fn step_x(&self, raw: &mut [i64; 9]) {
        for i in 0..9 {
            raw[i] = raw[i].wrapping_add(self.p[i].cx);
        }
    }

    #[inline(always)]
    fn frag(&self, raw: &[i64; 9], st: &Setup) -> Frag {
        let c = |i: usize| (raw[i] >> PF).clamp(0, 255) as u32;
        Frag { z: (raw[0] >> PF).clamp(0, st.zmax as i64) as u32, c: [c(1), c(2), c(3), c(4)], f: c(5), s: raw[6], t: raw[7], q: raw[8], qs: self.qs }
    }
}

/// floor(n / d) for d != 0.
#[inline(always)]
fn div_floor(n: i128, d: i128) -> i64 {
    n.div_euclid(d) as i64 - if d < 0 && n.rem_euclid(d) != 0 { 1 } else { 0 }
}

/// floor(n / d) for d != 0 in wrapping i64 (the GPU's arithmetic).
#[inline(always)]
pub fn div_floor64(n: i64, d: i64) -> i64 {
    let q = n.wrapping_div(d);
    if (n.wrapping_rem(d) != 0) && ((n < 0) != (d < 0)) { q.wrapping_sub(1) } else { q }
}

/// The vertices' attribute values as integers: z, r, g, b, a, fog, s, t, q, and the STQ
/// fraction bits. STQ get a common exponent so the largest magnitude is just below 2^31; UV
/// (FST) are their 12.4 values.
fn attr_vals(vs: &[&Vertex], st: &Setup) -> ([[i64; 9]; 3], i32) {
    let mut e = 0i32;
    if !st.fst {
        let mut m = 0f32;
        for v in vs {
            for x in [v.s, v.t, v.q] {
                // Finite: exponent not all ones.
                if x.to_bits() & 0x7F80_0000 != 0x7F80_0000 {
                    m = m.max(x.abs());
                }
            }
        }
        if m > 0.0 {
            let exp = ((m.to_bits() >> 23) & 0xFF) as i32 - 127;
            e = 30 - exp;
        }
    }
    let scale = 2f64.powi(e);
    let fx = |x: f32| if x.to_bits() & 0x7F80_0000 != 0x7F80_0000 { (x as f64 * scale) as i64 } else { 0 };
    let mut vals = [[0i64; 9]; 3];
    for (k, v) in vs.iter().enumerate() {
        let (s, t, q) = if st.fst { ((v.u as i64) << PF, (v.v as i64) << PF, 0) } else { (fx(v.s), fx(v.t), fx(v.q)) };
        vals[k] = [v.z as i64, v.r as i64, v.g as i64, v.b as i64, v.a as i64, v.f as i64, s, t, q];
    }
    (vals, if st.fst { 0 } else { e + PF as i32 })
}

/// Which attributes are raw already (S, T, Q, UV) and which get PF fraction bits.
const RAW_ATTR: [bool; 9] = [false, false, false, false, false, false, true, true, true];


pub fn draw(gs: &mut Gs, kind: u32) {
    // Measurement only: what the frame costs with free rasterization.
    if crate::diag::enabled("T5_NODRAW") {
        return;
    }
    queue(gs, kind);
}

/// GS thread time (µs): queue (setup, texture decode, flushes it triggers), parallel flushes,
/// serial flushes, image transfers, display read-out.
pub static GS_US: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];
/// Pixels inside drawn spans (sprites, triangles).
pub static COVERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Summed tile work in parallel flushes (µs, over all threads) and tiles drawn.
pub static TILE_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static TILES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Batches flushed (parallel, serial) and their primitives.
pub static FLUSHES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Resolve the kicked primitive and queue it.
fn queue(gs: &mut Gs, kind: u32) {
    PRIMS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // A primitive with an indexed texture reads the CLUT: a deferred load must be done.
    if gs.pending_clut.is_some() && gs.attrs() & 0x10 != 0 {
        let a = gs.attrs();
        let psm = ((gs.ctx[((a >> 9) & 1) as usize].tex0 >> 20) & 0x3F) as u32;
        if matches!(psm, PSMT8 | PSMT8H | PSMT4 | PSMT4HL | PSMT4HH) {
            gs.realize_clut();
        }
    }
    let st = cached_setup(gs);
    let ofx = (st.ctx.xyoffset & 0xFFFF) as i32;
    let ofy = ((st.ctx.xyoffset >> 32) & 0xFFFF) as i32;
    let n = match kind { 0 => 1, 1 | 2 | 6 => 2, _ => 3 };
    let mut vs = [Vertex::default(); 3];
    for (i, v) in gs.verts.iter().rev().take(n).rev().enumerate() {
        vs[i] = *v;
        vs[i].x -= ofx;
        vs[i].y -= ofy;
    }
    let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
    for v in &vs[..n] {
        x0 = x0.min(v.x >> 4);
        x1 = x1.max((v.x + 15) >> 4);
        y0 = y0.min(v.y >> 4);
        y1 = y1.max((v.y + 15) >> 4);
    }
    // Game frames: a sprite pass starting at the top-left corner of the displayed buffer.
    if kind == 6 && x0 <= 0 && y0 <= 0 && st.fbp == ((gs.dispfb[1] & 0x1FF) * 32) as u32 && st.tme {
        GAME_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A copy pass onto the displayed buffer: its source was drawn anew since the last copy
        // = one real game frame (the copies themselves repeat every field, shifted for
        // interlacing, so displayed pictures always differ).
        if let Some(t) = st.tex.as_ref() {
            let (first, n) = tex_pages(t, 0);
            let mut sum = first as u64;
            for i in 0..n.min(512) {
                sum = sum.wrapping_mul(31).wrapping_add(gs.page_gen[(first + i) & 511].load(std::sync::atomic::Ordering::Relaxed) as u64);
            }
            let changed = LAST_COPY_SRC.swap(sum, std::sync::atomic::Ordering::Relaxed) != sum;
            if changed && crate::diag::enabled("T5_DUPCHECK") {
                dup_report();
            }
            crate::diag::tl(if changed { "C new" } else { "C same" });
            if changed {
                RENDERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                RENDERED_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    if crate::diag::enabled("T5_STATELOG") {
        let t = st.tex.as_ref();
        crate::diag::count_with(|| format!(
            "st_k{}_tme{}_tfx{}_tcc{}_flt{}_psm{:x}_fst{}_abe{}_al{:x}_ate{}_atst{}_zte{}_ztst{}_zmsk{}_fpsm{:x}_zpsm{:x}_fge{}_iip{}_msk{:x}_mxl{}_lcm{}_mmin{}_wm{}{}_dthe{}_fba{}_pabe{}_colclamp{}",
            kind, st.tme as u8, t.map_or(9, |t| t.tfx), t.map_or(9, |t| t.tcc as u32), t.map_or(9, |t| t.mmag as u32), t.map_or(0, |t| t.psm),
            st.fst as u8, st.abe as u8, st.ctx.alpha & 0xFF, st.ate as u8, st.atst, st.zte as u8, st.ztst, st.zmsk as u8, st.fpsm, st.zpsm, st.fge as u8, st.gouraud as u8, st.fbmsk,
            t.map_or(9, |t| t.mxl), t.map_or(9, |t| t.lcm as u32), t.map_or(9, |t| t.mmin), t.map_or(9, |t| t.wms), t.map_or(9, |t| t.wmt),
            st.dthe as u8, st.fba as u8, st.pabe as u8, st.colclamp as u8
        ));
    }
    if crate::diag::enabled("T5_TEXLOG") {
        if let Some(t) = st.tex.as_ref() {
            let v = &vs[n - 1];
            crate::diag::once(format!("TEX k{} fbp {:#x} tbp {:#x} tbw {} psm {:#x} {}x{} tcc {} tfx {} cbp {:#x} cpsm {:#x} csm {} csa {} texa {:#x} abe {} alpha {:#x} rgba ({},{},{},{}) fst {}",
                kind, st.fbp, t.tbp, t.tbw, t.psm, 1 << t.tw, 1 << t.th, t.tcc as u8, t.tfx, (gs.clut_tex0 >> 37) & 0x3FFF, (gs.clut_tex0 >> 51) & 0xF, (gs.clut_tex0 >> 55) & 1, (gs.clut_tex0 >> 56) & 0x1F, gs.texa, st.abe as u8, st.ctx.alpha, v.r, v.g, v.b, v.a, st.fst as u8));
        }
    }
    if crate::diag::enabled("T5_PRIMSTATS") {
        crate::diag::count_with(|| format!("prim_k{}_{}x{}_tme{}_abe{}_fbp{:x}_tbp{:x}", kind, x1 - x0, y1 - y0, st.tme as u8, st.abe as u8, st.fbp, st.tex.as_ref().map(|t| t.tbp).unwrap_or(0)));
    }
    // Lines and points can reach one pixel past their end points.
    let bbox = Clip {
        x0: (x0 - 1).max(st.sx0).max(0),
        y0: (y0 - 1).max(st.sy0).max(0),
        x1: (x1 + 1).min(st.sx1 + 1).min(2048),
        y1: (y1 + 1).min(st.sy1 + 1).min(2048),
    };
    if bbox.x0 >= bbox.x1 || bbox.y0 >= bbox.y1 {
        return;
    }
    let zwrite = st.zte && !st.zmsk;
    // Depth writes on / off do not split batches (opaque and blended primitives alternate);
    // a batch is serial if any of its primitives writes depth into overlapping FB / Z pages.
    let key = (st.fbp, st.fbw, st.fpsm, st.zbp, st.zpsm, true);
    if gs.batch.key != key || gs.batch.items.len() >= MAX_BATCH {
        flush(gs);
        gs.batch.key = key;
        gs.batch.serial = false;
    }
    // Pages the primitive writes (frame, depth) and samples directly (a texture being drawn).
    let mut wp = [0u64; 8];
    Gs::for_pages(st.fpsm, st.fbp, st.fbw, bbox.x0 as u32, bbox.y0 as u32, bbox.x1 as u32, bbox.y1 as u32, |p| {
        wp[p >> 6] |= 1 << (p & 63);
        true
    });
    if st.zte {
        Gs::for_pages(st.zpsm, st.zbp, st.fbw, bbox.x0 as u32, bbox.y0 as u32, bbox.x1 as u32, bbox.y1 as u32, |p| {
            wp[p >> 6] |= 1 << (p & 63);
            true
        });
    }
    // Pages it writes (frame unless fully masked, depth if written).
    let mut ww = [0u64; 8];
    if st.fbmsk != 0xFFFF_FFFF {
        Gs::for_pages(st.fpsm, st.fbp, st.fbw, bbox.x0 as u32, bbox.y0 as u32, bbox.x1 as u32, bbox.y1 as u32, |p| {
            ww[p >> 6] |= 1 << (p & 63);
            true
        });
    }
    if st.zte && !st.zmsk {
        Gs::for_pages(st.zpsm, st.zbp, st.fbw, bbox.x0 as u32, bbox.y0 as u32, bbox.x1 as u32, bbox.y1 as u32, |p| {
            ww[p >> 6] |= 1 << (p & 63);
            true
        });
    }
    // Pages it samples directly: for UV coordinates the texel box its vertices span (one texel
    // more each way for bilinear), else the whole texture.
    let rp = st.tex.as_ref().and_then(|t| {
        let full = t.direct?;
        let single = !(t.mxl > 0 && t.mmin >= 2);
        if st.fst && single {
            let (mut u0, mut u1, mut v0, mut v1) = (u32::MAX, 0u32, u32::MAX, 0u32);
            for v in &vs[..n] {
                u0 = u0.min(v.u >> 4);
                u1 = u1.max((v.u + 15) >> 4);
                v0 = v0.min(v.v >> 4);
                v1 = v1.max((v.v + 15) >> 4);
            }
            let (x0, y0, x1, y1) = (u0.saturating_sub(1), v0.saturating_sub(1), u1 + 2, v1 + 2);
            let (w, h) = (1u32 << t.tw.min(10), 1u32 << t.th.min(10));
            // Clamp modes keep coordinates inside; repeat inside the texture is the box too.
            if x1 <= w && y1 <= h || (t.wms == 1 && t.wmt == 1) {
                let mut r = [0u64; 8];
                Gs::for_pages(t.psm, t.tbp, t.tbw, x0, y0, x1.min(w), y1.min(h), |p| {
                    r[p >> 6] |= 1 << (p & 63);
                    true
                });
                return Some(r);
            }
        }
        Some(full)
    });
    if crate::diag::enabled("T5_PASSLOG") && st.fbp == 0x2d60 {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 400 {
            let t = st.tex.as_ref();
            crate::diag::log(format!("PASS k{} fbw{} bbox {},{}-{},{} tex {:x?} uv {:?} direct {} abe {} zte {}", kind, st.fbw, bbox.x0, bbox.y0, bbox.x1, bbox.y1,
                t.map(|t| (t.tbp, t.tbw, t.psm, 1 << t.tw, 1 << t.th)), vs[..n].iter().map(|v| (v.u >> 4, v.v >> 4, v.s, v.t)).collect::<Vec<_>>(), rp.is_some(), st.abe, st.zte));
        }
    }
    if rp.is_some_and(|r| (0..8).any(|i| r[i] & wp[i] != 0)) {
        crate::diag::once(format!("direct self-feedback fbp {:x} tbp {:x}", st.fbp, st.tex.as_ref().unwrap().tbp));
    }
    let hit = |a: &[u64; 8], b: &[u64; 8]| (0..8).any(|i| a[i] & b[i] != 0);
    // Reading what this batch draws, or drawing what it reads: the rows of different workers
    // would not be in order, so the earlier part goes first as a batch of its own.
    if !gs.batch.items.is_empty() && (rp.is_some_and(|r| hit(&r, &gs.batch.pages)) || hit(&ww, &gs.batch.reads)) {
        flush(gs);
        gs.batch.key = key;
        gs.batch.serial = false;
    }
    if let Some(r) = rp {
        let a = newest_flight(gs, &r, false);
        gs.batch.after = gs.batch.after.max(a);
        for i in 0..8 {
            gs.batch.reads[i] |= r[i];
        }
    }
    let a = newest_flight(gs, &ww, true);
    gs.batch.after = gs.batch.after.max(a);
    // A texture decoded by a raster job: after it.
    if let Some(t) = st.tex.as_ref() {
        gs.batch.after = gs.batch.after.max(t.dep);
    }
    // Frame and depth buffer pages overlapping within the scissor rectangle: one pixel's depth
    // may be another's colour, so the batch is drawn in order on one core.
    if zwrite && !gs.batch.serial {
        let rows = |psm: u32| -> u32 {
            let ph = if matches!(psm, PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S) { 64 } else { 32 };
            (st.sy1 as u32 + 1).div_ceil(ph)
        };
        // Blocks (32 per page) each buffer spans: width in pages x page rows.
        let fb_end = st.fbp + st.fbw.max(1) * rows(st.fpsm) * 32;
        let z_end = st.zbp + st.fbw.max(1) * rows(st.zpsm) * 32;
        gs.batch.serial = st.fbp < z_end && st.zbp < fb_end;
    }
    // The pages the primitive writes are busy until drawn.
    for i in 0..8 {
        gs.batch.pages[i] |= wp[i];
    }
    // The pages the primitive may write change now (texture cache checks happen at queue time).
    if st.fbmsk != 0xFFFF_FFFF {
        gs.touch_rect(st.fpsm, st.fbp, st.fbw, bbox.x0 as u32, bbox.y0 as u32, bbox.x1 as u32, bbox.y1 as u32);
    }
    if zwrite {
        gs.touch_rect(st.zpsm, st.zbp, st.fbw, bbox.x0 as u32, bbox.y0 as u32, bbox.x1 as u32, bbox.y1 as u32);
    }
    gs.batch.area += ((bbox.x1 - bbox.x0) as i64) * ((bbox.y1 - bbox.y0) as i64);
    if crate::diag::enabled("T5_DUPCHECK") {
        dup_note(&st, kind, &vs[..n]);
    }
    let clut = gs.gpu.as_ref().map_or(0, |g| {
        let cpsm = ((gs.clut_tex0 >> 51) & 0xF) as u32;
        g.clut_slot | ((cpsm == PSMCT16 || cpsm == PSMCT16S) as u32) << 31
    });
    gs.batch.items.push(Queued { st, kind, vs, bbox, clut });
}

/// Draw the queued primitives.
/// Draw the queued primitives and wait until they are in local memory (before anything reads
/// it: texture decoding, CLUT loads, transfers, the display read-out).
#[track_caller]
pub fn flush_sync(gs: &mut Gs) {
    flush(gs);
    if gs.gpu.is_some() {
        return;
    }
    wait(gs);
    gs.resolve_clut();
}

/// Wait for the batch drawing in the background, if any.
#[track_caller]
pub fn wait(gs: &mut Gs) {
    let Some(w) = gs.workers.as_mut() else { return };
    if !w.idle() {
        crate::diag::tl("R wait");
        let loc = std::panic::Location::caller();
        crate::diag::count_with(|| format!("rwait_{}:{}", loc.file(), loc.line()));
        let t0 = std::time::Instant::now();
        while !w.idle() {
            std::thread::yield_now();
        }
        crate::diag::tl("R waited");
        if crate::diag::counting() {
            crate::diag::add(&format!("rwaitus_{}:{}", loc.file().rsplit(['/', '\\']).next().unwrap_or(""), loc.line()), t0.elapsed().as_micros() as u64);
        }
        GS_US[1].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    w.inflight = [0; 8];
    w.flights.clear();
}

/// Whether queued or in-flight drawing writes `pages`.
fn pages_drawn(gs: &mut Gs, pages: &[u64; 8]) -> bool {
    let hit = |a: &[u64; 8]| (0..8).any(|i| a[i] & pages[i] != 0);
    if !gs.batch.items.is_empty() && hit(&gs.batch.pages) {
        return true;
    }
    let Some(w) = gs.workers.as_ref() else { return false };
    w.newest(pages, true, false) > w.min_done()
}

/// The newest in-flight batch that writes (`reads` false) or samples (`reads` true) `pages`.
fn newest_flight(gs: &mut Gs, pages: &[u64; 8], reads: bool) -> u64 {
    let hit = |a: &[u64; 8]| (0..8).any(|i| a[i] & pages[i] != 0);
    let _ = hit;
    let Some(w) = gs.workers.as_ref() else { return 0 };
    w.newest(pages, !reads, reads)
}

/// The newest raster batch in flight that uses `pages` (after sending the queued batch if it
/// does), or None.
#[track_caller]
pub fn flight_using(gs: &mut Gs, pages: &[u64; 8]) -> Option<u64> {
    flight_using_blocks(gs, pages, None)
}

/// `flight_using`, refined to the 256-byte blocks `blocks` (sorted) where pages collide: a
/// batch counts only if one of its primitives (or its transfer) covers one of those blocks.
#[track_caller]
pub fn flight_using_blocks(gs: &mut Gs, pages: &[u64; 8], _blocks: Option<&[u32]>) -> Option<u64> {
    let hit = |a: &[u64; 8]| (0..8).any(|i| a[i] & pages[i] != 0);
    if !gs.batch.items.is_empty() && (hit(&gs.batch.pages) || hit(&gs.batch.reads)) {
        flush(gs);
    }
    let w = gs.workers.as_ref()?;
    let m = w.newest(pages, true, true);
    (m > w.min_done()).then_some(m)
}

/// Whether in-flight batch `seq` is a transfer (diagnostics).
pub fn flight_is_upload(gs: &Gs, seq: u64) -> bool {
    gs.workers.as_ref().and_then(|w| w.flights.iter().find(|f| f.0 == seq)).and_then(|f| f.3.as_ref()).is_some_and(|w| w.upload.is_some())
}

/// Block number (256 bytes) of pixel (x, y) of a buffer.
fn block_of(psm: u32, bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    use crate::gs::{addr16, addr32, addr4, addr8};
    let a = match psm {
        PSMT4 => addr4(bp, bw, x, y) >> 1,
        PSMT8 => addr8(bp, bw, x, y),
        PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => addr16(bp, bw, x, y, psm),
        _ => addr32(bp, bw, x, y, psm & 0x30 == 0x30),
    };
    (a >> 8) as u32
}

/// The blocks a rectangle of a buffer covers, sorted.
pub fn rect_blocks(psm: u32, bp: u32, bw: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<u32> {
    let (bx, by) = match psm {
        PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => (16, 8),
        PSMT8 => (16, 16),
        PSMT4 => (32, 16),
        _ => (8, 8),
    };
    let mut v = Vec::new();
    let mut y = y0 - y0 % by;
    while y < y1 {
        let mut x = x0 - x0 % bx;
        while x < x1 {
            v.push(block_of(psm, bp, bw, x.max(x0) & 2047, y.max(y0) & 2047));
            x += bx;
        }
        y += by;
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// The blocks a CSM1 CLUT load (TEX0 `t`) reads.
pub fn clut_blocks(t: u64) -> Vec<u32> {
    let cbp = ((t >> 37) & 0x3FFF) as u32;
    let cpsm = ((t >> 51) & 0xF) as u32;
    rect_blocks(if cpsm == 0 { PSMCT32 } else { cpsm }, cbp, 1, 0, 0, 16, 16)
}

/// A transfer chunk into pages batch `after` (or earlier ones) still uses: stored by worker 0
/// once every worker has passed `after`; later users of `pages` order after it.
pub fn submit_upload(gs: &mut Gs, up: crate::gs::Upload, pages: [u64; 8], after: u64) {
    let Some(w) = gs.workers.as_mut() else { return };
    w.sent += 1;
    let seq = w.sent;
    let work = std::sync::Arc::new(Work { after, items: Vec::new(), upload: Some(up), clut: None, decode: None, readout: None });
    w.record(seq, &pages, &[0; 8], Some(work.clone()));
    for tx in &w.txs {
        let _ = tx.send(work.clone());
    }
}

/// A CLUT load for raster worker 0 (reads `src` pages after batch `after`).
pub fn submit_clut(gs: &mut Gs, job: crate::gs::ClutJob, src: [u64; 8], after: u64) -> u64 {
    if gs.workers.is_none() {
        gs.workers = Some(Box::new(Workers::start(gs)));
    }
    let w = gs.workers.as_mut().unwrap();
    w.sent += 1;
    let seq = w.sent;
    let work = std::sync::Arc::new(Work { after, items: Vec::new(), upload: None, clut: Some(job), decode: None, readout: None });
    w.record(seq, &[0; 8], &src, Some(work.clone()));
    for tx in &w.txs {
        let _ = tx.send(work.clone());
    }
    seq
}

/// The display read-out for raster worker 0 (reads `src` pages after batch `after`).
pub fn submit_readout(gs: &mut Gs, job: crate::gs::ReadoutJob, src: [u64; 8], after: u64) {
    let w = gs.workers.as_mut().unwrap();
    w.sent += 1;
    let seq = w.sent;
    let work = std::sync::Arc::new(Work { after, items: Vec::new(), upload: None, clut: None, decode: None, readout: Some(job) });
    w.record(seq, &[0; 8], &src, Some(work.clone()));
    for tx in &w.txs {
        let _ = tx.send(work.clone());
    }
}

/// A texture decode for all raster workers; its sequence number (readers wait for it).
fn submit_decode(gs: &mut Gs, job: DecodeJob, after: u64) -> u64 {
    if gs.workers.is_none() {
        gs.workers = Some(Box::new(Workers::start(gs)));
    }
    let src = {
        let t = &job.tex;
        let w = job.img.w;
        let h = job.img.h;
        let mut m = [0u64; 8];
        Gs::for_pages(t.psm, job.tbp, job.tbw, 0, 0, w, h, |p| {
            m[p >> 6] |= 1 << (p & 63);
            true
        });
        m
    };
    let w = gs.workers.as_mut().unwrap();
    w.sent += 1;
    let seq = w.sent;
    let work = std::sync::Arc::new(Work { after, items: Vec::new(), upload: None, clut: None, decode: Some(job), readout: None });
    w.record(seq, &[0; 8], &src, Some(work.clone()));
    for tx in &w.txs {
        let _ = tx.send(work.clone());
    }
    seq
}

/// Wait until every worker has finished raster batch `seq`.
pub fn wait_seq(gs: &mut Gs, seq: u64) {
    let Some(w) = gs.workers.as_mut() else { return };
    if w.min_done() >= seq {
        return;
    }
    let t0 = std::time::Instant::now();
    crate::diag::tl("R wait");
    while w.min_done() < seq {
        std::thread::yield_now();
    }
    crate::diag::tl("R waited");
    crate::diag::add("rwaitus_clut_deferred", t0.elapsed().as_micros() as u64);
    GS_US[1].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Make sure no queued or running drawing reads or writes `pages` (PCSX2 GSRendererSW
/// InvalidateVideoMem / CheckSourcePages): the queue is sent if its batch uses one of them, then
/// only the in-flight batches that do are waited for.
#[track_caller]
pub fn sync_pages(gs: &mut Gs, pages: &[u64; 8]) {
    let hit = |a: &[u64; 8]| (0..8).any(|i| a[i] & pages[i] != 0);
    if gs.pending_clut.is_some_and(|p| hit(&p.2)) {
        gs.realize_clut();
    }
    if !gs.batch.items.is_empty() && (hit(&gs.batch.pages) || hit(&gs.batch.reads)) {
        flush(gs);
    }
    let Some(w) = gs.workers.as_mut() else { return };
    let need = w.newest(pages, true, true);
    if need <= w.min_done() {
        return;
    }
    if crate::diag::enabled("T5_SYNCLOG") {
        let loc = std::panic::Location::caller();
        let pg: Vec<usize> = (0..512).filter(|p| pages[p >> 6] & (1 << (p & 63)) != 0).collect();
        let hits: Vec<String> = w.flights.iter().filter(|f| hit(&f.1)).filter_map(|f| w.flight_info.iter().find(|i| i.0 == f.0)).map(|i| format!("fbp{:x}/fbw{}/z{:x}:{},{}-{},{}", i.1 .0, i.1 .1, i.1 .3, i.2 .0, i.2 .1, i.2 .2, i.2 .3)).collect();
        crate::diag::count_with(|| format!("synchit_{}_pages{}_by_{}", loc.line(), pg.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("+"), hits.join("|")));
    }
    let t0 = std::time::Instant::now();
    crate::diag::tl("R wait");
    while w.min_done() < need {
        std::thread::yield_now();
    }
    crate::diag::tl("R waited");
    if crate::diag::counting() {
        let loc = std::panic::Location::caller();
            crate::diag::add(&format!("rwaitus_{}:{}", loc.file().rsplit(['/', '\\']).next().unwrap_or(""), loc.line()), t0.elapsed().as_micros() as u64);
    }
    GS_US[1].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Draw the queued primitives. A batch for all cores runs in the background while the GS thread
/// goes on with the next commands; the next batch, and anything that reads local memory, waits
/// for it first (`wait`), so drawing stays in order.
#[track_caller]
pub fn flush(gs: &mut Gs) {
    if gs.batch.items.is_empty() {
        return;
    }
    if crate::diag::enabled("T5_FLUSHSTATS") {
        let loc = std::panic::Location::caller();
        let n = gs.batch.items.len();
        crate::diag::count_with(|| format!("flushat_{}:{}", loc.file().rsplit(['/', '\\']).next().unwrap_or(""), loc.line()));
        let k = gs.batch.key;
        crate::diag::count_with(|| format!("flushkey_f{:x}w{}p{}z{:x}zp{}_n{}", k.0, k.1, k.2, k.3, k.4, if n < 8 { n } else { 99 }));
    }
    // A deferred CLUT load reads pages this batch draws into: it comes first.
    if gs.pending_clut.is_some_and(|p| (0..8).any(|i| p.2[i] & gs.batch.pages[i] != 0)) {
        gs.realize_clut();
    }
    let t0 = std::time::Instant::now();
    let batch = std::mem::take(&mut gs.batch);
    gs.batch.key = batch.key;
    if gs.gpu.is_some() {
        gpu_flush(gs, &batch);
        GS_US[1].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    FLUSHES[2].fetch_add(batch.items.len() as u64, std::sync::atomic::Ordering::Relaxed);
    if batch.serial && crate::diag::enabled("T5_FLUSHSTATS") {
        let k = batch.key;
        let u = batch.items.iter().fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |u, q| (u.0.min(q.bbox.x0), u.1.min(q.bbox.y0), u.2.max(q.bbox.x1), u.3.max(q.bbox.y1)));
        crate::diag::add(&format!("serialpx_f{:x}w{}z{:x}_{},{}-{},{}", k.0, k.1, k.3, u.0, u.1, u.2, u.3), batch.area as u64);
    }
    if batch.serial || crate::diag::enabled("T5_RASTER_SERIAL") || safe() {
        wait(gs);
        for q in &batch.items {
            draw_queued(gs, q, &NO_CLIP);
        }
        FLUSHES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        GS_US[2].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    FLUSHES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if gs.workers.is_none() {
        gs.workers = Some(Box::new(Workers::start(gs)));
    }
    // Batches in flight this one depends on: it writes pages they read or write, or reads pages
    // they write (with another buffer layout the rows of different workers alias).
    let need = {
        let w = gs.workers.as_ref().unwrap();
        w.newest(&batch.pages, true, true).max(w.newest(&batch.reads, true, false))
    };
    // A small batch is drawn here: handing it to every worker costs more than drawing it.
    static SMALL: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let small = *SMALL.get_or_init(|| std::env::var("T5_SMALL").ok().and_then(|v| v.parse().ok()).unwrap_or(SMALL_BATCH));
    let ready = gs.workers.as_ref().map_or(true, |w| w.min_done() >= need.max(batch.after));
    if batch.area < small && ready {
        for q in &batch.items {
            draw_queued(gs, q, &NO_CLIP);
        }
        FLUSHES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        GS_US[2].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let mut batch = batch;
    batch.after = batch.after.max(need);
    let w = gs.workers.as_mut().unwrap();
    for i in 0..8 {
        w.inflight[i] |= batch.pages[i];
    }
    w.last_key = Some(batch.key);
    w.sent += 1;
    let seq = w.sent;
    let wpages = batch.pages;
    let wreads = batch.reads;
    if crate::diag::enabled("T5_SYNCLOG") {
        let u = batch.items.iter().fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |u, q| (u.0.min(q.bbox.x0), u.1.min(q.bbox.y0), u.2.max(q.bbox.x1), u.3.max(q.bbox.y1)));
        w.flight_info.push_back((seq, batch.key, u));
        if w.flight_info.len() > 64 {
            w.flight_info.pop_front();
        }
    }
    crate::diag::tl("R flush");
    let items = std::sync::Arc::new(Work { after: batch.after, items: batch.items, upload: None, clut: None, decode: None, readout: None });
    w.record(seq, &wpages, &wreads, Some(items.clone()));
    for tx in &w.txs {
        let _ = tx.send(items.clone());
    }
    crate::diag::TIME[5].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Time (µs) and covered pixels by primitive kind 0..7 (diagnostics).
pub static KIND_US: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];
pub static KIND_PX: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];

/// Per-pixel / per-primitive raster statistics (shared atomic counters and clock reads in the
/// workers' hot loops): only with T5_RASTER_STATS=1.
#[inline(always)]
fn rstats() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::diag::enabled("T5_RASTER_STATS"))
}

fn draw_queued(gs: &mut Gs, q: &Queued, clip: &Clip) {
    if !rstats() {
        draw_queued_inner(gs, q, clip);
        return;
    }
    let t0 = std::time::Instant::now();
    let c0 = COVERED.load(std::sync::atomic::Ordering::Relaxed);
    draw_queued_inner(gs, q, clip);
    let k = (q.kind & 7) as usize;
    KIND_US[k].fetch_add(t0.elapsed().as_nanos() as u64 / 1000, std::sync::atomic::Ordering::Relaxed);
    let _ = c0;
}

fn draw_queued_inner(gs: &mut Gs, q: &Queued, _clip: &Clip) {
    if let Some(pr) = prim_setup(&q.st, q.kind, &q.vs) {
        draw_prim(gs, &q.st, &pr);
    }
}

/// A primitive ready to rasterize: what the CPU reference draws and the GPU gets (`gpu.rs`).
/// Pixels: (x, y) in [x0, x1) × [y0, y1) and inside the scissor; triangles additionally inside
/// all three edges (w = e0 + ex·x + ey·y + bias >= 0), lines at the steps of their DDA.
#[derive(Clone, Copy, Default)]
pub(crate) struct Prim {
    pub kind: u32,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    /// Lines: steps and the end points (12.4).
    pub n: i64,
    pub lx: [i64; 4],
    /// Triangle edges: e0, ex, ey, bias.
    pub e: [[i64; 4]; 3],
    /// Plane origin (dx = x - ox, dy = y - oy; lines: dx = step).
    pub ox: i32,
    pub oy: i32,
    pub ip: Interp,
}

/// floor(n · 2^sh / d) (d > 0), in f64: deterministic, and only the CPU computes it.
#[inline(always)]
fn coef(n: i128, sh: u32, d: i128) -> i64 {
    ((n as f64) * ((1u64 << sh) as f64) / d as f64).floor() as i64
}

/// The primitive `kind` with vertices `vs` (offset applied), or None if it covers no pixel.
fn prim_setup(st: &Setup, kind: u32, vs: &[Vertex; 3]) -> Option<Prim> {
    let mut pr = Prim { kind, ..Default::default() };
    match kind {
        0 => {
            let v = vs[0];
            let (px, py) = ((v.x + 8) >> 4, (v.y + 8) >> 4);
            let (vals, qs) = attr_vals(&[&v], st);
            pr.ip.qs = qs;
            for i in 0..9 {
                pr.ip.p[i] = if RAW_ATTR[i] { Plane { c0: vals[0][i], cx: 0, cy: 0 } } else { Plane::flat(vals[0][i]) };
            }
            (pr.x0, pr.y0, pr.x1, pr.y1) = (px.max(st.sx0), py.max(st.sy0), (px + 1).min(st.sx1 + 1), (py + 1).min(st.sy1 + 1));
            (pr.ox, pr.oy) = (px, py);
        }
        1 | 2 => {
            let (a, b) = (&vs[0], &vs[1]);
            let (x0, y0, x1, y1) = (a.x as i64, a.y as i64, b.x as i64, b.y as i64);
            // One step per pixel along the major axis; step i is at (x0, y0) + (x1 - x0, y1 - y0)
            // · i / n, rounded to the nearest pixel. Attributes go the same way (flat: the
            // second vertex's colour).
            let n = (((x1 - x0).abs().max((y1 - y0).abs()) + 15) >> 4).max(1);
            let (vals, qs) = attr_vals(&[a, b], st);
            pr.ip.qs = qs;
            for i in 0..9 {
                let (va, vb) = (vals[0][i] as i128, vals[1][i] as i128);
                let sh = if RAW_ATTR[i] { 0 } else { PF };
                pr.ip.p[i] = if (1..=4).contains(&i) && !st.gouraud {
                    Plane::flat(vals[1][i])
                } else {
                    Plane { c0: (va << sh) as i64, cx: coef(vb - va, sh, n as i128), cy: 0 }
                };
            }
            pr.n = n;
            pr.lx = [x0, y0, x1, y1];
            let (px0, px1) = ((x0.min(x1) >> 4) as i32 - 1, ((x0.max(x1) + 15) >> 4) as i32 + 1);
            let (py0, py1) = ((y0.min(y1) >> 4) as i32 - 1, ((y0.max(y1) + 15) >> 4) as i32 + 1);
            (pr.x0, pr.y0, pr.x1, pr.y1) = (px0.max(st.sx0), py0.max(st.sy0), px1.min(st.sx1 + 1), py1.min(st.sy1 + 1));
        }
        6 => {
            let (mut va, mut vb) = (vs[0], vs[1]);
            let (x0, x1) = (va.x.min(vb.x), va.x.max(vb.x));
            let (y0, y1) = (va.y.min(vb.y), va.y.max(vb.y));
            pr.x0 = ((x0 + 15) >> 4).max(st.sx0);
            pr.x1 = ((x1 + 15) >> 4).min(st.sx1 + 1);
            pr.y0 = ((y0 + 15) >> 4).max(st.sy0);
            pr.y1 = ((y1 + 15) >> 4).min(st.sy1 + 1);
            if pr.x0 >= pr.x1 || pr.y0 >= pr.y1 {
                return None;
            }
            // Colour, depth, fog: the second vertex. S / U and Q along x, T / V along y, from
            // the vertex with the smaller coordinate to the other.
            let flat = vb;
            if va.x > vb.x {
                std::mem::swap(&mut va.x, &mut vb.x);
                std::mem::swap(&mut va.s, &mut vb.s);
                std::mem::swap(&mut va.u, &mut vb.u);
                std::mem::swap(&mut va.q, &mut vb.q);
            }
            if va.y > vb.y {
                std::mem::swap(&mut va.y, &mut vb.y);
                std::mem::swap(&mut va.t, &mut vb.t);
                std::mem::swap(&mut va.v, &mut vb.v);
            }
            let (vals, qs) = attr_vals(&[&va, &vb, &flat], st);
            pr.ip.qs = qs;
            for i in 0..6 {
                pr.ip.p[i] = Plane::flat(vals[2][i]);
            }
            let (dx, dy) = ((x1 - x0).max(1) as i128, (y1 - y0).max(1) as i128);
            (pr.ox, pr.oy) = (pr.x0, pr.y0);
            // value(p) = v0 + (v1 - v0) · (16 p - c0) / d
            let along = |v0: i64, v1: i64, c0: i32, d: i128, o: i32| {
                let dv = (v1 - v0) as i128;
                (v0.wrapping_add(coef(dv * (16 * o as i128 - c0 as i128), 0, d)), coef(dv * 16, 0, d))
            };
            let (s0, sx) = along(vals[0][6], vals[1][6], x0, dx, pr.ox);
            let (t0, ty) = along(vals[0][7], vals[1][7], y0, dy, pr.oy);
            pr.ip.p[6] = Plane { c0: s0, cx: sx, cy: 0 };
            pr.ip.p[7] = Plane { c0: t0, cx: 0, cy: ty };
            pr.ip.p[8] = if st.fst {
                Plane::default()
            } else {
                let (q0, qx) = along(vals[0][8], vals[1][8], x0, dx, pr.ox);
                Plane { c0: q0, cx: qx, cy: 0 }
            };
        }
        _ => {
            let (a, b, c) = (&vs[0], &vs[1], &vs[2]);
            let (ax, ay, bx, by, cx, cy) = (a.x as i64, a.y as i64, b.x as i64, b.y as i64, c.x as i64, c.y as i64);
            let area = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
            if area == 0 {
                return None;
            }
            let minx = ((ax.min(bx).min(cx) + 15) >> 4).max(st.sx0 as i64);
            let maxx = ((ax.max(bx).max(cx) + 15) >> 4).min(st.sx1 as i64 + 1);
            let miny = ((ay.min(by).min(cy) + 15) >> 4).max(st.sy0 as i64);
            let maxy = ((ay.max(by).max(cy) + 15) >> 4).min(st.sy1 as i64 + 1);
            if minx >= maxx || miny >= maxy {
                return None;
            }
            (pr.x0, pr.y0, pr.x1, pr.y1) = (minx as i32, miny as i32, maxx as i32, maxy as i32);
            // Edge functions w = (x1 - x0) * (py - y0) - (y1 - y0) * (px - x0), orientation
            // normalised so inside is >= 0, with the top-left rule as a bias (pixels exactly on
            // a right or bottom edge are not drawn). Edges: k = 0 (b, c), 1 (c, a), 2 (a, b):
            // the weights of a, b, c. w at pixel (px, py): e0 + ex * px + ey * py.
            let sign = if area > 0 { 1i64 } else { -1 };
            let edges = [(bx, by, cx, cy), (cx, cy, ax, ay), (ax, ay, bx, by)];
            for (k, &(x0, y0, x1, y1)) in edges.iter().enumerate() {
                let (dx, dy) = ((x1 - x0) * sign, (y1 - y0) * sign);
                let bias = if dy < 0 || (dy == 0 && dx > 0) { 0 } else { -1 };
                pr.e[k] = [((y1 - y0) * x0 - (x1 - x0) * y0) * sign, -(y1 - y0) * 16 * sign, (x1 - x0) * 16 * sign, bias];
            }
            // Attribute planes through the vertices' values.
            let area_n = (area * sign) as i128;
            (pr.ox, pr.oy) = (pr.x0, pr.y0);
            let (vals, qs) = attr_vals(&[a, b, c], st);
            pr.ip.qs = qs;
            for i in 0..9 {
                if (1..=4).contains(&i) && !st.gouraud {
                    pr.ip.p[i] = Plane::flat(vals[2][i]);
                    continue;
                }
                let sh = if RAW_ATTR[i] { 0 } else { PF };
                let (mut n0, mut nx, mut ny) = (0i128, 0i128, 0i128);
                for k in 0..3 {
                    let [e0, ex, ey, _] = pr.e[k];
                    let v = vals[k][i] as i128;
                    n0 += v * (e0 as i128 + ex as i128 * pr.ox as i128 + ey as i128 * pr.oy as i128);
                    nx += v * ex as i128;
                    ny += v * ey as i128;
                }
                pr.ip.p[i] = Plane { c0: coef(n0, sh, area_n), cx: coef(nx, sh, area_n), cy: coef(ny, sh, area_n) };
            }
        }
    }
    (pr.x0 < pr.x1 && pr.y0 < pr.y1).then_some(pr)
}

/// Draw a primitive (CPU reference) in the rows this thread owns.
fn draw_prim(gs: &mut Gs, st: &Setup, pr: &Prim) {
    let pf = st.plot;
    let ip = &pr.ip;
    match pr.kind {
        1 | 2 => {
            let [x0, y0, x1, y1] = pr.lx;
            let n = pr.n;
            for i in 0..n {
                let px = div_floor((x0 * n + (x1 - x0) * i + 8 * n) as i128, 16 * n as i128) as i32;
                let py = div_floor((y0 * n + (y1 - y0) * i + 8 * n) as i128, 16 * n as i128) as i32;
                if px < pr.x0 || px >= pr.x1 || py < pr.y0 || py >= pr.y1 || !owns_row(py) {
                    continue;
                }
                pf(gs, st, px, py, &ip.frag(&ip.raw(i, 0), st));
            }
        }
        3 | 4 | 5 => {
            let (minx, maxx) = (pr.x0 as i64, pr.x1 as i64);
            rows(gs, pr.y0 as i64, pr.y1 as i64, maxx - minx, |gs, py| {
                // The covered span of this row: each edge bounds x from one side.
                let (mut xs, mut xe) = (minx, maxx);
                for k in 0..3 {
                    let [e0, ex, ey, bias] = pr.e[k];
                    let w = e0 + ey * py + ex * minx + bias;
                    if ex == 0 {
                        if w < 0 {
                            return;
                        }
                    } else if ex > 0 {
                        // w + ex * (x - minx) >= 0  =>  x >= minx + ceil(-w / ex)
                        if w < 0 {
                            xs = xs.max(minx + (-w + ex - 1) / ex);
                        }
                    } else {
                        // x <= minx + floor(w / -ex)
                        if w < 0 {
                            return;
                        }
                        xe = xe.min(minx + w / -ex + 1);
                    }
                }
                if xs >= xe {
                    return;
                }
                if rstats() { COVERED.fetch_add((xe - xs) as u64, std::sync::atomic::Ordering::Relaxed); }
                let mut raw = ip.raw(xs - pr.ox as i64, py - pr.oy as i64);
                for px in xs..xe {
                    pf(gs, st, px as i32, py as i32, &ip.frag(&raw, st));
                    ip.step_x(&mut raw);
                }
            });
        }
        _ => {
            // Sprites and points: every pixel of the rectangle.
            let (x0, x1) = (pr.x0, pr.x1);
            rows(gs, pr.y0 as i64, pr.y1 as i64, (x1 - x0) as i64, |gs, py| {
                if rstats() { COVERED.fetch_add((x1 - x0) as u64, std::sync::atomic::Ordering::Relaxed); }
                let mut raw = ip.raw((x0 - pr.ox) as i64, py - pr.oy as i64);
                for px in x0..x1 {
                    pf(gs, st, px, py as i32, &ip.frag(&raw, st));
                    ip.step_x(&mut raw);
                }
            });
        }
    }
}










// ---- texturing ----

/// A decoded texture level: RGBA8 texels (alpha after TEXA expansion), row-major.
pub struct TexImg {
    pub w: u32,
    pub h: u32,
    /// Filled when created, or by a raster decode job (readers order after it, `Tex::dep`).
    data: std::cell::UnsafeCell<Vec<u32>>,
}

unsafe impl Sync for TexImg {}
unsafe impl Send for TexImg {}

impl TexImg {
    #[inline(always)]
    fn px(&self) -> &Vec<u32> {
        unsafe { &*self.data.get() }
    }
}

/// Size in pixels of a page in each storage mode (for the pages a texture covers).
fn page_dims(psm: u32) -> (u32, u32) {
    match psm {
        PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => (64, 64),
        PSMT8 => (128, 64),
        PSMT4 => (128, 128),
        _ => (64, 32),
    }
}

/// Level `l` of texture `t`, decoded, from the cache unless its pages were written since.
fn cached_level(gs: &mut Gs, t: &Tex, l: usize) -> (std::sync::Arc<TexImg>, u64) {
    let (tbp, tbw) = if l == 0 { (t.tbp, t.tbw) } else { t.mip[l] };
    let w = 1u32 << t.tw.saturating_sub(l as u32).min(10);
    let h = 1u32 << t.th.saturating_sub(l as u32).min(10);
    let indexed = matches!(t.psm, PSMT8 | PSMT8H | PSMT4 | PSMT4HL | PSMT4HH);
    let clut_hash = if indexed { gs.clut_hash } else { 0 };
    let key = ((tbp as u64) | (tbw as u64) << 14 | (t.psm as u64) << 20 | (w as u64) << 26 | (h as u64) << 38, clut_hash,
               (t.ta0 as u64) | (t.ta1 as u64) << 8 | (t.aem as u64) << 16);
    // Generation sum of the pages the texture covers.
    let (pw, ph) = page_dims(t.psm);
    let stride = match t.psm { PSMT8 | PSMT4 => (tbw / 2).max(1), _ => tbw.max(1) }.max(w.div_ceil(pw));
    let pages = (stride * h.div_ceil(ph)) as usize;
    let first = (tbp / 32) as usize;
    let mut sum = 0u64;
    for i in 0..pages.min(512) {
        sum = sum.wrapping_mul(31).wrapping_add(gs.page_gen[(first + i) & 511].load(std::sync::atomic::Ordering::Relaxed) as u64 + 1);
    }
    if let Some((g, img, dep)) = gs.tex_cache.get(&key) {
        if *g == sum {
            return (img.clone(), *dep);
        }
    }
    // The pages changed since the image was cached: wait only if queued or running drawing
    // uses one of them (PCSX2 GSRendererSW::CheckSourcePages).
    let mut mask = [0u64; 8];
    for i in 0..pages.min(512) {
        let p = (first + i) & 511;
        mask[p >> 6] |= 1 << (p & 63);
    }
    // A deferred CLUT load an indexed texture needs, or drawing in flight on its pages: the
    // decoding becomes a raster job (the GS thread does not wait).
    if indexed && gs.pending_clut.is_some() {
        gs.realize_clut();
    }
    if gs.pending_clut.is_some_and(|p| (0..8).any(|i| p.2[i] & mask[i] != 0)) {
        gs.realize_clut();
    }
    let cpsm = ((gs.clut_tex0 >> 51) & 0xF) as u32;
    let clut16 = cpsm == PSMCT16 || cpsm == PSMCT16S;
    if safe() {
        sync_pages(gs, &mask);
    }
    let busy = flight_using(gs, &mask);
    if !safe() && (busy.is_some() || (indexed && gs.clut_job.is_some())) {
        let img = std::sync::Arc::new(TexImg { w, h, data: std::cell::UnsafeCell::new(vec![0u32; (w * h) as usize]) });
        let mut after = busy.unwrap_or(0);
        let clut = if indexed {
            match gs.clut_job.clone() {
                Some(b) => {
                    // The palette comes from a CLUT job (worker 0): decode after it.
                    after = after.max(gs.clut_job_seq);
                    ClutSrc::Job(b)
                }
                None => ClutSrc::Known(Box::new(gs.clut)),
            }
        } else {
            ClutSrc::Known(Box::new([0; 256]))
        };
        let job = DecodeJob { img: img.clone(), tex: t.clone(), tbp, tbw, clut, clut16 };
        let dep = submit_decode(gs, job, after);
        if gs.tex_cache.len() > 512 {
            gs.tex_cache.clear();
        }
        gs.tex_cache.insert(key, (sum, img.clone(), dep));
        return (img, dep);
    }
    // Rows in parallel on the (otherwise idle) rayon pool; the format decision per texture.
    let mut data = vec![0u32; (w * h) as usize];
    let g: &Gs = gs;
    let psm = t.psm;
    let row = |y: u32, out: &mut [u32]| {
        macro_rules! each {
            ($f:expr) => {
                for (x, o) in out.iter_mut().enumerate() {
                    let raw = g.read_pixel(psm, tbp, tbw, x as u32 & 2047, y & 2047);
                    *o = $f(raw);
                }
            };
        }
        match psm {
            PSMT8 | PSMT8H | PSMT4 | PSMT4HL | PSMT4HH => {
                if clut16 {
                    each!(|raw: u32| pack(c16(g.clut[(raw & 255) as usize], t)))
                } else {
                    each!(|raw: u32| g.clut[(raw & 255) as usize])
                }
            }
            PSMCT16 | PSMCT16S => each!(|raw: u32| pack(c16(raw, t))),
            PSMCT24 => each!(|raw: u32| {
                let rgb = raw & 0xFF_FFFF;
                let a = if t.aem && rgb == 0 { 0 } else { t.ta0 };
                rgb | (a << 24)
            }),
            _ => each!(|raw: u32| raw),
        }
    };
    if w * h >= 16384 {
        data.par_chunks_mut(w as usize).enumerate().for_each(|(y, out)| row(y as u32, out));
    } else {
        for (y, out) in data.chunks_mut(w as usize).enumerate() {
            row(y as u32, out);
        }
    }
    let img = std::sync::Arc::new(TexImg { w, h, data: std::cell::UnsafeCell::new(data) });
    if gs.tex_cache.len() > 512 {
        gs.tex_cache.clear();
    }
    gs.tex_cache.insert(key, (sum, img.clone(), 0));
    (img, 0)
}

/// Where a decode job takes the CLUT from.
pub enum ClutSrc {
    Known(Box<[u32; 256]>),
    Job(std::sync::Arc<crate::gs::ClutBuf>),
}

/// A texture level the raster workers decode, each its own rows.
pub struct DecodeJob {
    img: std::sync::Arc<TexImg>,
    tex: Tex,
    tbp: u32,
    tbw: u32,
    clut: ClutSrc,
    clut16: bool,
}

/// Decode the rows of `job` that worker `k` of `n` owns (as `cached_level`).
fn run_decode(g: &Gs, job: &DecodeJob, k: u32, n: u32) {
    let img = &*job.img;
    let data = unsafe { &mut *img.data.get() };
    let clut: &[u32; 256] = match &job.clut {
        ClutSrc::Known(c) => c,
        ClutSrc::Job(b) => b.get(),
    };
    let t = &job.tex;
    let (psm, tbp, tbw) = (t.psm, job.tbp, job.tbw);
    let w = img.w as usize;
    let bd = band() as u32;
    for y in 0..img.h {
        if (y / bd) % n != k {
            continue;
        }
        let out = &mut data[y as usize * w..(y as usize + 1) * w];
        for (x, o) in out.iter_mut().enumerate() {
            let raw = g.read_pixel(psm, tbp, tbw, x as u32 & 2047, y & 2047);
            *o = match psm {
                PSMT8 | PSMT8H | PSMT4 | PSMT4HL | PSMT4HH => {
                    let e = clut[(raw & 255) as usize];
                    if job.clut16 { pack(c16(e, t)) } else { e }
                }
                PSMCT16 | PSMCT16S => pack(c16(raw, t)),
                PSMCT24 => {
                    let rgb = raw & 0xFF_FFFF;
                    let a = if t.aem && rgb == 0 { 0 } else { t.ta0 };
                    rgb | (a << 24)
                }
                _ => raw,
            };
        }
    }
}

fn pack(c: [u32; 4]) -> u32 {
    c[0] | c[1] << 8 | c[2] << 16 | c[3] << 24
}

/// A texel as RGBA (0..255, alpha after TEXA expansion).
#[inline(always)]
fn texel(t: &Tex, level: usize, u: i32, v: i32) -> [u32; 4] {
    let c = texel_raw(t, level, u, v);
    [c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF, c >> 24]
}

/// Texture wrap (CLAMP): repeat, clamp, region clamp, region repeat. Sizes are powers of two.
#[inline(always)]
fn wrap(c: i32, size: i32, mode: u32, min: i32, max: i32) -> i32 {
    match mode {
        0 => c & (size - 1),
        1 => c.clamp(0, size - 1),
        2 => c.clamp(min, max).clamp(0, size - 1),
        _ => ((c & min) | max) & (size - 1),
    }
}

/// A texel straight from local memory (`Tex::direct`), as `cached_level` would decode it.
#[inline(never)]
fn texel_direct(t: &Tex, level: usize, u: i32, v: i32) -> u32 {
    let w = 1i32 << t.tw.saturating_sub(level as u32).min(10);
    let h = 1i32 << t.th.saturating_sub(level as u32).min(10);
    let l = level as i32;
    let x = wrap(u, w, t.wms, t.minu >> l, t.maxu >> l) as u32 & 2047;
    let y = wrap(v, h, t.wmt, t.minv >> l, t.maxv >> l) as u32 & 2047;
    let (tbp, tbw) = if level == 0 { (t.tbp, t.tbw) } else { t.mip[level] };
    let mem = t.mem as *const u8;
    unsafe {
        match t.psm {
            PSMCT16 | PSMCT16S => {
                let a = crate::gs::addr16(tbp, tbw, x, y, t.psm);
                pack(c16(std::ptr::read_unaligned(mem.add(a) as *const u16) as u32, t))
            }
            _ => {
                let raw = std::ptr::read_unaligned(mem.add(crate::gs::addr32(tbp, tbw, x, y, false)) as *const u32);
                if t.psm == PSMCT24 {
                    let rgb = raw & 0xFF_FFFF;
                    let a = if t.aem && rgb == 0 { 0 } else { t.ta0 };
                    rgb | (a << 24)
                } else {
                    raw
                }
            }
        }
    }
}

/// A texel as packed RGBA8.
#[inline(always)]
fn texel_raw(t: &Tex, level: usize, u: i32, v: i32) -> u32 {
    if t.direct.is_some() {
        return texel_direct(t, level, u, v);
    }
    if level == 0 {
        if let Some(img) = t.img[0].as_ref() {
            // Level 0 (nearly every fetch): sizes are powers of two, the wrapped / clamped
            // coordinates lie inside the image.
            let (w, h) = (img.w as i32, img.h as i32);
            let x = wrap(u, w, t.wms, t.minu, t.maxu);
            let y = wrap(v, h, t.wmt, t.minv, t.maxv);
            return unsafe { *img.px().get_unchecked((y * w + x) as usize) };
        }
        return 0;
    }
    let Some(img) = t.img[level].as_ref().or(t.img[0].as_ref()) else { return 0 };
    let (w, h) = (img.w as i32, img.h as i32);
    let x = wrap(u, w, t.wms, t.minu >> level, t.maxu >> level);
    let y = wrap(v, h, t.wmt, t.minv >> level, t.maxv >> level);
    img.px()[(y * w + x) as usize]
}

fn c16(c: u32, t: &Tex) -> [u32; 4] {
    let r = (c & 31) << 3;
    let g = ((c >> 5) & 31) << 3;
    let b = ((c >> 10) & 31) << 3;
    let a = if c & 0x8000 != 0 { t.ta1 } else if t.aem && c & 0x7FFF == 0 { 0 } else { t.ta0 };
    [r, g, b, a]
}

/// 16 · log2(1 + i / 256), floored (fraction of the integer log2 below).
static LOG2_TAB: std::sync::LazyLock<[i32; 256]> = std::sync::LazyLock::new(|| std::array::from_fn(|i| (16.0 * (1.0 + i as f64 / 256.0).log2()).floor() as i32));

/// Entry `i` of the log2 fraction table (for the GPU).
pub fn log2_tab(i: usize) -> i32 {
    LOG2_TAB[i]
}

/// 16 · log2(x) for x > 0, from the leading one and the next 8 bits.
#[inline(always)]
fn log2_16(x: u64) -> i32 {
    let m = 63 - x.leading_zeros();
    let frac = ((x << (63 - m)) >> 55) & 0xFF;
    16 * m as i32 + LOG2_TAB[frac as usize]
}

/// (filter, level) for LOD `lod16` (1/16 units); None = it depends on Q.
fn lod_select(t: &Tex, q: Option<(i64, i32)>) -> Option<(bool, usize)> {
    let lod = if t.lcm {
        t.k16
    } else if t.mxl == 0 && t.mmag == (t.mmin & 1 == 1) {
        0
    } else {
        // LOD = (log2(1 / Q) << L) + K, Q = q / 2^qs.
        let (q, qs) = q?;
        (qs * 16 - log2_16(q.unsigned_abs().max(1))) * (1 << t.l) + t.k16
    };
    Some(if lod <= 0 || t.mxl == 0 {
        (if lod <= 0 { t.mmag } else { t.mmin & 1 == 1 }, 0usize)
    } else {
        let lvl = (((lod + 8) >> 4) as u32).min(t.mxl) as usize;
        (t.mmin == 1 || t.mmin >= 4, if t.mmin >= 2 { lvl } else { 0 })
    })
}

#[inline(always)]
fn sample(t: &Tex, f: &Frag, fst: bool) -> [u32; 4] {
    // Texel coordinates in 1/16 at level 0: UV (FST) directly, STQ as S / Q · size.
    let (u16, v16) = if fst {
        ((f.s >> PF) as i32, (f.t >> PF) as i32)
    } else if f.q != 0 {
        // Wrapping i64 as on the GPU (no overflow for real coordinates).
        let cu = |s: i64, lg: u32| div_floor64(s.wrapping_mul(16 << lg.min(10)), f.q).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        (cu(f.s, t.tw), cu(f.t, t.th))
    } else {
        (0, 0)
    };
    let (filter, level) = match t.fixed_lod {
        Some(fl) => fl,
        None => lod_select(t, Some(if fst { (1, 0) } else { (f.q, f.qs) })).unwrap(),
    };
    let (u16, v16) = (u16 >> level, v16 >> level);
    if !filter {
        return texel(t, level, u16 >> 4, v16 >> 4);
    }
    // Bilinear as the GS does it: texel coordinates have 4 fraction bits, the weights are
    // (16 - f) and f in each direction.
    let (u16, v16) = (u16 - 8, v16 - 8);
    let (iu, iv) = (u16 >> 4, v16 >> 4);
    let (fu, fv) = ((u16 & 15) as u32, (v16 & 15) as u32);
    if fu == 0 && fv == 0 {
        return texel(t, level, iu, iv);
    }
    let c00 = texel_raw(t, level, iu, iv);
    let c10 = texel_raw(t, level, iu + 1, iv);
    let c01 = texel_raw(t, level, iu, iv + 1);
    let c11 = texel_raw(t, level, iu + 1, iv + 1);
    let (w00, w10, w01, w11) = ((16 - fu) * (16 - fv), fu * (16 - fv), (16 - fu) * fv, fu * fv);
    const M: u32 = 0x00FF_00FF;
    // R / B and G / A in the two halves of a word: one multiply per texel and pair.
    let rb = (((c00 & M) * w00 + (c10 & M) * w10 + (c01 & M) * w01 + (c11 & M) * w11) >> 8) & M;
    let ga = ((((c00 >> 8) & M) * w00 + ((c10 >> 8) & M) * w10 + ((c01 >> 8) & M) * w01 + ((c11 >> 8) & M) * w11) >> 8) & M;
    let c = rb | (ga << 8);
    [c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF, c >> 24]
}

// ---- the pixel pipeline ----

/// Frames the game finished drawing (see `queue`).
pub static GAME_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Real game frames: copy passes whose source frame buffer was drawn since the previous one
/// (per log period, and since start for the FPS overlay). THE frame rate measure.
pub static RENDERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static RENDERED_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_COPY_SRC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PIXELS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PRIMS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A pixel function specialised for a primitive's texture / alpha test / depth test / blend /
/// fog switches (PCSX2's software renderer selects scanline functions the same way).
type PlotFn = fn(&mut Gs, &Setup, i32, i32, &Frag);

fn plot_fn(tme: bool, ate: bool, zte: bool, abe: bool, fge: bool) -> PlotFn {
    macro_rules! pick {
        ($($t:literal $a:literal $z:literal $b:literal $f:literal),*) => {
            match (tme, ate, zte, abe, fge) {
                $(($t, $a, $z, $b, $f) => plot_g::<$t, $a, $z, $b, $f>,)*
            }
        };
    }
    pick!(
        false false false false false, false false false false true, false false false true false, false false false true true,
        false false true false false, false false true false true, false false true true false, false false true true true,
        false true false false false, false true false false true, false true false true false, false true false true true,
        false true true false false, false true true false true, false true true true false, false true true true true,
        true false false false false, true false false false true, true false false true false, true false false true true,
        true false true false false, true false true false true, true false true true false, true false true true true,
        true true false false false, true true false false true, true true false true false, true true false true true,
        true true true false false, true true true false true, true true true true false, true true true true true
    )
}

fn plot_g<const TME: bool, const ATE: bool, const ZTE: bool, const ABE: bool, const FGE: bool>(gs: &mut Gs, st: &Setup, x: i32, y: i32, f: &Frag) {
    let (ux, uy) = (x as u32 & 2047, y as u32 & 2047);
    let mut c = f.c;
    if let (true, Some(t)) = (TME, st.tex.as_ref()) {
        let tx = sample(t, f, st.fst);
        let m = |a: u32, b: u32| (a * b) >> 7;
        match t.tfx {
            0 => {
                c[0] = m(tx[0], c[0]).min(255);
                c[1] = m(tx[1], c[1]).min(255);
                c[2] = m(tx[2], c[2]).min(255);
                if t.tcc {
                    c[3] = m(tx[3], c[3]).min(255);
                }
            }
            1 => {
                c[0] = tx[0];
                c[1] = tx[1];
                c[2] = tx[2];
                if t.tcc {
                    c[3] = tx[3];
                }
            }
            2 | 3 => {
                let fa = c[3];
                c[0] = (m(tx[0], c[0]) + fa).min(255);
                c[1] = (m(tx[1], c[1]) + fa).min(255);
                c[2] = (m(tx[2], c[2]) + fa).min(255);
                if t.tcc {
                    c[3] = if t.tfx == 2 { (tx[3] + fa).min(255) } else { tx[3] };
                }
            }
            _ => {}
        }
    }
    if FGE {
        let fv = f.f;
        for i in 0..3 {
            c[i] = (c[i] * fv + st.fogcol[i] * (255 - fv)) >> 8;
        }
    }
    // Alpha test.
    let mut write_fb = true;
    let mut write_rgb_only = false;
    let mut write_z = !st.zmsk;
    if ATE {
        let a = c[3];
        let pass = match st.atst {
            0 => false,
            1 => true,
            2 => a < st.aref,
            3 => a <= st.aref,
            4 => a == st.aref,
            5 => a >= st.aref,
            6 => a > st.aref,
            _ => a != st.aref,
        };
        if !pass {
            match st.afail {
                0 => return,
                1 => write_z = false,
                2 => write_fb = false,
                _ => {
                    write_rgb_only = true;
                    write_z = false;
                }
            }
        }
    }
    let fpsm = st.fpsm;
    // The destination is needed for blending, the destination alpha test, masks and the
    // formats that keep part of the word.
    let need_dst = (ABE && !(st.pabe && c[3] & 0x80 == 0)) || st.date || st.fbmsk != 0 || write_rgb_only || fpsm != PSMCT32 || !write_fb;
    let dst = if need_dst { gs.read_pixel(fpsm, st.fbp, st.fbw, ux, uy) } else { 0 };
    let (dr, dg, db, da) = match fpsm {
        PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => (((dst & 31) << 3), (((dst >> 5) & 31) << 3), (((dst >> 10) & 31) << 3), if dst & 0x8000 != 0 { 0x80 } else { 0 }),
        PSMCT24 | PSMZ24 => (dst & 0xFF, (dst >> 8) & 0xFF, (dst >> 16) & 0xFF, 0x80),
        _ => (dst & 0xFF, (dst >> 8) & 0xFF, (dst >> 16) & 0xFF, dst >> 24),
    };
    // Destination alpha test.
    if st.date {
        let bit = match fpsm {
            PSMCT16 | PSMCT16S => dst & 0x8000 != 0,
            PSMCT32 => dst & 0x8000_0000 != 0,
            _ => false,
        };
        if bit != st.datm {
            return;
        }
    }
    // Depth test.
    let z = f.z;
    if ZTE {
        let dz = gs.read_pixel(st.zpsm, st.zbp, st.fbw, ux, uy);
        let pass = match st.ztst {
            0 => false,
            1 => true,
            2 => z >= dz,
            _ => z > dz,
        };
        if !pass {
            return;
        }
    }
    // Alpha blend.
    if ABE && !(st.pabe && c[3] & 0x80 == 0) {
        let cs = [c[0] as i32, c[1] as i32, c[2] as i32];
        let cd = [dr as i32, dg as i32, db as i32];
        let pick = |sel: u32, i: usize| match sel {
            0 => cs[i],
            1 => cd[i],
            _ => 0,
        };
        let cc = match st.ac {
            0 => c[3] as i32,
            1 => da as i32,
            _ => st.afix as i32,
        };
        for i in 0..3 {
            let v = (((pick(st.aa, i) - pick(st.ab, i)) * cc) >> 7) + pick(st.ad, i);
            c[i] = if st.colclamp { v.clamp(0, 255) as u32 } else { (v & 0xFF) as u32 };
        }
    }
    if st.dthe && (fpsm == PSMCT16 || fpsm == PSMCT16S) {
        let d = st.dimx[(uy & 3) as usize][(ux & 3) as usize];
        for i in 0..3 {
            let v = c[i] as i32 + d;
            c[i] = if st.colclamp { v.clamp(0, 255) as u32 } else { (v & 0xFF) as u32 };
        }
    }
    if st.fba {
        c[3] |= 0x80;
    }
    if write_fb {
        let mut out = match fpsm {
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => (c[0] >> 3) | ((c[1] >> 3) << 5) | ((c[2] >> 3) << 10) | (((c[3] >> 7) & 1) << 15),
            _ => c[0] | (c[1] << 8) | (c[2] << 16) | (c[3] << 24),
        };
        let mut msk = st.fbmsk;
        if fpsm == PSMCT16 || fpsm == PSMCT16S {
            msk = ((msk >> 3) & 0x1F) | ((msk >> 6) & 0x3E0) | ((msk >> 9) & 0x7C00) | ((msk >> 16) & 0x8000);
        }
        if write_rgb_only {
            msk |= if fpsm == PSMCT16 || fpsm == PSMCT16S { 0x8000 } else { 0xFF00_0000 };
        }
        out = (out & !msk) | (dst & msk);
        gs.store_pixel(fpsm, st.fbp, st.fbw, ux, uy, out);
    }
    if write_z && ZTE {
        gs.store_pixel(st.zpsm, st.zbp, st.fbw, ux, uy, z);
    }
}


/// `T5_DUPCHECK=1`: primitives drawn more than once with identical state and vertices within
/// one rendered frame (a double-execution bug would show up here), reported per frame.
static DUP: std::sync::Mutex<Option<(std::collections::HashMap<u64, u32>, u64)>> = std::sync::Mutex::new(None);

fn dup_note(st: &Setup, kind: u32, vs: &[Vertex]) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (kind, st.fbp, st.zbp, st.ctx.alpha, st.ctx.test, st.ctx.tex0, st.ctx.frame).hash(&mut h);
    for v in vs {
        (v.x, v.y, v.z, v.r, v.g, v.b, v.a, v.q.to_bits(), v.s.to_bits(), v.t.to_bits(), v.u, v.v).hash(&mut h);
    }
    let mut g = DUP.lock().unwrap();
    let (map, px) = g.get_or_insert_with(Default::default);
    *map.entry(h.finish()).or_insert(0) += 1;
    *px += 1;
}

fn dup_report() {
    let mut g = DUP.lock().unwrap();
    if let Some((map, total)) = g.take() {
        let dups: u32 = map.values().map(|c| c - 1).sum();
        let max = map.values().max().copied().unwrap_or(0);
        crate::diag::log(format!("DUPCHECK frame: prims {total} unique {} duplicates {dups} max repeat {max}", map.len()));
    }
}
