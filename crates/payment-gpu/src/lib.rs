//! CUDA engines for h5i-pay's batch workloads.
//!
//! [`Gpu`] implements h5i-app-gpu's `Backend` for reconciliation
//! (`Recon` → `Report`) and for batch policy checks (`PolicyBatch` →
//! codes), so the batch API can select it with `ExecutionPolicy` and the
//! differential tests can `compare` it with the CPU engines.
//!
//! The kernels are Rust: the device crate (`device/`) includes
//! `payment_domain::policy` and `payment_reconcile::rules` verbatim (the
//! source the CPU engines run and Aeneas extracts) and is compiled to PTX by
//! Rust-CUDA's `rustc_codegen_nvvm` (`build-device.sh`; toolchain in
//! `docs/gpu-toolchain.md`). The PTX is committed
//! (`kernels/payment_gpu_device.ptx`) and loaded through
//! `h5i-app-gpu-backend-nvvm` (`cust`). Only the launch shell and the
//! hash-table build are GPU-specific code; the differential tests now check
//! the compiler and the hardware, not a transcription.
//!
//! The GPU computes candidate decisions and reports. It never commits
//! financial state: callers validate the output shape, and every money
//! movement still goes through the ledger kernel.

#[cfg(feature = "cuda")]
mod cuda;
#[cfg(feature = "cuda")]
pub use cuda::{Gpu, Timings};

/// Whether this build has the CUDA engines.
pub const BUILT_WITH_CUDA: bool = cfg!(feature = "cuda");

/// A GPU engine if this build has one and a device answers; `None` means
/// "use the CPU" (h5i-app-gpu's `Auto` falls back only on absence).
#[cfg(feature = "cuda")]
pub fn try_gpu() -> Option<Gpu> {
    Gpu::new().ok()
}
