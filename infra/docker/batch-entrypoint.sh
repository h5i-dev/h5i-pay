#!/bin/sh
# AWS Batch job. The worker has no database credentials and cannot move money.
#
#   reconcile <internal-key> <external-key> [engine]
#       One pair: inputs are S3 keys in $INPUT_BUCKET (binary records files
#       or records CSV, told apart by their first bytes); the report goes to
#       $OUTPUT_BUCKET/reports/<job>.json and a copy to the Object-Lock
#       audit bucket.
#   reconcile-run <run> [engine]
#       The nightly run (infra/terraform/reconcile.tf): every tenant pair
#       `h5i-pay export` wrote under s3://$INPUT_BUCKET/runs/<run>/
#       {internal,external}/<tenant>.h5ib (binary records files; .csv pairs
#       from an export with --format csv work too), reconciled by one
#       `h5i-pay-batch jobs` process, so the CUDA context is created once per
#       run; reports to s3://$OUTPUT_BUCKET/runs/<run>/reports/<tenant>.json
#       (and the audit bucket), where `h5i-pay ingest` reads them.
#   policy <requests-key> <policy-key> [engine]
#       Bulk payout screening: decisions to $OUTPUT_BUCKET/payouts/<job>.csv.
set -eu
job=${AWS_BATCH_JOB_ID:-local-$(date +%s)}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case "${1:-}" in
reconcile)
  internal=$2
  external=$3
  engine=${4:-auto}
  aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${internal}" "$work/internal"
  aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${external}" "$work/external"
  h5i-pay-batch reconcile "$work/internal" "$work/external" "$work/report.json" --engine "$engine"
  aws s3 cp --only-show-errors "$work/report.json" "s3://${OUTPUT_BUCKET}/reports/${job}.json"
  aws s3 cp --only-show-errors "$work/report.json" "s3://${AUDIT_BUCKET}/reconciliation/${job}.json"
  ;;
reconcile-run)
  run=$2
  engine=${3:-auto}
  case "$run" in *[!A-Za-z0-9._-]*|'') echo "bad run id: $run" >&2; exit 2;; esac
  aws s3 cp --only-show-errors --recursive "s3://${INPUT_BUCKET}/runs/${run}/" "$work/in/"
  mkdir -p "$work/reports"
  : > "$work/jobs.txt"
  for f in "$work"/in/internal/*.h5ib "$work"/in/internal/*.csv; do
    [ -e "$f" ] || continue
    name=$(basename "$f")
    t=${name%.*}
    case "$t" in *[!0-9]*|'') echo "unexpected file $f" >&2; exit 2;; esac
    echo "$f $work/in/external/$name $work/reports/$t.json" >> "$work/jobs.txt"
  done
  h5i-pay-batch jobs "$work/jobs.txt" --engine "$engine"
  aws s3 cp --only-show-errors --recursive "$work/reports/" "s3://${OUTPUT_BUCKET}/runs/${run}/reports/"
  aws s3 cp --only-show-errors --recursive "$work/reports/" "s3://${AUDIT_BUCKET}/reconciliation/${run}/"
  ;;
policy)
  requests=$2
  policy=$3
  engine=${4:-auto}
  aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${requests}" "$work/requests.csv"
  aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${policy}" "$work/policy.json"
  h5i-pay-batch policy "$work/requests.csv" "$work/policy.json" "$work/decisions.csv" --engine "$engine"
  aws s3 cp --only-show-errors "$work/decisions.csv" "s3://${OUTPUT_BUCKET}/payouts/${job}.csv"
  ;;
*)
  echo "usage: reconcile <internal-key> <external-key> [engine] | reconcile-run <run> [engine] | policy <requests-key> <policy-key> [engine]" >&2
  exit 2
  ;;
esac
