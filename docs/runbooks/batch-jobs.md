# Batch jobs (reconciliation workers)

Queues: `h5i-pay-<env>-batch-gpu` (EC2 GPU, scales from zero) and
`h5i-pay-<env>-batch-cpu` (Fargate). Same image; the worker picks the GPU
only when one answers and the batch has at least `--min-gpu-rows`
(default 2,000,000: each job pays the CUDA context and JIT once;
`docs/benchmarks.md` explains the threshold).

Nightly: the `h5i-pay-<env>-reconcile` Step Functions state machine
(`infra/terraform/reconcile.tf`) runs export → `reconcile-run` on the GPU
queue (CPU queue if that fails) → ingest. The execution's name is the
run id. To re-run a night, start a new execution
(`aws stepfunctions start-execution --state-machine-arn … --name <new-id>`);
ingesting is idempotent per (tenant, run).
Results: `GET $API/v1/reconcile/reports[?run=<id>]` (operator token).

Bulk payout screening (the verified `policy::check`, GPU or CPU):
`h5i-pay-batch policy <requests.csv> <policy.json> <decisions.csv>`, or on
Batch the entrypoint's `policy <requests-key> <policy-key>`; decisions go
to `s3://<batch-bucket>/payouts/<job>.csv`. Approved rows are candidates;
the payouts are still posted as ledger transfers by an operator.

Submit one pair by hand:

```sh
aws batch submit-job --job-name recon-$(date +%F) \
  --job-queue h5i-pay-test-batch-gpu --job-definition h5i-pay-test-reconcile-gpu \
  --parameters internal=internal/2026-10-31.csv,external=stripe/2026-10-31.csv,engine=auto
```

| Situation | Action |
|---|---|
| Job stays `RUNNABLE` on the GPU queue | no GPU capacity or G/VT vCPU quota: submit the same parameters to the CPU queue with `--job-definition h5i-pay-test-reconcile-cpu`; request a quota increase |
| Job fails with a CUDA error | the worker does not silently fall back after selecting the GPU; rerun on the CPU queue (`engine=parallel`) and attach the log to an issue |
| `jobs-dlq` alarm | a message failed five times; inspect it, fix the input, re-drive with `aws sqs start-message-move-task` |
| `jobs-lag` alarm | queue backed up: raise `gpu_max_vcpus` / `cpu_fallback_max_vcpus` |
| Report shows exceptions | follow reconciliation-exceptions.md |

Performance regressions: the `gpu-tests` CI job (self-hosted GPU runner)
runs the differential suite and `bench all --sizes 100000,1000000`; compare
`bench.json` with `benchmarks/results/g5.xlarge.json` and investigate any
p50 more than 20% slower.
