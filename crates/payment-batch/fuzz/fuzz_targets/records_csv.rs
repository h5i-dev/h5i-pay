//! Any bytes: the parallel records-CSV parser never panics, and what it
//! accepts it accepts line by line (the same rows as a one-line-at-a-time
//! parse of the same text).
#![no_main]
use libfuzzer_sys::fuzz_target;
use payment_batch::files;

fuzz_target!(|data: &[u8]| {
    let all = files::parse_records("fuzz", data);
    if let Ok(rows) = all {
        let mut one = Vec::new();
        for line in data.split(|b| *b == b'\n') {
            let mut l = line.to_vec();
            l.push(b'\n');
            one.extend(files::parse_records("fuzz", &l).expect("a line of an accepted file"));
        }
        assert_eq!(rows, one);
    }
});
