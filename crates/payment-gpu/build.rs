//! With the `cuda` feature, compile `kernels/batch.cu` to PTX with nvcc.
//! The PTX targets a virtual architecture (default `compute_75`), so the
//! driver JIT-compiles it for the GPU at hand: T4 (sm_75, AWS g4dn), A10G
//! (sm_86, g5), L4 (sm_89, g6). Override with `PAYMENT_GPU_ARCH`.

fn main() {
    println!("cargo:rerun-if-changed=kernels/batch.cu");
    println!("cargo:rerun-if-env-changed=PAYMENT_GPU_ARCH");
    println!("cargo:rerun-if-env-changed=NVCC");
    if std::env::var_os("CARGO_FEATURE_CUDA").is_none() {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("batch.ptx");
    let arch = std::env::var("PAYMENT_GPU_ARCH").unwrap_or_else(|_| "compute_75".into());
    let nvcc = std::env::var("NVCC").unwrap_or_else(|_| {
        let cuda = std::env::var("CUDA_PATH").unwrap_or_else(|_| "/usr/local/cuda".into());
        format!("{cuda}/bin/nvcc")
    });
    let status = std::process::Command::new(&nvcc)
        .args(["-ptx", "-O3", &format!("-arch={arch}"), "-o"])
        .arg(&out)
        .arg("kernels/batch.cu")
        .status()
        .unwrap_or_else(|e| panic!("running {nvcc}: {e} (the `cuda` feature needs the CUDA toolkit)"));
    assert!(status.success(), "nvcc failed on kernels/batch.cu");
}
