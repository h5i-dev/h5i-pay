#!/bin/sh
# The nightly reconciliation's database steps (Step Functions,
# infra/terraform/reconcile.tf), in the ops image: the production h5i-pay
# binary plus the AWS CLI. Same database credentials and TLS as the API.
#
#   export <run>   h5i-pay export, then upload to s3://$INPUT_BUCKET/runs/<run>/
#   ingest <run>   download s3://$OUTPUT_BUCKET/runs/<run>/reports/, h5i-pay ingest
set -eu
cmd=${1:-}
run=${2:-}
case "$run" in *[!A-Za-z0-9._-]*|'') echo "usage: export|ingest <run> (run: [A-Za-z0-9._-]+)" >&2; exit 2;; esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case "$cmd" in
export)
  h5i-pay export "$work/run"
  aws s3 cp --only-show-errors --recursive "$work/run/" "s3://${INPUT_BUCKET}/runs/${run}/"
  ;;
ingest)
  aws s3 cp --only-show-errors --recursive "s3://${OUTPUT_BUCKET}/runs/${run}/reports/" "$work/reports/"
  h5i-pay ingest "$run" "$work/reports"
  ;;
*)
  echo "usage: export|ingest <run>" >&2
  exit 2
  ;;
esac
