#!/usr/bin/env bash
# J1: the batch worker end to end on binary records files (docs/benchmarks.md,
# "The batch worker on binary input"). GPU vs the parallel CPU engine on the
# same files, interleaved, warm and cold page cache, one-shot (`reconcile`,
# a process per job) and long-lived (`jobs`, one process for K jobs).
#
#   benchmarks/j1-batch.sh <worker> <bench> <data-dir> <rows> <rounds> <out.jsonl> [K]
#
# <worker>, <bench>: release builds of h5i-pay-batch (--features cuda) and
# bench (--features gpu). <data-dir> must be on a real disk (cold runs evict
# the inputs from the page cache with `dd iflag=nocache`, per file, so other
# processes' caches are left alone). Inputs are generated once
# (`bench gen-files`) and deleted at the end; CSV copies only up to 10M rows.
set -euo pipefail
W=$1 B=$2 D=$3 N=$4 R=$5 OUT=$6 K=${7:-4}
dir="$D/$N"
mkdir -p "$dir"
csv=""
[ "$N" -le 10000000 ] && csv=--csv
[ -f "$dir/i.h5ib" ] || "$B" gen-files "$N" "$dir" $csv >&2
evict() { sync; for f in "$@"; do dd if="$f" iflag=nocache count=0 status=none; done; }
tag() { python3 -c 'import json,sys; d=json.loads(sys.argv[1]); d.update(json.loads(sys.argv[2])); print(json.dumps(d))' "$1" "$2"; }
# Long-lived cold runs need distinct files per job: K-1 extra copies of the pair.
copies=1
if [ "$N" -le 10000000 ]; then
  copies=$K
  for k in $(seq 2 "$K"); do
    [ -f "$dir/i$k.h5ib" ] || { cp "$dir/i.h5ib" "$dir/i$k.h5ib"; cp "$dir/e.h5ib" "$dir/e$k.h5ib"; }
  done
fi
pair() { if [ "$1" = 1 ]; then echo "$dir/i.h5ib $dir/e.h5ib"; else echo "$dir/i$1.h5ib $dir/e$1.h5ib"; fi; }
for r in $(seq 1 "$R"); do
  for e in gpu parallel; do
    for cache in warm cold; do
      # One-shot.
      if [ $cache = cold ]; then evict $(pair 1); else cat $(pair 1) > /dev/null; fi
      line=$("$W" reconcile $(pair 1) "$dir/report.json" --engine $e)
      tag "$line" "{\"rows\":$N,\"mode\":\"one-shot\",\"cache\":\"$cache\",\"round\":$r,\"format\":\"binary\"}" >> "$OUT"
      # Long-lived: K jobs in one process (distinct copies when there are).
      : > "$dir/jobs.txt"
      for k in $(seq 1 "$K"); do
        c=$(( (k - 1) % copies + 1 ))
        echo "$(pair $c) $dir/report-$k.json" >> "$dir/jobs.txt"
        if [ $cache = cold ]; then evict $(pair $c); else cat $(pair $c) > /dev/null; fi
      done
      k=0
      while read -r line; do
        case "$line" in *'"event":"reconciliation"'*)
          k=$((k + 1))
          tag "$line" "{\"rows\":$N,\"mode\":\"long-lived\",\"job\":$k,\"cache\":\"$cache\",\"round\":$r,\"format\":\"binary\",\"distinct_files\":$copies}" >> "$OUT";;
        esac
      done < <("$W" jobs "$dir/jobs.txt" --engine $e)
      rm -f "$dir"/report-*.json
    done
    # CSV one-shot, warm, for comparison with the CSV path.
    if [ -n "$csv" ]; then
      cat "$dir/i.csv" "$dir/e.csv" > /dev/null
      line=$("$W" reconcile "$dir/i.csv" "$dir/e.csv" "$dir/report-csv.json" --engine $e)
      tag "$line" "{\"rows\":$N,\"mode\":\"one-shot\",\"cache\":\"warm\",\"round\":$r,\"format\":\"csv\"}" >> "$OUT"
    fi
  done
done
# Same report from both formats and engines (bodies after engine/elapsed).
if [ -n "$csv" ]; then
  cmp <(sed 's/^.*"internal"://' "$dir/report.json") <(sed 's/^.*"internal"://' "$dir/report-csv.json") >&2
fi
echo "$(df -h "$D" | tail -1)" >&2
rm -rf "$dir"
