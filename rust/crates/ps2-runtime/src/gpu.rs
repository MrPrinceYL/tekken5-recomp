//! The GPU device for the GS rasterizer (wgpu compute). The GS's local memory lives in a
//! storage buffer; shaders compute exactly what `raster.rs` computes (integer arithmetic only).

/// Run a future to completion on this thread (wgpu's adapter / device requests).
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(std::sync::Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// A high-performance adapter with 64-bit shader integers, or None.
    pub fn new() -> Option<Gpu> {
        // Vulkan: DX12 has no 64-bit shader integers (WGPU_BACKEND overrides for tests).
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        if std::env::var_os("WGPU_BACKEND").is_none() {
            desc.backends = wgpu::Backends::VULKAN;
        }
        let instance = wgpu::Instance::new(desc);
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).ok()?;
        if !adapter.features().contains(wgpu::Features::SHADER_INT64) {
            crate::diag::log(format!("GPU {}: no SHADER_INT64", adapter.get_info().name));
            return None;
        }
        let limits = wgpu::Limits { max_storage_buffer_binding_size: 256 << 20, max_buffer_size: 256 << 20, ..wgpu::Limits::default() };
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gs"),
            required_features: wgpu::Features::SHADER_INT64,
            required_limits: limits,
            ..Default::default()
        }))
        .ok()?;
        crate::diag::log(format!("GPU {} ({:?})", adapter.get_info().name, adapter.get_info().backend));
        Some(Gpu { device, queue })
    }

    /// Copy `src` back to the host (blocking).
    pub fn read_back(&self, src: &wgpu::Buffer, offset: u64, size: u64) -> Vec<u8> {
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor { label: None, size, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(src, offset, &staging, 0, size);
        self.queue.submit([enc.finish()]);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let v = slice.get_mapped_range().to_vec();
        staging.unmap();
        v
    }
}

/// A primitive as the shader reads it (`gs.wgsl` struct Prim, packed by `raster::gpu_prim`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GPrim {
    /// kind, state, clut (slot | clut16 << 31), qs; x0, y0, x1, y1; ox, oy.
    pub h: [i32; 16],
    pub lx: [i64; 4],
    pub misc: [i64; 4],
    pub e: [[i64; 4]; 3],
    pub p: [[i64; 4]; 9],
}
unsafe impl bytemuck::Zeroable for GPrim {}
unsafe impl bytemuck::Pod for GPrim {}

const STATES: usize = 8192;
const PRIMS: usize = 65536;
const TILES: usize = 1 << 18;
const LISTS: usize = 8 << 20;
const DATA: usize = 8 << 20;
const PARAMS: usize = 4096;
const CLUTS: u32 = 4096;
/// Words per state (`gs.wgsl` STATE_WORDS).
pub const STATE_WORDS: usize = 64;

#[derive(Clone, Copy)]
enum Pipe {
    Raster,
    Serial,
    Upload,
    Copy,
    Clut,
}

/// The GS's local memory and rasterizer on the GPU. The GS thread records commands in GS order
/// (raster batches, uploads, local copies, CLUT loads); `submit` sends them, `sync_mem` reads
/// local memory back for whatever the CPU must see (display read-out, local -> host).
pub struct GsGpu {
    g: Gpu,
    vram: wgpu::Buffer,
    states: wgpu::Buffer,
    prims: wgpu::Buffer,
    tiles: wgpu::Buffer,
    lists: wgpu::Buffer,
    data: wgpu::Buffer,
    params: wgpu::Buffer,
    bg: wgpu::BindGroup,
    pipes: [wgpu::ComputePipeline; 5],
    st_words: Vec<u32>,
    st_index: std::collections::HashMap<usize, u32>,
    st_keep: Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    prims_v: Vec<GPrim>,
    tiles_v: Vec<[u32; 4]>,
    lists_v: Vec<u32>,
    data_v: Vec<u32>,
    params_v: Vec<[u32; 64]>,
    cmds: Vec<(Pipe, u32, u32)>,
    /// Per 16x16 tile of the 2048x2048 space: primitives of the batch being binned.
    tile_n: Vec<u32>,
    /// The CLUT slot the next primitive reads.
    pub clut_slot: u32,
    pub submits: u64,
    /// Display read-out: staging copies of local memory (busy until read out) and the thread
    /// that reads them out.
    ring: Vec<(wgpu::Buffer, std::sync::Arc<std::sync::atomic::AtomicBool>)>,
    ring_next: usize,
    ring_tx: Option<std::sync::mpsc::Sender<(usize, crate::gs::ReadoutJob)>>,
}

/// Swizzle tables (offsets within a page per format, from `gs::addr*` themselves) and the
/// log2 table, at the offsets `gs.wgsl` expects.
fn tables() -> Vec<u32> {
    use crate::gs::*;
    let mut t = vec![0u32; 45056 + 256];
    for y in 0..32 {
        for x in 0..64 {
            t[(y * 64 + x) as usize] = (addr32(0, 1, x, y, false) / 4) as u32;
            t[2048 + (y * 64 + x) as usize] = (addr32(0, 1, x, y, true) / 4) as u32;
        }
    }
    for (v, psm) in [PSMCT16, PSMCT16S, PSMZ16, PSMZ16S].into_iter().enumerate() {
        for y in 0..64 {
            for x in 0..64 {
                t[4096 + v * 4096 + (y * 64 + x) as usize] = (addr16(0, 1, x, y, psm) / 2) as u32;
            }
        }
    }
    for y in 0..64 {
        for x in 0..128 {
            t[20480 + (y * 128 + x) as usize] = addr8(0, 2, x, y) as u32;
        }
    }
    for y in 0..128 {
        for x in 0..128 {
            t[28672 + (y * 128 + x) as usize] = addr4(0, 2, x, y) as u32;
        }
    }
    for i in 0..256 {
        t[45056 + i] = crate::raster::log2_tab(i) as u32;
    }
    t
}

impl GsGpu {
    /// The GPU with local memory `mem` and the CLUT `clut` in slot 0, or None without a GPU.
    pub fn new(mem: &[u8], clut: &[u32; 256]) -> Option<GsGpu> {
        use wgpu::util::DeviceExt;
        let g = Gpu::new()?;
        let d = &g.device;
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let buf = |label: &str, size: usize, usage: wgpu::BufferUsages| d.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size as u64, usage, mapped_at_creation: false });
        let vram = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("vram"), contents: mem, usage: storage | wgpu::BufferUsages::COPY_SRC });
        let tabs = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("tabs"), contents: bytemuck::cast_slice(&tables()), usage: storage });
        let states = buf("states", STATES * STATE_WORDS * 4, storage);
        let prims = buf("prims", PRIMS * std::mem::size_of::<GPrim>(), storage);
        let tiles = buf("tiles", TILES * 16, storage);
        let lists = buf("lists", LISTS * 4, storage);
        let mut cl = vec![0u32; CLUTS as usize * 256];
        cl[..256].copy_from_slice(clut);
        let cluts = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("cluts"), contents: bytemuck::cast_slice(&cl), usage: storage });
        let data = buf("data", DATA * 4, storage);
        let params = buf("params", PARAMS * 256, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST);
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("gs.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("gs.wgsl").into()) });
        let sto = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gs"),
            entries: &[
                sto(0, false),
                sto(1, true),
                sto(2, true),
                sto(3, true),
                sto(4, true),
                sto(5, true),
                sto(6, false),
                sto(7, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: true, min_binding_size: std::num::NonZeroU64::new(64) },
                    count: None,
                },
            ],
        });
        let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gs"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: vram.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: tabs.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: states.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: prims.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: tiles.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: lists.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: cluts.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: data.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &params, offset: 0, size: std::num::NonZeroU64::new(64) }) },
            ],
        });
        let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("gs"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
        let pipe = |entry: &str| d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: Some(&pl), module: &module, entry_point: Some(entry), compilation_options: Default::default(), cache: None });
        let pipes = [pipe("raster"), pipe("raster_serial"), pipe("upload"), pipe("copy"), pipe("clut")];
        Some(GsGpu {
            vram,
            states,
            prims,
            tiles,
            lists,
            data,
            params,
            bg,
            pipes,
            st_words: Vec::new(),
            st_index: Default::default(),
            st_keep: Vec::new(),
            prims_v: Vec::new(),
            tiles_v: Vec::new(),
            lists_v: Vec::new(),
            data_v: Vec::new(),
            params_v: Vec::new(),
            cmds: Vec::new(),
            tile_n: vec![0; 128 * 128],
            clut_slot: 0,
            submits: 0,
            ring: Vec::new(),
            ring_next: 0,
            ring_tx: None,
            g,
        })
    }

    fn param(&mut self, p: [u32; 16]) -> u32 {
        let mut w = [0u32; 64];
        w[..16].copy_from_slice(&p);
        self.params_v.push(w);
        (self.params_v.len() - 1) as u32
    }

    /// Room for one more command with `prims` primitives and `words` transfer data (submits
    /// what is recorded if not).
    pub fn reserve(&mut self, prims: usize, words: usize) {
        if self.params_v.len() + 1 > PARAMS
            || self.prims_v.len() + prims > PRIMS
            || self.st_words.len() / STATE_WORDS + prims > STATES
            || self.tiles_v.len() + 128 * 128 > TILES
            || self.lists_v.len() + prims * 1200 > LISTS
            || self.data_v.len() + words > DATA
        {
            self.submit();
        }
    }

    /// The index of state `key` (kept alive by `keep` until the submit), packed by `words`.
    pub fn state(&mut self, key: usize, keep: std::sync::Arc<dyn std::any::Any + Send + Sync>, words: impl FnOnce() -> [u32; STATE_WORDS]) -> u32 {
        if let Some(&i) = self.st_index.get(&key) {
            return i;
        }
        let i = (self.st_words.len() / STATE_WORDS) as u32;
        self.st_words.extend_from_slice(&words());
        self.st_index.insert(key, i);
        self.st_keep.push(keep);
        i
    }

    pub fn prim_count(&self) -> usize {
        self.prims_v.len()
    }

    pub fn push_prim(&mut self, p: GPrim) {
        self.prims_v.push(p);
    }

    /// Draw the primitives pushed since index `first` (one batch: same frame / depth buffer).
    /// `serial`: frame and depth buffer overlap, one workgroup draws primitive by primitive.
    pub fn raster(&mut self, first: usize, serial: bool) {
        let count = self.prims_v.len() - first;
        if count == 0 {
            return;
        }
        if serial {
            let p = self.param([0, 0, first as u32, count as u32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            self.cmds.push((Pipe::Serial, p, 1));
            return;
        }
        // Bin by bounding box into 16x16 tiles, primitive order kept per tile.
        let tile_rect = |h: &[i32; 16]| ((h[4] >> 4) as usize, (h[5] >> 4) as usize, (((h[6] + 15) >> 4) as usize).min(128), (((h[7] + 15) >> 4) as usize).min(128));
        let t0 = self.tiles_v.len();
        let mut touched: Vec<u32> = Vec::new();
        for pr in &self.prims_v[first..] {
            let (x0, y0, x1, y1) = tile_rect(&pr.h);
            for ty in y0..y1 {
                for tx in x0..x1 {
                    let t = ty * 128 + tx;
                    if self.tile_n[t] == 0 {
                        touched.push(t as u32);
                    }
                    self.tile_n[t] += 1;
                }
            }
        }
        // Offsets, then the lists; tile_n becomes the tile's fill position.
        let mut off = self.lists_v.len() as u32;
        for &t in &touched {
            let n = self.tile_n[t as usize];
            self.tiles_v.push([(t % 128) | ((t / 128) << 16), off, n, 0]);
            self.tile_n[t as usize] = off;
            off += n;
        }
        self.lists_v.resize(off as usize, 0);
        for i in first..self.prims_v.len() {
            let (x0, y0, x1, y1) = tile_rect(&self.prims_v[i].h);
            for ty in y0..y1 {
                for tx in x0..x1 {
                    let t = ty * 128 + tx;
                    let o = self.tile_n[t];
                    self.lists_v[o as usize] = i as u32;
                    self.tile_n[t] = o + 1;
                }
            }
        }
        for &t in &touched {
            self.tile_n[t as usize] = 0;
        }
        let n = (self.tiles_v.len() - t0) as u32;
        let p = self.param([0, t0 as u32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        self.cmds.push((Pipe::Raster, p, n));
    }

    /// A host -> local transfer chunk (`gs::Gs::run_upload`).
    pub fn upload(&mut self, u: &crate::gs::Upload) {
        use crate::gs::*;
        let bits: u32 = match u.dpsm {
            PSMCT32 | PSMZ32 => 32,
            PSMCT24 | PSMZ24 => 24,
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => 16,
            PSMT8 | PSMT8H => 8,
            _ => 4,
        };
        let npix = (u.data.len() as u64 * 8 / bits as u64) as u32;
        if npix == 0 || u.rrw == 0 {
            return;
        }
        let words = u.data.len().div_ceil(4);
        self.reserve(0, words);
        let base = self.data_v.len() as u32;
        for c in u.data.chunks(4) {
            let mut b = [0u8; 4];
            b[..c.len()].copy_from_slice(c);
            self.data_v.push(u32::from_le_bytes(b));
        }
        let p = self.param([u.dbp, u.dbw, u.dpsm, bits, u.dsax, u.dsay, u.rrw, u.rrh, u.x, u.y, base, npix, 0, 0, 0, 0]);
        self.cmds.push((Pipe::Upload, p, npix.div_ceil(256)));
    }

    /// A local -> local transfer (`gs::Gs::local_to_local`); `serial` when source and
    /// destination may overlap. src / dst: (bp, bw, psm).
    #[allow(clippy::too_many_arguments)]
    pub fn copy(&mut self, src: (u32, u32, u32), dst: (u32, u32, u32), dir: u32, spos: (u32, u32), dpos: (u32, u32), size: (u32, u32), serial: bool) {
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        self.reserve(0, 0);
        let p = self.param([src.0, src.1, src.2, dir, dst.0, dst.1, dst.2, serial as u32, spos.0, spos.1, dpos.0, dpos.1, size.0, size.1, 0, 0]);
        let n = if serial { 1 } else { (size.0 * size.1).div_ceil(256) };
        self.cmds.push((Pipe::Copy, p, n));
    }

    /// A CLUT load (TEX0 `t`, TEXCLUT `texclut`): later primitives read the new slot.
    pub fn clut(&mut self, t: u64, texclut: u64) {
        self.reserve(0, 0);
        let dst = (self.clut_slot + 1) % CLUTS;
        let p = self.param([t as u32, (t >> 32) as u32, texclut as u32, (texclut >> 32) as u32, self.clut_slot, dst, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        self.cmds.push((Pipe::Clut, p, 1));
        self.clut_slot = dst;
    }

    /// Send everything recorded to the GPU.
    pub fn submit(&mut self) {
        if self.cmds.is_empty() {
            return;
        }
        let q = &self.g.queue;
        let w = |b: &wgpu::Buffer, v: &[u8]| {
            if !v.is_empty() {
                q.write_buffer(b, 0, v);
            }
        };
        w(&self.states, bytemuck::cast_slice(&self.st_words));
        w(&self.prims, bytemuck::cast_slice(&self.prims_v));
        w(&self.tiles, bytemuck::cast_slice(&self.tiles_v));
        w(&self.lists, bytemuck::cast_slice(&self.lists_v));
        w(&self.data, bytemuck::cast_slice(&self.data_v));
        w(&self.params, bytemuck::cast_slice(&self.params_v));
        let mut enc = self.g.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            for &(pipe, p, n) in &self.cmds {
                pass.set_pipeline(&self.pipes[pipe as usize]);
                pass.set_bind_group(0, &self.bg, &[p * 256]);
                pass.dispatch_workgroups(n, 1, 1);
            }
        }
        q.submit([enc.finish()]);
        self.submits += 1;
        self.cmds.clear();
        self.st_words.clear();
        self.st_index.clear();
        self.st_keep.clear();
        self.prims_v.clear();
        self.tiles_v.clear();
        self.lists_v.clear();
        self.data_v.clear();
        self.params_v.clear();
    }

    /// The display read-out of everything drawn so far, on the read-out thread: local memory is
    /// copied on the GPU and read back there; the GS thread goes on (it waits only when all
    /// staging copies are still being read out).
    pub fn readout_async(&mut self, job: crate::gs::ReadoutJob) {
        const RING: usize = 3;
        if self.ring.is_empty() {
            for _ in 0..RING {
                let b = self.g.device.create_buffer(&wgpu::BufferDescriptor { label: Some("readout"), size: crate::gs::VRAM as u64, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
                self.ring.push((b, Default::default()));
            }
            let (tx, rx) = std::sync::mpsc::channel::<(usize, crate::gs::ReadoutJob)>();
            let ring: Vec<_> = self.ring.iter().map(|(b, f)| (b.clone(), f.clone())).collect();
            let device = self.g.device.clone();
            std::thread::Builder::new()
                .name("gs readout".into())
                .spawn(move || {
                    let mut mirror = crate::gs::Gs::new();
                    while let Ok((i, job)) = rx.recv() {
                        let (b, busy) = &ring[i];
                        let slice = b.slice(..);
                        slice.map_async(wgpu::MapMode::Read, |_| {});
                        let _ = device.poll(wgpu::PollType::wait_indefinitely());
                        mirror.mem.copy_from_slice(&slice.get_mapped_range());
                        b.unmap();
                        busy.store(false, std::sync::atomic::Ordering::Release);
                        job.run(&mirror);
                    }
                })
                .unwrap();
            self.ring_tx = Some(tx);
        }
        let i = self.ring_next;
        self.ring_next = (i + 1) % self.ring.len();
        while self.ring[i].1.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::yield_now();
        }
        self.submit();
        let mut enc = self.g.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(&self.vram, 0, &self.ring[i].0, 0, crate::gs::VRAM as u64);
        self.g.queue.submit([enc.finish()]);
        self.ring[i].1.store(true, std::sync::atomic::Ordering::Release);
        let _ = self.ring_tx.as_ref().unwrap().send((i, job));
    }

    /// Everything drawn so far into `mem` (blocking).
    pub fn sync_mem(&mut self, mem: &mut [u8]) {
        self.submit();
        let v = self.g.read_back(&self.vram, 0, mem.len() as u64);
        mem.copy_from_slice(&v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::util::DeviceExt;

    /// The integer operations the rasterizer relies on give the CPU's results on the GPU.
    #[test]
    fn int64_matches_cpu() {
        let Some(g) = Gpu::new() else { panic!("no GPU with SHADER_INT64") };
        let src = r#"
@group(0) @binding(0) var<storage, read> inp: array<i64>;
@group(0) @binding(1) var<storage, read_write> outp: array<i64>;
fn div_floor64(n: i64, d: i64) -> i64 {
    // Not `%`: i64 remainder is wrong on some backends; r = n - q·d is exact.
    let q = n / d;
    let r = n - q * d;
    if (r != 0li) && ((r < 0li) != (d < 0li)) { return q - 1li; }
    return q;
}
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= arrayLength(&inp) / 2u { return; }
    let a = inp[2u * i];
    let b = inp[2u * i + 1u];
    outp[4u * i] = a * b;
    outp[4u * i + 1u] = div_floor64(a, b);
    outp[4u * i + 2u] = a >> 16u;
    outp[4u * i + 3u] = a + b * 2047li;
}
"#;
        let mut v: Vec<i64> = Vec::new();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for k in 0..4096 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let a = (x as i64) >> (k % 40);
            let b = ((x.rotate_left(29) as i64) >> (20 + k % 40)) | 1;
            v.push(a);
            v.push(if k % 7 == 0 { -b } else { b });
        }
        let inp = g.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&v), usage: wgpu::BufferUsages::STORAGE });
        let n = v.len() / 2;
        let out = g.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (n * 32) as u64, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
        let module = g.device.create_shader_module(wgpu::ShaderModuleDescriptor { label: None, source: wgpu::ShaderSource::Wgsl(src.into()) });
        let pipe = g.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: None, layout: None, module: &module, entry_point: Some("main"), compilation_options: Default::default(), cache: None });
        let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipe.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: inp.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: out.as_entire_binding() }],
        });
        let mut enc = g.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&pipe);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(n.div_ceil(64) as u32, 1, 1);
        }
        g.queue.submit([enc.finish()]);
        let got: Vec<i64> = bytemuck::cast_slice(&g.read_back(&out, 0, (n * 32) as u64)).to_vec();
        let mut bad = 0;
        for i in 0..n {
            let (a, b) = (v[2 * i], v[2 * i + 1]);
            let want = [a.wrapping_mul(b), crate::raster::div_floor64(a, b), a >> 16, a.wrapping_add(b.wrapping_mul(2047))];
            if got[4 * i..4 * i + 4] != want {
                if bad < 5 {
                    eprintln!("{a} {b}: gpu {:?} cpu {:?}", &got[4 * i..4 * i + 4], want);
                }
                bad += 1;
            }
        }
        assert_eq!(bad, 0);
    }
}
