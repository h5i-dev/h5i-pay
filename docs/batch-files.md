# Batch worker files

The files `h5i-pay export` writes, `h5i-pay-batch` reads and writes, and
`h5i-pay ingest` reads. Code: `crates/payment-batch/src/files.rs` (CSV,
report) and `crates/payment-batch/src/binfile.rs` (binary records file).

## Records

One side of a reconciliation: internal records (from the payment kernel)
or external ones (the PSP's settlement report). Each record is
`key u64, amount u64, currency u32, status u32`; the key is
`kind << 56 | id` (kind 1 capture, 2 refund), `u64::MAX` is reserved.
Two encodings, told apart by the first eight bytes:

**Binary records file (`.h5ib`)**, the default since J1: a 128-byte header
and four little-endian columns, read with `mmap` and no per-row parsing.

| offset | bytes | field |
|---:|---:|---|
| 0 | 8 | magic `H5IPREC\0` |
| 8 | 4 | version, 1 |
| 12 | 4 | header length, 128 |
| 16 | 8 | rows `n` (below 2³² − 1) |
| 24 | 8 | offset of `key: [u64; n]`, 128 |
| 32 | 8 | offset of `amount: [u64; n]`, 128 + 8n |
| 40 | 8 | offset of `currency: [u32; n]`, 128 + 16n |
| 48 | 8 | offset of `status: [u32; n]`, 128 + 20n |
| 56 | 8 | file length, 128 + 24n |
| 64 | 8 | checksum of bytes 128.. |
| 72 | 48 | reserved, zero |
| 120 | 8 | checksum of bytes 0..120 |

The reader refuses a file unless every field has exactly these values (no
gaps, overlaps or misaligned columns), the file is exactly as long as the
header says, and both checksums match. The checksum hashes each 1 MiB
block with four multiply–rotate lanes and folds the block hashes in order
(`binfile::checksum`), so it is verified on every core. It detects
truncation and corruption, not tampering. 24 bytes per row: 10M rows per
side are 2 × 240 MB.

**Records CSV**: `key,amount,currency,status` per line, no header; blank
lines are skipped, fields are trimmed. The worker parses it on every
thread (the file is split at line boundaries). Errors name the line.

`h5i-pay-batch convert <in> <out>` turns one into the other.

## Reconciliation report (JSON)

```json
{"engine":"gpu","elapsed_ms":99.6,
 "internal":{"matched":970000,"amount_mismatch":10000,...},
 "external":{"matched":...,"extra_external":...},
 "exceptions":[{"side":"internal","code":"amount_mismatch","key":72057594037927937,"amount":512}, ...]}
```

Counts per code for each side, then every row that is not `matched`, the
internal side first, each in input order. The report from a binary pair
and from the same rows as CSV is the same byte for byte after `engine` and
`elapsed_ms` (`crates/payment-gpu/tests/worker.rs`). It is formatted on
every thread, in chunks, and written in row order.

## Jobs list (`h5i-pay-batch jobs`)

One job per line, `<internal> <external> <report.json>`, separated by
spaces or tabs (paths without spaces); blank lines and lines starting with
`#` are skipped; `-` reads the list from stdin. All jobs run in one
process, so the CUDA context, the PTX JIT and the device and pinned buffers
are set up once. Each job logs one JSON line; a failed job logs
`reconciliation_failed` and the others still run; the last line is
`{"event":"reconciliation_jobs","jobs":…,"failed":…}`, and the exit status
is non-zero if any job failed. The nightly run (`batch-entrypoint.sh
reconcile-run`) is one such process for all tenants of a run.

## Payout screening

Requests CSV `merchant,recipient,amount,currency,kind`; policy JSON
`{"min_amount", "max_amount": [4], "currencies": [≤ 8], "kinds_mask",
"merchants": [ascending], "blocked": [≤ 64]}`; decisions CSV
`index,code,name`.
