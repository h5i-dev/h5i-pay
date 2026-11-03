//! Any bytes: `binfile::verify` never panics; if it accepts them, the
//! length is the header's and every row can be read.
#![no_main]
use libfuzzer_sys::fuzz_target;
use payment_batch::binfile;

fuzz_target!(|data: &[u8]| {
    // An 8-aligned copy, as a mapping would be.
    let mut buf = vec![0u64; data.len().div_ceil(8)];
    let bytes = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, data.len()) };
    bytes.copy_from_slice(data);
    let _ = binfile::parse_header(bytes);
    if let Ok(h) = binfile::verify(bytes) {
        assert_eq!(bytes.len() as u64, binfile::HEADER as u64 + binfile::ROW as u64 * h.rows);
        let c = binfile::cols(bytes, h).unwrap();
        let mut sum = 0u64;
        for i in 0..c.len() {
            sum = sum.wrapping_add(c.get(i).amount);
        }
        std::hint::black_box(sum);
    }
});
