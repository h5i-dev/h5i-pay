// h5i-pay GPU kernels: reconciliation (hash join) and batch policy checks.
//
// These are CUDA transcriptions of the Rust specifications
//   crates/payment-reconcile/src/rules.rs   (classify_internal / classify_external, hash)
//   crates/payment-domain/src/policy.rs     (check, contains_sorted, contains_linear)
// and of the hash join in crates/payment-reconcile/src/lib.rs. The Rust is
// what the Lean proofs and the CPU engines use; equality with these kernels
// is established by differential tests (tests/differential), not by proof.
//
// Every kernel computes independent rows from read-only inputs into
// disjoint output slots; nothing here commits any state.

typedef unsigned long long u64;
typedef unsigned int u32;

#define EMPTY 0xFFFFFFFFFFFFFFFFull
#define NONE 0xFFFFFFFFu

struct Record {
    u64 key;
    u64 amount;
    u32 currency;
    u32 status;
};

// Codes (payment_reconcile::rules).
#define MATCHED 0u
#define AMOUNT_MISMATCH 1u
#define CURRENCY_MISMATCH 2u
#define STATUS_MISMATCH 3u
#define MISSING_EXTERNAL 4u
#define DUPLICATE_EXTERNAL 5u
#define DUPLICATE_INTERNAL 6u
#define EXTRA_EXTERNAL 7u

__device__ __forceinline__ u64 hash64(u64 k) {
    u64 lo = k & 0xffffffffull;
    u64 hi = k >> 32;
    u64 a = (lo * 0x9e3779b1ull) ^ (hi * 0x85ebca77ull);
    return a ^ (a >> 29) ^ (k >> 17);
}

// Insert every row's key; count rows per key and keep the lowest row index.
extern "C" __global__ void recon_build(const Record* rows, u32 n, u64* keys, u32* count, u32* first, u64 mask) {
    u32 i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    u64 k = rows[i].key;
    u64 s = hash64(k) & mask;
    while (true) {
        u64 prev = atomicCAS(&keys[s], EMPTY, k);
        if (prev == EMPTY || prev == k) {
            atomicAdd(&count[s], 1u);
            atomicMin(&first[s], i);
            return;
        }
        s = (s + 1) & mask;
    }
}

__device__ __forceinline__ long long find_slot(const u64* keys, u64 mask, u64 k) {
    u64 s = hash64(k) & mask;
    while (true) {
        u64 x = keys[s];
        if (x == k) return (long long)s;
        if (x == EMPTY) return -1;
        s = (s + 1) & mask;
    }
}

extern "C" __global__ void recon_internal(const Record* internal, u32 n, const Record* external,
                                          const u64* ikeys, const u32* icount, u64 imask,
                                          const u64* ekeys, const u32* ecount, const u32* efirst, u64 emask,
                                          u32* codes, u32* matched) {
    u32 i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    Record x = internal[i];
    long long si = find_slot(ikeys, imask, x.key);
    long long se = find_slot(ekeys, emask, x.key);
    u32 ic = si < 0 ? 0u : icount[si];
    u32 ec = se < 0 ? 0u : ecount[se];
    u32 ef = se < 0 ? NONE : efirst[se];
    u32 code;
    if (ic > 1) {
        code = DUPLICATE_INTERNAL;
    } else if (ec == 0) {
        code = MISSING_EXTERNAL;
    } else if (ec > 1) {
        code = DUPLICATE_EXTERNAL;
    } else {
        Record e = external[ef];
        if (e.currency != x.currency) code = CURRENCY_MISMATCH;
        else if (e.amount != x.amount) code = AMOUNT_MISMATCH;
        else if (e.status != x.status) code = STATUS_MISMATCH;
        else code = MATCHED;
    }
    codes[i] = code;
    matched[i] = ef;
}

extern "C" __global__ void recon_external(const Record* external, u32 n,
                                          const u64* ikeys, const u32* icount, u64 imask,
                                          const u64* ekeys, const u32* ecount, u64 emask,
                                          u32* codes) {
    u32 j = blockIdx.x * blockDim.x + threadIdx.x;
    if (j >= n) return;
    u64 k = external[j].key;
    long long si = find_slot(ikeys, imask, k);
    long long se = find_slot(ekeys, emask, k);
    u32 ic = si < 0 ? 0u : icount[si];
    u32 ec = se < 0 ? 0u : ecount[se];
    u32 code;
    if (ic == 0) code = EXTRA_EXTERNAL;
    else if (ic > 1) code = DUPLICATE_INTERNAL;
    else if (ec > 1) code = DUPLICATE_EXTERNAL;
    else code = MATCHED;
    codes[j] = code;
}

// payment_domain::policy
struct Limits {
    u64 min_amount;
    u64 max_amount[4];
    u32 currencies[8];
    u32 n_currencies;
    u32 kinds_mask;
};

struct PaymentReq {
    u64 merchant;
    u64 recipient;
    u64 amount;
    u32 currency;
    u32 kind;
};

#define APPROVED 0u
#define ZERO_AMOUNT 1u
#define BELOW_MINIMUM 2u
#define UNKNOWN_KIND 3u
#define KIND_NOT_ALLOWED 4u
#define ABOVE_LIMIT 5u
#define CURRENCY_NOT_ALLOWED 6u
#define MERCHANT_NOT_ALLOWED 7u
#define SELF_PAYMENT 8u
#define RECIPIENT_BLOCKED 9u
#define BAD_POLICY 10u

__device__ __forceinline__ bool contains_sorted(const u64* sorted, u64 len, u64 x) {
    u64 lo = 0, hi = len;
    while (lo < hi) {
        u64 mid = lo + (hi - lo) / 2;
        u64 v = sorted[mid];
        if (v == x) return true;
        if (v < x) lo = mid + 1; else hi = mid;
    }
    return false;
}

extern "C" __global__ void policy_check(const PaymentReq* reqs, u32 n, const Limits* lp,
                                        const u64* merchants, u64 n_merchants,
                                        const u64* blocked, u64 blocked_len, u64 n_blocked,
                                        u32* out) {
    u32 i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    const Limits l = *lp;
    PaymentReq r = reqs[i];
    u32 code;
    if (l.n_currencies > 8u || n_blocked > 64ull || n_blocked > blocked_len) {
        code = BAD_POLICY;
    } else if (r.amount == 0) {
        code = ZERO_AMOUNT;
    } else if (r.amount < l.min_amount) {
        code = BELOW_MINIMUM;
    } else if (r.kind >= 4u) {
        code = UNKNOWN_KIND;
    } else if (((l.kinds_mask >> r.kind) & 1u) == 0u) {
        code = KIND_NOT_ALLOWED;
    } else if (r.amount > l.max_amount[r.kind]) {
        code = ABOVE_LIMIT;
    } else {
        bool cur = false;
        for (u32 c = 0; c < l.n_currencies && c < 8u; c++) {
            if (l.currencies[c] == r.currency) { cur = true; break; }
        }
        if (!cur) {
            code = CURRENCY_NOT_ALLOWED;
        } else if (!contains_sorted(merchants, n_merchants, r.merchant)) {
            code = MERCHANT_NOT_ALLOWED;
        } else if (r.recipient == r.merchant) {
            code = SELF_PAYMENT;
        } else {
            bool b = false;
            for (u64 j = 0; j < n_blocked && j < blocked_len; j++) {
                if (blocked[j] == r.recipient) { b = true; break; }
            }
            code = b ? RECIPIENT_BLOCKED : APPROVED;
        }
    }
    out[i] = code;
}
