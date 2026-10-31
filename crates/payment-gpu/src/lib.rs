//! CUDA engines for h5i-pay's batch workloads.
//!
//! [`Gpu`] implements h5i-app-gpu's `Backend` for reconciliation
//! (`Recon` → `Report`) and for batch policy checks (`PolicyBatch` →
//! codes), so the batch API can select it with `ExecutionPolicy` and the
//! differential tests can `compare` it with the CPU engines.
//!
//! The kernels (`kernels/batch.cu`) are CUDA C transcriptions of the Rust
//! specifications, compiled to PTX by nvcc and loaded through
//! `h5i-app-gpu-backend-nvvm` (Rust-CUDA's `cust`). The h5i examples compile
//! Rust itself to PTX with `rustc_codegen_nvvm`; that toolchain is pinned to
//! CUDA 12.6 and Ubuntu packages, while this host has CUDA 13.2 on Amazon
//! Linux, so h5i-pay ships hand-written kernels and relies on differential
//! tests for equivalence (`docs/proof-assumptions.md`).
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
