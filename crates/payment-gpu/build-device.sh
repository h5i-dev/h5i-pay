#!/usr/bin/env bash
# Compile crates/payment-gpu/device to PTX with rustc_codegen_nvvm and copy it
# to kernels/payment_gpu_device.ptx (committed). `--check` fails if the
# committed PTX differs from a fresh build.
#
# Needs the pinned Rust-CUDA environment (docs/gpu-toolchain.md):
# rust-cuda 3a82c35a, nightly-2026-04-02, rustc_codegen_nvvm built with the
# prebuilt LLVM 7.1, libNVVM from CUDA 12.6. ENV defaults to
# /var/tmp/h5i-gpu-env.sh, which sets RUSTUP_HOME, CARGO_HOME,
# CARGO_TARGET_DIR, CUDA_PATH and LD_LIBRARY_PATH.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
source "${ENV:-/var/tmp/h5i-gpu-env.sh}"
# compute_75: the driver JIT-compiles it for T4 (sm_75), A10G (sm_86), L4 (sm_89).
arch="${PAYMENT_GPU_ARCH:-compute_75}"
backend="$CARGO_TARGET_DIR/release/librustc_codegen_nvvm.so"
test -f "$backend" || { echo "build rustc_codegen_nvvm first: $backend missing" >&2; exit 1; }
export RUSTFLAGS="-Zcodegen-backend=$backend -Zunstable-options -Zcrate-attr=feature(register_tool) -Zcrate-attr=register_tool(nvvm_internal) -Zcrate-attr=no_std -Zsaturating_float_casts=false -Cpanic=immediate-abort -Cllvm-args=-arch=$arch"
cd "$here/device"
cargo +nightly-2026-04-02 build --release --locked --features gpu --target nvptx64-nvidia-cuda -Zbuild-std=core,alloc -j 4
ptx="$CARGO_TARGET_DIR/nvptx64-nvidia-cuda/release/payment_gpu_device.ptx"
out="$here/kernels/payment_gpu_device.ptx"
if [[ "${1:-}" == "--check" ]]; then
  cmp -s "$ptx" "$out" || { echo "kernels/payment_gpu_device.ptx is stale: run crates/payment-gpu/build-device.sh" >&2; exit 1; }
  echo "kernels/payment_gpu_device.ptx is up to date"
else
  cp "$ptx" "$out"
  sha256sum "$out"
fi
