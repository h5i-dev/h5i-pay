//! The batch worker's binary records file (`.h5ib`): one side of a
//! reconciliation as four little-endian columns, read with `mmap` and no
//! per-row parsing. `h5i-pay export` writes it, `h5i-pay-batch convert`
//! turns a records CSV into it (and back), and `h5i-pay-batch reconcile`
//! reads it or CSV (told apart by the magic bytes).
//!
//! Layout, version 1 (all integers little-endian):
//!
//! | offset | bytes | field |
//! |---:|---:|---|
//! | 0 | 8 | magic `H5IPREC\0` |
//! | 8 | 4 | version, 1 |
//! | 12 | 4 | header length, 128 |
//! | 16 | 8 | rows `n` |
//! | 24 | 8 | offset of `key: [u64; n]`, 128 |
//! | 32 | 8 | offset of `amount: [u64; n]`, 128 + 8n |
//! | 40 | 8 | offset of `currency: [u32; n]`, 128 + 16n |
//! | 48 | 8 | offset of `status: [u32; n]`, 128 + 20n |
//! | 56 | 8 | file length, 128 + 24n |
//! | 64 | 8 | checksum of bytes 128.. ([`checksum`]) |
//! | 72 | 48 | reserved, zero |
//! | 120 | 8 | checksum of bytes 0..120 |
//!
//! The file is untrusted input. [`Mapped::open`] checks every header
//! field (the offsets must be exactly the ones above: no gaps, overlaps or
//! misaligned columns), that the file is exactly as long as the header
//! says, and that `n` is below the engines' `u32` row limit;
//! [`Mapped::verify`] checks the data checksum. Only then are the columns
//! handed out as slices of the mapping ([`Records::cols`]); the engines
//! still reject the reserved key (`payment_reconcile::validate_cols`).
//!
//! The checksum detects truncation and corruption in transit or at rest; it
//! is not a MAC (anyone who can write the file can recompute it). It is
//! computed per 1 MiB block, so verification runs on every core.
//!
//! The worker maps a private copy of its input (downloaded to its own
//! temporary directory). Like any `mmap` reader, it must not be given a
//! file another process truncates while it runs (that would be a SIGBUS,
//! not wrong output).

use payment_reconcile::{Cols, Rows};
use std::io::{Seek, SeekFrom, Write};

pub const MAGIC: [u8; 8] = *b"H5IPREC\0";
pub const VERSION: u32 = 1;
pub const HEADER: usize = 128;
/// Bytes per row: key 8, amount 8, currency 4, status 4.
pub const ROW: usize = 24;
/// The engines index rows with `u32` (and reserve `u32::MAX`).
pub const MAX_ROWS: u64 = u32::MAX as u64 - 1;
const BLOCK: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// Shorter than the header.
    TooShort(usize),
    BadMagic,
    Version(u32),
    HeaderLength(u32),
    ReservedNotZero,
    HeaderChecksum,
    TooManyRows(u64),
    /// A column offset is not the canonical one (gap, overlap, misaligned).
    Layout { column: &'static str, offset: u64, expected: u64 },
    /// The header's file length, or the actual one, is not `128 + 24n`.
    Length { expected: u64, header: u64, actual: u64 },
    DataChecksum,
    /// The bytes do not start at an 8-byte-aligned address (only for
    /// in-memory buffers; a mapping is page-aligned).
    Misaligned,
    /// This reader assumes a little-endian host.
    BigEndianHost,
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bad records file: {self:?}")
    }
}

impl std::error::Error for FormatError {}

/// murmur3's 64-bit finalizer.
#[inline(always)]
fn fmix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

const M: u64 = 0x9e37_79b9_7f4a_7c15;

/// The hash of block `index` (at most [`BLOCK`] bytes): four independent
/// multiply–rotate lanes over 32-byte groups, the tail zero-padded.
fn block_hash(index: u64, b: &[u8]) -> u64 {
    let mut l = [index ^ 0x243f_6a88_85a3_08d3, index ^ 0x1319_8a2e_0370_7344, index ^ 0xa409_3822_299f_31d0, index ^ 0x082e_fa98_ec4e_6c89];
    let mut step = |g: &[u8]| {
        for (k, lane) in l.iter_mut().enumerate() {
            let w = u64::from_le_bytes(g[8 * k..8 * k + 8].try_into().unwrap());
            *lane = (*lane ^ w).wrapping_mul(M).rotate_left(31);
        }
    };
    let (groups, tail) = b.as_chunks::<32>();
    for g in groups {
        step(g);
    }
    if !tail.is_empty() {
        let mut pad = [0u8; 32];
        pad[..tail.len()].copy_from_slice(tail);
        step(&pad);
    }
    fmix(l[0] ^ l[1].rotate_left(16) ^ l[2].rotate_left(32) ^ l[3].rotate_left(48) ^ b.len() as u64)
}

fn combine(acc: u64, bh: u64) -> u64 {
    fmix(acc ^ bh).wrapping_add(M)
}

/// The checksum of `data`: its 1 MiB blocks hashed independently (on every
/// thread), then folded in order with the total length.
pub fn checksum(data: &[u8]) -> u64 {
    use rayon::prelude::*;
    let blocks: Vec<u64> = data.par_chunks(BLOCK).enumerate().map(|(i, b)| block_hash(i as u64, b)).collect();
    fmix(blocks.into_iter().fold(0, combine) ^ data.len() as u64)
}

/// [`checksum`] over a stream of bytes, written to `out` in 1 MiB blocks.
struct Sink<W: Write> {
    out: W,
    buf: Vec<u8>,
    blocks: u64,
    acc: u64,
    len: u64,
}

impl<W: Write> Sink<W> {
    fn new(out: W) -> Self {
        Sink { out, buf: Vec::with_capacity(BLOCK), blocks: 0, acc: 0, len: 0 }
    }

    fn flush_block(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            self.acc = combine(self.acc, block_hash(self.blocks, &self.buf));
            self.blocks += 1;
            self.len += self.buf.len() as u64;
            self.out.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    #[inline(always)]
    fn put(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.buf.len() + bytes.len() > BLOCK {
            let room = BLOCK - self.buf.len();
            self.buf.extend_from_slice(&bytes[..room]);
            self.flush_block()?;
            return self.put(&bytes[room..]);
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<(W, u64)> {
        self.flush_block()?;
        Ok((self.out, fmix(self.acc ^ self.len)))
    }
}

fn offsets(n: u64) -> [u64; 5] {
    let h = HEADER as u64;
    [h, h + 8 * n, h + 16 * n, h + 20 * n, h + 24 * n]
}

fn header(n: u64, data_sum: u64) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[0..8].copy_from_slice(&MAGIC);
    h[8..12].copy_from_slice(&VERSION.to_le_bytes());
    h[12..16].copy_from_slice(&(HEADER as u32).to_le_bytes());
    h[16..24].copy_from_slice(&n.to_le_bytes());
    for (k, o) in offsets(n).iter().enumerate() {
        h[24 + 8 * k..32 + 8 * k].copy_from_slice(&o.to_le_bytes());
    }
    h[64..72].copy_from_slice(&data_sum.to_le_bytes());
    let hs = checksum(&h[..120]);
    h[120..128].copy_from_slice(&hs.to_le_bytes());
    h
}

/// Write `rows` as a records file (column by column, one pass per column).
pub fn write<R: Rows + ?Sized>(path: &std::path::Path, rows: &R) -> std::io::Result<()> {
    let n = rows.len();
    if n as u64 > MAX_ROWS {
        return Err(std::io::Error::other(FormatError::TooManyRows(n as u64)));
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(&[0u8; HEADER])?;
    let mut s = Sink::new(f);
    for i in 0..n {
        s.put(&rows.key(i).to_le_bytes())?;
    }
    for i in 0..n {
        s.put(&rows.get(i).amount.to_le_bytes())?;
    }
    for i in 0..n {
        s.put(&rows.get(i).currency.to_le_bytes())?;
    }
    for i in 0..n {
        s.put(&rows.get(i).status.to_le_bytes())?;
    }
    let (mut f, sum) = s.finish()?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&header(n as u64, sum))?;
    f.sync_data()?;
    Ok(())
}

/// Whether `path` starts with the records-file magic (otherwise: CSV).
pub fn is_records_file(path: &str) -> std::io::Result<bool> {
    use std::io::Read;
    let mut m = [0u8; 8];
    let mut f = std::fs::File::open(path)?;
    let mut got = 0;
    while got < 8 {
        match f.read(&mut m[got..])? {
            0 => return Ok(false),
            k => got += k,
        }
    }
    Ok(m == MAGIC)
}

/// A checked header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub rows: u64,
    pub data_checksum: u64,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Check the header of `bytes` (the whole file) and its length; not the data.
pub fn parse_header(bytes: &[u8]) -> Result<Header, FormatError> {
    if cfg!(target_endian = "big") {
        return Err(FormatError::BigEndianHost);
    }
    if bytes.len() < HEADER {
        return Err(FormatError::TooShort(bytes.len()));
    }
    let h = &bytes[..HEADER];
    if h[0..8] != MAGIC {
        return Err(FormatError::BadMagic);
    }
    if u32_at(h, 8) != VERSION {
        return Err(FormatError::Version(u32_at(h, 8)));
    }
    if u32_at(h, 12) != HEADER as u32 {
        return Err(FormatError::HeaderLength(u32_at(h, 12)));
    }
    if checksum(&h[..120]) != u64_at(h, 120) {
        return Err(FormatError::HeaderChecksum);
    }
    if h[72..120].iter().any(|b| *b != 0) {
        return Err(FormatError::ReservedNotZero);
    }
    let n = u64_at(h, 16);
    if n > MAX_ROWS {
        return Err(FormatError::TooManyRows(n));
    }
    let want = offsets(n);
    for (k, column) in ["key", "amount", "currency", "status"].into_iter().enumerate() {
        let offset = u64_at(h, 24 + 8 * k);
        if offset != want[k] {
            return Err(FormatError::Layout { column, offset, expected: want[k] });
        }
    }
    let header_len = u64_at(h, 56);
    if header_len != want[4] || bytes.len() as u64 != want[4] {
        return Err(FormatError::Length { expected: want[4], header: header_len, actual: bytes.len() as u64 });
    }
    Ok(Header { rows: n, data_checksum: u64_at(h, 64) })
}

/// Check the header and the data checksum of `bytes`.
pub fn verify(bytes: &[u8]) -> Result<Header, FormatError> {
    let h = parse_header(bytes)?;
    if checksum(&bytes[HEADER..]) != h.data_checksum {
        return Err(FormatError::DataChecksum);
    }
    Ok(h)
}

/// The columns of a verified file. `bytes` must start 8-byte aligned.
pub fn cols(bytes: &[u8], h: Header) -> Result<Cols<'_>, FormatError> {
    if !(bytes.as_ptr() as usize).is_multiple_of(8) {
        return Err(FormatError::Misaligned);
    }
    let n = h.rows as usize;
    let o = offsets(h.rows).map(|x| x as usize);
    assert_eq!(bytes.len(), o[4]);
    // SAFETY: `parse_header` checked the length, so every range is inside
    // `bytes`; the base is 8-aligned and the offsets are multiples of 8
    // (u64 columns) and 4 (u32 columns); any bit pattern is a valid integer;
    // the host is little-endian; the slices borrow `bytes`.
    unsafe {
        let p = bytes.as_ptr();
        Ok(Cols {
            key: std::slice::from_raw_parts(p.add(o[0]) as *const u64, n),
            amount: std::slice::from_raw_parts(p.add(o[1]) as *const u64, n),
            currency: std::slice::from_raw_parts(p.add(o[2]) as *const u32, n),
            status: std::slice::from_raw_parts(p.add(o[3]) as *const u32, n),
        })
    }
}

/// A records file mapped into memory, its header checked.
pub struct Mapped {
    map: memmap2::Mmap,
    header: Header,
}

impl Mapped {
    /// Map `path` read-only and check its header and length.
    pub fn open(path: &str) -> Result<Mapped, Box<dyn std::error::Error + Send + Sync>> {
        let f = std::fs::File::open(path)?;
        // SAFETY: a read-only private mapping; see the module comment on
        // files truncated underneath the reader.
        let map = unsafe { memmap2::MmapOptions::new().map(&f)? };
        let header = parse_header(&map).map_err(|e| format!("{path}: {e}"))?;
        Ok(Mapped { map, header })
    }

    /// Check the data checksum (reads every page once, on every thread).
    pub fn verify(self, path: &str) -> Result<Records, Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(unix)]
        let _ = self.map.advise(memmap2::Advice::Sequential);
        if checksum(&self.map[HEADER..]) != self.header.data_checksum {
            return Err(format!("{path}: {}", FormatError::DataChecksum).into());
        }
        Ok(Records { map: self.map, header: self.header })
    }

    pub fn rows(&self) -> u64 {
        self.header.rows
    }
}

/// A verified records file.
pub struct Records {
    map: memmap2::Mmap,
    header: Header,
}

impl Records {
    pub fn cols(&self) -> Cols<'_> {
        // A mapping is page-aligned, so this cannot fail.
        cols(&self.map, self.header).expect("mmap is page-aligned")
    }

    pub fn rows(&self) -> usize {
        self.header.rows as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use payment_reconcile::Record;

    /// An 8-aligned copy of `b`.
    fn aligned(b: &[u8]) -> (Vec<u64>, usize) {
        let mut v = vec![0u64; b.len().div_ceil(8)];
        // SAFETY: the vector has at least b.len() bytes.
        unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), v.as_mut_ptr() as *mut u8, b.len()) };
        (v, b.len())
    }

    fn bytes_of(v: &(Vec<u64>, usize)) -> &[u8] {
        // SAFETY: v.1 <= 8 * v.0.len().
        unsafe { std::slice::from_raw_parts(v.0.as_ptr() as *const u8, v.1) }
    }

    fn file(rows: &[Record]) -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!("h5ib-{}-{}", std::process::id(), rows.len()));
        write(&dir, rows).unwrap();
        let b = std::fs::read(&dir).unwrap();
        std::fs::remove_file(&dir).unwrap();
        b
    }

    fn sample(n: usize) -> Vec<Record> {
        crate::synth::recon(n, 7).internal
    }

    #[test]
    fn round_trip() {
        for n in [0usize, 1, 3, 1000, 50_000] {
            let rows = sample(n);
            let b = file(&rows);
            assert_eq!(b.len(), HEADER + ROW * rows.len());
            let a = aligned(&b);
            let h = verify(bytes_of(&a)).unwrap();
            let c = cols(bytes_of(&a), h).unwrap();
            assert_eq!((0..c.len()).map(|i| c.get(i)).collect::<Vec<_>>(), rows);
        }
    }

    #[test]
    fn mapped_file_round_trip() {
        let rows = sample(70_000);
        let p = std::env::temp_dir().join(format!("h5ib-map-{}", std::process::id()));
        write(&p, &rows[..]).unwrap();
        let s = p.to_str().unwrap();
        assert!(is_records_file(s).unwrap());
        let r = Mapped::open(s).unwrap().verify(s).unwrap();
        let c = r.cols();
        assert_eq!((0..c.len()).map(|i| c.get(i)).collect::<Vec<_>>(), rows);
        std::fs::remove_file(&p).unwrap();
    }

    /// The streaming checksum of the writer equals the parallel one.
    #[test]
    fn checksum_streams() {
        let data: Vec<u8> = (0..(3 * BLOCK + 12_345)).map(|i| (i * 31 % 251) as u8).collect();
        for cut in [0usize, 1, 7, BLOCK - 1, BLOCK, BLOCK + 3, data.len()] {
            let mut s = Sink::new(Vec::new());
            s.put(&data[..cut]).unwrap();
            for b in data[cut..].chunks(4097) {
                s.put(b).unwrap();
            }
            let (out, sum) = s.finish().unwrap();
            assert_eq!(out, data);
            assert_eq!(sum, checksum(&data), "cut {cut}");
        }
        assert_ne!(checksum(&[0u8; 8]), checksum(&[0u8; 16]));
    }

    fn rejects(b: &[u8]) -> FormatError {
        let a = aligned(b);
        verify(bytes_of(&a)).unwrap_err()
    }

    fn set_u64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Recompute the header checksum, so the test reaches the later checks.
    fn reseal(b: &mut [u8]) {
        let hs = checksum(&b[..120]);
        set_u64(b, 120, hs);
    }

    #[test]
    fn rejects_bad_files() {
        let good = file(&sample(1000));
        // synth adds ~0.1% duplicate rows.
        let rows = u64_at(&good, 16);
        assert!((1000..1010).contains(&rows), "{rows}");
        // Truncated anywhere: in the header, mid-column, one byte short.
        for len in [0, 7, 8, 64, 127, 128, 129, good.len() / 2, good.len() - 1] {
            let e = rejects(&good[..len]);
            assert!(matches!(e, FormatError::TooShort(_) | FormatError::Length { .. }), "{len}: {e:?}");
        }
        // Oversized: trailing bytes.
        let mut long = good.clone();
        long.extend_from_slice(&[0; 8]);
        assert!(matches!(rejects(&long), FormatError::Length { .. }));
        // Header fields.
        let mut b = good.clone();
        b[0] = b'X';
        assert_eq!(rejects(&b), FormatError::BadMagic);
        let mut b = good.clone();
        b[8] = 2;
        assert_eq!(rejects(&b), FormatError::Version(2));
        let mut b = good.clone();
        b[12] = 64;
        assert_eq!(rejects(&b), FormatError::HeaderLength(64));
        let mut b = good.clone();
        b[100] = 1;
        assert_eq!(rejects(&b), FormatError::HeaderChecksum);
        reseal(&mut b);
        assert_eq!(rejects(&b), FormatError::ReservedNotZero);
        // A row count that does not match the length, or overflows it.
        for bad in [rows + 1, rows - 1, MAX_ROWS, u64::MAX / 24 + 1, u64::MAX] {
            let mut b = good.clone();
            set_u64(&mut b, 16, bad);
            assert_eq!(rejects(&b), FormatError::HeaderChecksum);
            reseal(&mut b);
            let e = rejects(&b);
            assert!(matches!(e, FormatError::Layout { .. } | FormatError::TooManyRows(_)), "{bad}: {e:?}");
        }
        // Misaligned, overlapping or gapped columns.
        for (k, delta) in [(0usize, 1i64), (1, 4), (1, -8), (2, 8), (3, 2), (3, -4)] {
            let mut b = good.clone();
            let o = u64_at(&b, 24 + 8 * k);
            set_u64(&mut b, 24 + 8 * k, (o as i64 + delta) as u64);
            reseal(&mut b);
            assert!(matches!(rejects(&b), FormatError::Layout { .. }), "{k} {delta}");
        }
        // A stated file length that disagrees.
        let mut b = good.clone();
        set_u64(&mut b, 56, good.len() as u64 + 24);
        reseal(&mut b);
        assert!(matches!(rejects(&b), FormatError::Length { .. }));
        // Any flipped data byte.
        for at in [HEADER, HEADER + 8 * rows as usize + 3, good.len() - 1] {
            let mut b = good.clone();
            b[at] ^= 0x10;
            assert_eq!(rejects(&b), FormatError::DataChecksum, "{at}");
        }
        // A good file at an unaligned address.
        let a = aligned(&[&[0u8][..], &good].concat());
        let b = &bytes_of(&a)[1..];
        let h = verify(b).unwrap();
        assert_eq!(cols(b, h).unwrap_err(), FormatError::Misaligned);
        // Not a records file.
        assert_eq!(rejects("1,2,392,0\n".repeat(20).as_bytes()), FormatError::BadMagic);
    }

    /// Random corruptions never panic, and never verify unless they are
    /// the original bytes (a deterministic stand-in for `fuzz/`).
    #[test]
    fn random_corruptions_are_rejected() {
        let good = file(&sample(300));
        let mut rng = crate::synth::Rng(0x5eed);
        for _ in 0..20_000 {
            let mut b = good.clone();
            match rng.below(4) {
                0 => {
                    let at = rng.below(b.len() as u64) as usize;
                    b[at] ^= 1 << rng.below(8);
                }
                1 => b.truncate(rng.below(b.len() as u64) as usize),
                2 => {
                    let at = rng.below(HEADER as u64) as usize;
                    b[at] = rng.next_u64() as u8;
                    reseal(&mut b);
                }
                _ => {
                    let extra = 1 + rng.below(64) as usize;
                    b.extend(std::iter::repeat_n(0u8, extra));
                }
            }
            if b != good {
                let a = aligned(&b);
                assert!(verify(bytes_of(&a)).is_err());
            }
        }
    }
}
