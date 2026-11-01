#!/bin/sh
# AWS Batch job. The worker has no database credentials and cannot move money.
#
#   reconcile <internal-key> <external-key> [engine]
#       One pair: inputs are S3 keys in $INPUT_BUCKET; the report goes to
#       $OUTPUT_BUCKET/reports/<job>.json and a copy to the Object-Lock
#       audit bucket.
#   reconcile-run <run> [engine]
#       The nightly run (infra/terraform/reconcile.tf): every tenant pair
#       `h5i-pay export` wrote under s3://$INPUT_BUCKET/runs/<run>/
#       {internal,external}/<tenant>.csv; reports to
#       s3://$OUTPUT_BUCKET/runs/<run>/reports/<tenant>.json (and the audit
#       bucket), where `h5i-pay ingest` reads them.
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
  aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${internal}" "$work/internal.csv"
  aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${external}" "$work/external.csv"
  h5i-pay-batch reconcile "$work/internal.csv" "$work/external.csv" "$work/report.json" --engine "$engine"
  aws s3 cp --only-show-errors "$work/report.json" "s3://${OUTPUT_BUCKET}/reports/${job}.json"
  aws s3 cp --only-show-errors "$work/report.json" "s3://${AUDIT_BUCKET}/reconciliation/${job}.json"
  ;;
reconcile-run)
  run=$2
  engine=${3:-auto}
  case "$run" in *[!A-Za-z0-9._-]*|'') echo "bad run id: $run" >&2; exit 2;; esac
  aws s3 cp --only-show-errors --recursive "s3://${INPUT_BUCKET}/runs/${run}/" "$work/in/"
  mkdir -p "$work/reports"
  for f in "$work"/in/internal/*.csv; do
    [ -e "$f" ] || continue
    t=$(basename "$f" .csv)
    h5i-pay-batch reconcile "$f" "$work/in/external/$t.csv" "$work/reports/$t.json" --engine "$engine"
  done
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
