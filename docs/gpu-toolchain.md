# GPU toolchain: one Rust source for CPU, proofs and GPU

The GPU engines run Rust compiled to PTX by Rust-CUDA's `rustc_codegen_nvvm`,
the way h5i-app's examples do (`examples/app/discount-gpu`,
`examples/app/parallel` in h5i). The device crate
`crates/payment-gpu/device` includes, verbatim with `#[path]`:

- `crates/payment-domain/src/policy.rs` (`policy::check`, extracted by
  Aeneas, `Policies.check_sound`);
- `crates/payment-reconcile/src/rules.rs` (`classify_internal`,
  `classify_external`, extracted and proven in `proofs/reconcile`; and
  `find_slot` / `internal_row` / `external_row`, the shared probe the CPU
  engines also call).

Its own code is the launch shell (thread index, bounds checks, raw-pointer
ABI) and `recon_build`, the hash-table build with device atomics. The PTX
is committed as `crates/payment-gpu/kernels/payment_gpu_device.ptx` and
embedded by `payment-gpu` (`include_str!`); the host side no longer needs
nvcc. The CUDA C kernels (`kernels/batch.cu`) are deleted.

## Pins

| | |
|---|---|
| Rust-CUDA | `3a82c35a7196731fb60eebdac2a40d2b1bf0ba95` (the rev h5i-app-gpu-backend-nvvm and `cust` pin) |
| Rust | `nightly-2026-04-02` (rustc 1.96.0-nightly 7e46c5f6f) with `rust-src`, `rustc-dev`, `llvm-tools-preview` |
| LLVM | prebuilt 7.1.0 (`USE_PREBUILT_LLVM=1`, downloaded by `rustc_codegen_nvvm`'s build script from `rust-gpu/rustc_codegen_nvvm-llvm`, release `llvm-7.1.0`) |
| libNVVM | CUDA 12.6 (`cuda-nvvm-12-6` 12.6.85, `cuda-nvcc-12-6`, `cuda-cudart-devel-12-6`), installed next to CUDA 13.2 |
| Device lockfile | `crates/payment-gpu/device/Cargo.lock`, started from h5i's `examples/app/discount-gpu/Cargo.lock` (glam 0.30.10: with 0.34 `cuda_std` does not build) |
| Target | `nvptx64-nvidia-cuda`, `-Cllvm-args=-arch=compute_75` (PTX `.target sm_75`, ISA 8.5; the driver JIT-compiles it for T4, A10G, L4) |

## What this host (Amazon Linux 2023) needed

h5i's `backend-nvvm/setup.sh` assumes Ubuntu 24.04. Here, natively:

1. `dnf install cuda-nvvm-12-6 cuda-nvcc-12-6 cuda-cudart-devel-12-6 ncurses-devel`
   (from NVIDIA's `cuda-rhel9` repo, already configured). The packages
   repoint `/usr/local/cuda` (alternatives) to 12.6;
   `alternatives --install /usr/local/cuda cuda /usr/local/cuda-13.2 132`
   puts it back on 13.2, which everything else uses. 12.6 is only on
   `CUDA_PATH`/`LD_LIBRARY_PATH` for the device build.
2. The nightly toolchain under `RUSTUP_HOME=/var/tmp/h5i-gpu-rustup`
   (nothing under `$HOME`), rust-cuda cloned to `/var/tmp/h5i-gpu-rust-cuda`.
3. The prebuilt LLVM 7.1's `llvm-as` and `libLLVM-7.1.so` link Ubuntu's
   `libedit.so.2` (and through it `libbsd.so.0`); AL2023 has
   `libedit.so.0` only. The 24.04 packages need glibc 2.38 (AL2023 has
   2.34), so the Ubuntu **20.04** packages are extracted (not installed) to
   `/var/tmp/h5i-gpu-system-libs` and put on `LD_LIBRARY_PATH` /
   `LIBRARY_PATH`:
   - `libedit2_3.1-20191231-1_amd64.deb`, sha256 `51a1190157e2dfe2c26bbdc114d1fc659456def2e78e6e9582809cf92a0a49a4`
   - `libbsd0_0.10.0-1_amd64.deb`, sha256 `4f668025fe923a372eb7fc368d6769fcfff6809233d48fd20fc072917cd82e60`

   `libtinfo.so.6` and `libxml2.so.2` come from AL2023 (a harmless "no
   version information" warning for libtinfo).
4. `USE_PREBUILT_LLVM=1 cargo +nightly-2026-04-02 build --release --locked -p rustc_codegen_nvvm`
   in the rust-cuda checkout (25 s after the first build; the first, with
   the LLVM download, a few minutes).

The environment is `/var/tmp/h5i-gpu-env.sh`:

```sh
export RUSTUP_HOME=/var/tmp/h5i-gpu-rustup CARGO_HOME=/var/tmp/h5i-gpu-cargo-home CARGO_TARGET_DIR=/var/tmp/h5i-gpu-target TMPDIR=/var/tmp
export CUDA_PATH=/usr/local/cuda-12.6 CUDA_VISIBLE_DEVICES=0
export PATH=/root/.cargo/bin:/usr/local/cuda-12.6/bin:$PATH
export LD_LIBRARY_PATH=/usr/local/cuda-12.6/nvvm/lib64:/usr/local/cuda-12.6/lib64:$RUSTUP_HOME/toolchains/nightly-2026-04-02-x86_64-unknown-linux-gnu/lib:/var/tmp/h5i-gpu-system-libs/usr/lib/x86_64-linux-gnu
export LIBRARY_PATH=/var/tmp/h5i-gpu-system-libs/usr/lib/x86_64-linux-gnu
```

No container was needed. On Ubuntu 24.04 (CI's `device-ptx` job) the
Ubuntu packages from h5i's `setup.sh` are used instead of step 3.

## Building and checking the PTX

```sh
crates/payment-gpu/build-device.sh           # build, copy to kernels/, print sha256
crates/payment-gpu/build-device.sh --check   # rebuild and fail if the committed PTX differs
cargo test --release -p h5i-pay-tests --features gpu --test differential
```

The build is deterministic here: two builds gave byte-identical PTX.

## Trusted for the GPU path

Rust-CUDA's `rustc_codegen_nvvm` and `cuda_std`, the nightly rustc front
end, LLVM 7.1, libNVVM 12.6, the driver's PTX JIT, the CUDA driver and
hardware, `cust`, and the launch shell and `recon_build` in the device
crate. The decisions themselves are the same source the CPU runs and the
proofs are about; the differential tests now check the compiler and the
hardware against the CPU engines, not a hand transcription.
