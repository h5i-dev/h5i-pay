#!/bin/sh
# AWS Batch job: reconcile <internal-key> <external-key> [engine]
# Inputs are S3 keys in $INPUT_BUCKET; the report goes to
# $OUTPUT_BUCKET/reports/<job>.json and a copy to the Object-Lock audit
# bucket. The worker has no database credentials and cannot move money.
set -eu
[ "${1:-}" = "reconcile" ] || { echo "usage: reconcile <internal-key> <external-key> [engine]" >&2; exit 2; }
internal=$2
external=$3
engine=${4:-auto}
job=${AWS_BATCH_JOB_ID:-local-$(date +%s)}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${internal}" "$work/internal.csv"
aws s3 cp --only-show-errors "s3://${INPUT_BUCKET}/${external}" "$work/external.csv"
h5i-pay-batch reconcile "$work/internal.csv" "$work/external.csv" "$work/report.json" --engine "$engine"
aws s3 cp --only-show-errors "$work/report.json" "s3://${OUTPUT_BUCKET}/reports/${job}.json"
aws s3 cp --only-show-errors "$work/report.json" "s3://${AUDIT_BUCKET}/reconciliation/${job}.json"
