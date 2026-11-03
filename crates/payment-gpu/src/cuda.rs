//! Host side of the CUDA engines: buffers, launches, timing.

use cust::event::{Event, EventFlags};
use cust::memory::{CopyDestination, DeviceBuffer, DeviceCopy};
use h5i_app_gpu::batch::Backend;
use h5i_app_gpu_backend_nvvm::{Failure, Nvvm};
use payment_batch::PolicyBatch;
use payment_domain::policy::{Limits, PaymentReq};
use payment_reconcile::rules::{capacity, NONE};
use payment_reconcile::{validate, Recon, Record, Report};
use std::time::Instant;

/// The device crate (`../device`) compiled by rustc_codegen_nvvm
/// (`build-device.sh`; `build-device.sh --check` rebuilds and compares).
/// Its entry points run the same `policy::check` and `rules::internal_row` /
/// `external_row` source as the CPU engines.
const PTX: &str = include_str!("../kernels/payment_gpu_device.ptx");
const BLOCK: u32 = 256;

/// Where the time of the last run went.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    /// Allocation, host→device copies and table initialization (wall clock).
    pub h2d_ms: f64,
    /// All kernels, from CUDA events on the stream.
    pub kernel_ms: f32,
    /// Device→host copies of the results (wall clock).
    pub d2h_ms: f64,
    /// The whole call, wall clock.
    pub total_ms: f64,
    /// Bytes allocated on the device for this call.
    pub device_bytes: usize,
}

pub struct Gpu {
    /// Buffers kept between calls (a long-lived worker reuses them); they
    /// are declared before `rt` so they are freed before the context.
    cache: Cache,
    copies: cust::stream::Stream,
    rt: Nvvm,
    pub last: Timings,
    pub last_cols: ColTimings,
}

impl Gpu {
    /// Create a CUDA context and load the kernels (JIT-compiles the PTX).
    pub fn new() -> Result<Self, Failure> {
        let rt = Nvvm::new(PTX)?;
        let copies = cust::stream::Stream::new(cust::stream::StreamFlags::NON_BLOCKING, None)?;
        Ok(Gpu { cache: Cache::default(), copies, rt, last: Timings::default(), last_cols: ColTimings::default() })
    }

    /// Free and total device memory, in bytes.
    pub fn mem_info() -> Result<(usize, usize), Failure> {
        Ok(cust::memory::mem_get_info()?)
    }
}

/// View a slice of plain `repr(C)` records as 64-bit words.
///
/// # Safety
/// `T` must be `repr(C)`, have no padding, and have a size that is a
/// multiple of 8 and an alignment of 8.
unsafe fn words<T>(xs: &[T]) -> &[u64] {
    assert!(std::mem::size_of::<T>().is_multiple_of(8) && std::mem::align_of::<T>() == 8);
    unsafe { std::slice::from_raw_parts(xs.as_ptr() as *const u64, std::mem::size_of_val(xs) / 8) }
}

fn upload<T: DeviceCopy>(xs: &[T], bytes: &mut usize) -> Result<DeviceBuffer<T>, Failure> {
    // A zero-length buffer is not a valid allocation; kernels never read it.
    let b = if xs.is_empty() { DeviceBuffer::from_slice(&[unsafe { std::mem::zeroed::<T>() }])? } else { DeviceBuffer::from_slice(xs)? };
    *bytes += b.len() * std::mem::size_of::<T>();
    Ok(b)
}

fn grid(n: usize) -> u32 {
    (n as u32).div_ceil(BLOCK)
}

struct DevTable {
    keys: DeviceBuffer<u64>,
    count: DeviceBuffer<u32>,
    first: DeviceBuffer<u32>,
    mask: u64,
}

fn table(n: usize, bytes: &mut usize) -> Result<DevTable, Failure> {
    let cap = capacity(n);
    let mut keys = unsafe { DeviceBuffer::<u64>::uninitialized(cap)? };
    keys.set_8(0xff)?;
    let count = DeviceBuffer::<u32>::zeroed(cap)?;
    let mut first = unsafe { DeviceBuffer::<u32>::uninitialized(cap)? };
    first.set_32(NONE)?;
    *bytes += cap * (8 + 4 + 4);
    Ok(DevTable { keys, count, first, mask: (cap - 1) as u64 })
}

impl Backend<Recon, (), Report> for Gpu {
    type Error = Failure;

    fn run(&mut self, (): (), r: &Recon) -> Result<Report, Failure> {
        validate(r).map_err(|e| Box::new(e) as Failure)?;
        assert_eq!(std::mem::size_of::<Record>(), 24);
        let (ni, ne) = (r.internal.len(), r.external.len());
        let start = Instant::now();
        let mut bytes = 0;
        // SAFETY: `Record` is repr(C) u64, u64, u32, u32: 24 bytes, no padding.
        let int_rows = upload(unsafe { words(&r.internal) }, &mut bytes)?;
        let ext_rows = upload(unsafe { words(&r.external) }, &mut bytes)?;
        let it = table(ni, &mut bytes)?;
        let et = table(ne, &mut bytes)?;
        let codes_i = DeviceBuffer::<u32>::zeroed(ni.max(1))?;
        let matched = DeviceBuffer::<u32>::zeroed(ni.max(1))?;
        let codes_e = DeviceBuffer::<u32>::zeroed(ne.max(1))?;
        bytes += 4 * (2 * ni.max(1) + ne.max(1));
        let h2d = start.elapsed().as_secs_f64() * 1e3;

        let stream = self.rt.stream();
        let m = self.rt.module();
        let build = m.get_function("recon_build")?;
        let probe_i = m.get_function("recon_internal")?;
        let probe_e = m.get_function("recon_external")?;
        let begin = Event::new(EventFlags::DEFAULT)?;
        let end = Event::new(EventFlags::DEFAULT)?;
        begin.record(stream)?;
        // SAFETY: each kernel bounds-checks its row index against n, reads
        // only the buffers passed, and writes disjoint slots (or table slots
        // through atomics); all buffers outlive the stream synchronization.
        unsafe {
            if ne > 0 {
                cust::launch!(build<<<grid(ne), BLOCK, 0, stream>>>(
                    ext_rows.as_device_ptr(), ne, et.keys.as_device_ptr(), et.count.as_device_ptr(), et.first.as_device_ptr(), et.mask))?;
            }
            if ni > 0 {
                cust::launch!(build<<<grid(ni), BLOCK, 0, stream>>>(
                    int_rows.as_device_ptr(), ni, it.keys.as_device_ptr(), it.count.as_device_ptr(), it.first.as_device_ptr(), it.mask))?;
                cust::launch!(probe_i<<<grid(ni), BLOCK, 0, stream>>>(
                    int_rows.as_device_ptr(), ni, ext_rows.as_device_ptr(), ne,
                    it.keys.as_device_ptr(), it.count.as_device_ptr(), it.first.as_device_ptr(), it.mask,
                    et.keys.as_device_ptr(), et.count.as_device_ptr(), et.first.as_device_ptr(), et.mask,
                    codes_i.as_device_ptr(), matched.as_device_ptr()))?;
            }
            if ne > 0 {
                cust::launch!(probe_e<<<grid(ne), BLOCK, 0, stream>>>(
                    ext_rows.as_device_ptr(), ne,
                    it.keys.as_device_ptr(), it.count.as_device_ptr(), it.first.as_device_ptr(), it.mask,
                    et.keys.as_device_ptr(), et.count.as_device_ptr(), et.first.as_device_ptr(), et.mask,
                    codes_e.as_device_ptr()))?;
            }
        }
        end.record(stream)?;
        end.synchronize()?;
        let kernel_ms = end.elapsed_time_f32(&begin)?;
        let t2 = Instant::now();
        let mut out = Report { internal: vec![0; ni.max(1)], matched: vec![0; ni.max(1)], external: vec![0; ne.max(1)] };
        codes_i.copy_to(&mut out.internal)?;
        matched.copy_to(&mut out.matched)?;
        codes_e.copy_to(&mut out.external)?;
        out.internal.truncate(ni);
        out.matched.truncate(ni);
        out.external.truncate(ne);
        let d2h = t2.elapsed().as_secs_f64() * 1e3;
        self.last = Timings { h2d_ms: h2d, kernel_ms, d2h_ms: d2h, total_ms: start.elapsed().as_secs_f64() * 1e3, device_bytes: bytes };
        Ok(out)
    }
}

impl Backend<PolicyBatch, (), Vec<u32>> for Gpu {
    type Error = Failure;

    fn run(&mut self, (): (), b: &PolicyBatch) -> Result<Vec<u32>, Failure> {
        assert_eq!(std::mem::size_of::<PaymentReq>(), 32);
        assert_eq!(std::mem::size_of::<Limits>(), 80);
        let n = b.requests.len();
        if n >= u32::MAX as usize {
            return Err("batch too large".into());
        }
        let start = Instant::now();
        let mut bytes = 0;
        // SAFETY: both are repr(C) with u64-aligned, padding-free layouts
        // (sizes asserted above).
        let reqs = upload(unsafe { words(&b.requests) }, &mut bytes)?;
        let limits = upload(unsafe { words(std::slice::from_ref(&b.limits)) }, &mut bytes)?;
        let merchants = upload(&b.merchants, &mut bytes)?;
        let blocked = upload(&b.blocked, &mut bytes)?;
        let codes = DeviceBuffer::<u32>::zeroed(n.max(1))?;
        bytes += 4 * n.max(1);
        let h2d = start.elapsed().as_secs_f64() * 1e3;
        let stream = self.rt.stream();
        let f = self.rt.module().get_function("policy_check")?;
        let begin = Event::new(EventFlags::DEFAULT)?;
        let end = Event::new(EventFlags::DEFAULT)?;
        begin.record(stream)?;
        if n > 0 {
            // SAFETY: the kernel bounds-checks i < n, reads only these
            // buffers (lengths passed explicitly) and writes out[i].
            unsafe {
                cust::launch!(f<<<grid(n), BLOCK, 0, stream>>>(
                    reqs.as_device_ptr(), n, limits.as_device_ptr(),
                    merchants.as_device_ptr(), b.merchants.len(),
                    blocked.as_device_ptr(), b.blocked.len(), b.n_blocked,
                    codes.as_device_ptr()))?;
            }
        }
        end.record(stream)?;
        end.synchronize()?;
        let kernel_ms = end.elapsed_time_f32(&begin)?;
        let t2 = Instant::now();
        let mut out = vec![0u32; n.max(1)];
        codes.copy_to(&mut out)?;
        out.truncate(n);
        let d2h = t2.elapsed().as_secs_f64() * 1e3;
        self.last = Timings { h2d_ms: h2d, kernel_ms, d2h_ms: d2h, total_ms: start.elapsed().as_secs_f64() * 1e3, device_bytes: bytes };
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Reconciliation on columns (J1): the batch worker's binary records files.

/// Where the time of the last [`Gpu::reconcile_cols`] went (milliseconds,
/// wall clock unless noted). The input copies overlap the table builds and
/// the external probe, so the phases do not add up to `total_ms`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColTimings {
    /// The reserved-key scan of both key columns (CPU, every thread).
    pub validate_ms: f64,
    /// Growing the cached device and pinned buffers (0 when they fit).
    pub alloc_ms: f64,
    /// From the first input byte copied to the last one on the device:
    /// mapped file → pinned staging (CPU) → device (DMA), overlapped.
    pub h2d_ms: f64,
    /// Kernels and table initialization (CUDA events; overlaps `h2d_ms`).
    pub kernel_ms: f32,
    /// From the last input byte on the device to the last kernel's end.
    pub after_h2d_ms: f64,
    /// The one-byte codes back to pinned host memory.
    pub d2h_ms: f64,
    pub total_ms: f64,
    pub h2d_bytes: usize,
    pub device_bytes: usize,
}

/// The codes of a column run, in pinned host memory owned by the [`Gpu`]
/// (valid until its next call); `matched` only if asked for.
pub struct ColsOut<'a> {
    pub internal: &'a [u8],
    pub external: &'a [u8],
    pub matched: Option<Vec<u32>>,
    pub timings: ColTimings,
}

/// Staging buffers for host→device copies: `NSTAGE` × `STAGE` bytes of
/// pinned memory, filled by the CPU from the mapped file while the DMA
/// engine drains the previous ones.
const STAGE: usize = 8 << 20;
const NSTAGE: usize = 3;

// Device buffer slots of the column path.
const IKEY: usize = 0;
const EKEY: usize = 4;
const ITAB: usize = 8;
const ETAB: usize = 11;
const CODES_I: usize = 14;
const CODES_E: usize = 15;
const SLOTS: usize = 16;

/// Device and pinned buffers kept between calls; they only grow.
#[derive(Default)]
struct Cache {
    dev: Vec<Option<DeviceBuffer<u8>>>,
    stage: Vec<LockedBuffer<u8>>,
    staged: Vec<Event>,
    out_i: Option<LockedBuffer<u8>>,
    out_e: Option<LockedBuffer<u8>>,
}

use cust::memory::{AsyncCopyDestination, DevicePointer, DeviceSlice, LockedBuffer};
use cust::stream::{Stream, StreamWaitEventFlags};
use payment_reconcile::{validate_cols, Cols};

impl Cache {
    /// Device slot `slot` with at least `bytes` bytes (and at least 8).
    fn dev(&mut self, slot: usize, bytes: usize, grown: &mut usize) -> Result<DevicePointer<u8>, Failure> {
        if self.dev.is_empty() {
            self.dev.resize_with(SLOTS, || None);
        }
        let bytes = bytes.max(8);
        let s = &mut self.dev[slot];
        if s.as_ref().is_none_or(|b| b.len() < bytes) {
            *s = None; // free first: device memory is the limit at 100M rows
            *s = Some(unsafe { DeviceBuffer::<u8>::uninitialized(bytes)? });
            *grown += bytes;
        }
        Ok(s.as_ref().expect("allocated").as_device_ptr())
    }

    fn pinned(slot: &mut Option<LockedBuffer<u8>>, n: usize) -> Result<(), Failure> {
        if slot.as_ref().is_none_or(|b| b.len() < n.max(1)) {
            *slot = None;
            *slot = Some(unsafe { LockedBuffer::<u8>::uninitialized(n.max(1))? });
        }
        Ok(())
    }

    fn device_bytes(&self) -> usize {
        self.dev.iter().flatten().map(|b| b.len()).sum()
    }
}

/// The bytes of a column.
fn bytes<T: Copy>(xs: &[T]) -> &[u8] {
    // SAFETY: plain integers; any byte view of them is valid.
    unsafe { std::slice::from_raw_parts(xs.as_ptr() as *const u8, std::mem::size_of_val(xs)) }
}

/// Copy `src` to device address `dst` through the staging buffers, on the
/// `copies` stream: the CPU fills buffer `k % NSTAGE` once its previous DMA
/// has finished, then queues the DMA and returns to the next chunk.
fn stage_upload(cache: &mut Cache, copies: &Stream, src: &[u8], dst: DevicePointer<u8>, k: &mut usize) -> Result<(), Failure> {
    for (c, chunk) in src.chunks(STAGE).enumerate() {
        let b = *k % NSTAGE;
        *k += 1;
        cache.staged[b].synchronize()?;
        let stage = &mut cache.stage[b][..chunk.len()];
        stage.copy_from_slice(chunk);
        // SAFETY: `dst` has room for `src` (allocated by the caller), the
        // range is inside it, and the staging buffer is not touched again
        // until its event (recorded after this copy) has completed.
        unsafe {
            let d = DeviceSlice::from_raw_parts_mut(dst.add(c * STAGE), chunk.len());
            d.async_copy_from(&*stage, copies)?;
        }
        cache.staged[b].record(copies)?;
    }
    Ok(())
}

impl Gpu {
    /// Reconcile two sides given as columns (the binary records file).
    ///
    /// The input goes through pinned staging buffers in 8 MiB chunks on a
    /// copy stream: both key columns first, so the table builds and the
    /// external probe run while the other columns are still being copied;
    /// the internal probe runs when everything is on the device. Codes are
    /// one byte per row, copied back to pinned memory. Device and pinned
    /// buffers are cached and reused by the next call (a long-lived
    /// worker). Same codes as every engine (`tests/differential`).
    pub fn reconcile_cols(&mut self, int: Cols<'_>, ext: Cols<'_>, with_matched: bool) -> Result<ColsOut<'_>, Failure> {
        let start = Instant::now();
        validate_cols(&int, &ext).map_err(|e| Box::new(e) as Failure)?;
        let validate_ms = start.elapsed().as_secs_f64() * 1e3;
        let (ni, ne) = (int.len(), ext.len());
        let (ci, ce) = (capacity(ni), capacity(ne));
        let t = Instant::now();
        let mut grown = 0;
        let cache = &mut self.cache;
        if cache.stage.is_empty() {
            for _ in 0..NSTAGE {
                cache.stage.push(unsafe { LockedBuffer::<u8>::uninitialized(STAGE)? });
                cache.staged.push(Event::new(EventFlags::DISABLE_TIMING)?);
            }
        }
        let icols = [bytes(int.key), bytes(int.amount), bytes(int.currency), bytes(int.status)];
        let ecols = [bytes(ext.key), bytes(ext.amount), bytes(ext.currency), bytes(ext.status)];
        let mut ip = [DevicePointer::null(); 4];
        let mut ep = [DevicePointer::null(); 4];
        for k in 0..4 {
            ip[k] = cache.dev(IKEY + k, icols[k].len(), &mut grown)?;
            ep[k] = cache.dev(EKEY + k, ecols[k].len(), &mut grown)?;
        }
        let tab = |cache: &mut Cache, base: usize, cap: usize, grown: &mut usize| -> Result<[DevicePointer<u8>; 3], Failure> {
            Ok([cache.dev(base, cap * 8, grown)?, cache.dev(base + 1, cap * 4, grown)?, cache.dev(base + 2, cap * 4, grown)?])
        };
        let it = tab(cache, ITAB, ci, &mut grown)?;
        let et = tab(cache, ETAB, ce, &mut grown)?;
        let codes_i = cache.dev(CODES_I, ni, &mut grown)?;
        let codes_e = cache.dev(CODES_E, ne, &mut grown)?;
        Cache::pinned(&mut cache.out_i, ni)?;
        Cache::pinned(&mut cache.out_e, ne)?;
        let alloc_ms = if grown > 0 { t.elapsed().as_secs_f64() * 1e3 } else { 0.0 };

        let ks = self.rt.stream();
        let m = self.rt.module();
        let build = m.get_function("recon_build_cols")?;
        let probe_i = m.get_function("recon_internal_cols")?;
        let probe_e = m.get_function("recon_external_cols")?;
        let ev = || Event::new(EventFlags::DEFAULT);
        let (k0, k1, k2, k3) = (ev()?, ev()?, ev()?, ev()?);
        let keys_in = Event::new(EventFlags::DISABLE_TIMING)?;
        let all_in = Event::new(EventFlags::DISABLE_TIMING)?;
        // Empty tables: keys EMPTY, counts 0, first NONE.
        // SAFETY: each range is inside its slot (allocated above).
        unsafe {
            for (t, cap) in [(it, ci), (et, ce)] {
                DeviceSlice::from_raw_parts_mut(t[0], cap * 8).set_8_async(0xff, ks)?;
                DeviceSlice::from_raw_parts_mut(t[1], cap * 4).set_8_async(0, ks)?;
                DeviceSlice::from_raw_parts_mut(t[2], cap * 4).set_8_async(0xff, ks)?;
            }
        }
        let tc = Instant::now();
        let mut k = 0;
        stage_upload(cache, &self.copies, ecols[0], ep[0], &mut k)?;
        stage_upload(cache, &self.copies, icols[0], ip[0], &mut k)?;
        keys_in.record(&self.copies)?;
        ks.wait_event(&keys_in, StreamWaitEventFlags::DEFAULT)?;
        k0.record(ks)?;
        let u64p = |p: DevicePointer<u8>| p.cast::<u64>();
        let u32p = |p: DevicePointer<u8>| p.cast::<u32>();
        // SAFETY: as in `run` above: every kernel bounds-checks its row
        // index, reads only the buffers passed (lengths explicit, tables of
        // `mask + 1` slots), and writes its own row's code or table slots
        // through atomics; all buffers live in the cache past the final
        // synchronization; launches are ordered after the copies they read
        // by the events.
        unsafe {
            if ne > 0 {
                cust::launch!(build<<<grid(ne), BLOCK, 0, ks>>>(u64p(ep[0]), ne, 0u32, u64p(et[0]), u32p(et[1]), u32p(et[2]), (ce - 1) as u64))?;
            }
            if ni > 0 {
                cust::launch!(build<<<grid(ni), BLOCK, 0, ks>>>(u64p(ip[0]), ni, 0u32, u64p(it[0]), u32p(it[1]), u32p(it[2]), (ci - 1) as u64))?;
            }
            if ne > 0 {
                cust::launch!(probe_e<<<grid(ne), BLOCK, 0, ks>>>(
                    u64p(ep[0]), ne,
                    u64p(it[0]), u32p(it[1]), u32p(it[2]), (ci - 1) as u64,
                    u64p(et[0]), u32p(et[1]), u32p(et[2]), (ce - 1) as u64,
                    codes_e))?;
            }
        }
        k1.record(ks)?;
        for c in 1..4 {
            stage_upload(cache, &self.copies, icols[c], ip[c], &mut k)?;
            stage_upload(cache, &self.copies, ecols[c], ep[c], &mut k)?;
        }
        all_in.record(&self.copies)?;
        ks.wait_event(&all_in, StreamWaitEventFlags::DEFAULT)?;
        let matched = if with_matched { Some(DeviceBuffer::<u32>::zeroed(ni.max(1))?) } else { None };
        k2.record(ks)?;
        unsafe {
            if ni > 0 {
                let mp = matched.as_ref().map_or(DevicePointer::null(), |b| b.as_device_ptr());
                cust::launch!(probe_i<<<grid(ni), BLOCK, 0, ks>>>(
                    u64p(ip[0]), u64p(ip[1]), u32p(ip[2]), u32p(ip[3]), ni,
                    u64p(ep[0]), u64p(ep[1]), u32p(ep[2]), u32p(ep[3]), ne,
                    u64p(it[0]), u32p(it[1]), u32p(it[2]), (ci - 1) as u64,
                    u64p(et[0]), u32p(et[1]), u32p(et[2]), (ce - 1) as u64,
                    codes_i, mp))?;
            }
        }
        k3.record(ks)?;
        all_in.synchronize()?;
        let h2d_ms = tc.elapsed().as_secs_f64() * 1e3;
        let t_in = Instant::now();
        k3.synchronize()?;
        let after_h2d_ms = t_in.elapsed().as_secs_f64() * 1e3;
        let kernel_ms = k1.elapsed_time_f32(&k0)? + k3.elapsed_time_f32(&k2)?;
        let t2 = Instant::now();
        let out_i = &mut cache.out_i.as_mut().expect("allocated")[..ni];
        let out_e = &mut cache.out_e.as_mut().expect("allocated")[..ne];
        // SAFETY: the device ranges hold ni / ne codes; the pinned
        // destinations are not read before the stream synchronization.
        unsafe {
            DeviceSlice::from_raw_parts(codes_i, ni).async_copy_to(out_i, ks)?;
            DeviceSlice::from_raw_parts(codes_e, ne).async_copy_to(out_e, ks)?;
        }
        ks.synchronize()?;
        let matched = match matched {
            Some(b) => {
                let mut v = vec![0u32; ni.max(1)];
                b.copy_to(&mut v)?;
                v.truncate(ni);
                Some(v)
            }
            None => None,
        };
        let d2h_ms = t2.elapsed().as_secs_f64() * 1e3;
        self.last_cols = ColTimings {
            validate_ms,
            alloc_ms,
            h2d_ms,
            kernel_ms,
            after_h2d_ms,
            d2h_ms,
            total_ms: start.elapsed().as_secs_f64() * 1e3,
            h2d_bytes: icols.iter().chain(&ecols).map(|c| c.len()).sum(),
            device_bytes: cache.device_bytes(),
        };
        Ok(ColsOut { internal: &cache.out_i.as_ref().expect("allocated")[..ni], external: &cache.out_e.as_ref().expect("allocated")[..ne], matched, timings: self.last_cols })
    }
}
