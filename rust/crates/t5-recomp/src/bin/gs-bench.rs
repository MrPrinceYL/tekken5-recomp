//! GS replay benchmark / parity check: `gs-bench [dir]` replays a T5_GSCAPTURE capture and
//! prints a hash of local memory per field (compare runs with different rasteriser settings,
//! e.g. T5_NODIRECT=1) and the time per field.
fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "gscap".into());
    let _shared = t5_recomp::boot();
    let r = ps2_runtime::gscap::replay(&dir);
    let total: f64 = r.iter().map(|x| x.1).sum();
    let mut all: u64 = 0xcbf2_9ce4_8422_2325;
    for (h, _) in &r {
        all = (all ^ h).wrapping_mul(0x100_0000_01b3);
    }
    if std::env::var_os("GSBENCH_FIELDS").is_some() {
        for (i, (h, t)) in r.iter().enumerate() {
            println!("field {i}: {h:016x} {t:.2} ms");
        }
    }
    println!("{} fields, {:.1} ms total, {:.2} ms/field, hash {all:016x}", r.len(), total, total / r.len().max(1) as f64);
}
