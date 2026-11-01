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
    rt: Nvvm,
    pub last: Timings,
}

impl Gpu {
    /// Create a CUDA context and load the kernels (JIT-compiles the PTX).
    pub fn new() -> Result<Self, Failure> {
        Ok(Gpu { rt: Nvvm::new(PTX)?, last: Timings::default() })
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
