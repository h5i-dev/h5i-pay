# h5i-pay infrastructure (M4)

Infrastructure as code for the AWS layout of the design (section 5). It is
written to be reviewed and applied by an operator. **Nothing here has been
applied**; no AWS resource was created while writing it. `terraform fmt`
and `terraform validate` pass (Terraform 1.9.8, hashicorp/aws ~> 5.80).

```
Clients ─► Route 53 ─► ALB (TLS 1.2+, WAF: rate limit + AWS managed rules)
                        │
          ┌─────────────┴──────────── private subnets ──────────────────────┐
          │ ECS Fargate: h5i-pay API (+ ADOT collector)   ──► RDS PostgreSQL │ isolated subnets, Multi-AZ, KMS
          │   │  ▲ PSP calls via NAT (Stripe test mode)                     │
          │   ├─► SQS jobs (+ DLQ)                                          │
          │   └─► AWS Batch: GPU queue (g4dn/g5, from zero) / CPU queue (Fargate)
          │          └─► S3: settlements (in), batch (out), audit (Object Lock)
          └─────────────────────────────────────────────────────────────────┘
Secrets Manager + KMS · CloudWatch logs/metrics/alarms → SNS · AWS Backup with weekly restore tests
```

| File | What |
|---|---|
| `terraform/network.tf` | VPC over 2 AZs: public (ALB, NAT), private (ECS, Batch), isolated data subnets (RDS, no route out); S3 gateway and interface endpoints (ECR, logs, Secrets Manager, SQS, KMS, STS, CloudWatch); VPC flow logs of rejected traffic |
| `terraform/security.tf` | security groups: internet → ALB:443 → API:8080 → RDS:5432; Batch workers have outbound HTTPS only and no database path |
| `terraform/data.tf` | KMS key (rotation on); Secrets Manager (HMAC secret generated; Stripe **test** keys filled by an operator, never in Terraform); RDS PostgreSQL 17 (Multi-AZ, encrypted, RDS-managed master password, IAM auth, 14-day backups, deletion protection, Performance Insights); S3 buckets (SSE-KMS, versioning, TLS-only, public access blocked, Object Lock compliance mode for audit exports); SQS with DLQ |
| `terraform/compute.tf` | ECR (immutable tags, scan on push); ALB with HTTP→HTTPS redirect, access logs; WAF; ECS cluster, Fargate task (read-only root, non-root user, secrets injected), service with circuit breaker + alarm-based rollback, CPU autoscaling |
| `terraform/batch.tf` | AWS Batch GPU compute environment (ECS GPU AMI, min 0 vCPU, IMDSv2, encrypted EBS), Fargate CPU environment, two queues, job definitions for the reconciliation worker |
| `terraform/iam.tf` | one role per actor (task execution, API task, Batch service/instance/job, RDS monitoring, flow logs, backup), GitHub OIDC deploy role limited to the `test` environment |
| `terraform/observability.tf` | log groups (KMS, 90 days); alarms: ledger invariant violated, ledger posting refused, reconciliation exceptions, ALB 5xx %, unhealthy targets, DLQ depth, queue age, RDS CPU and storage; SNS topic; AWS Backup plan and weekly restore-testing plan |
| `terraform/environments/test.tfvars` | a test-mode environment (Mock PSP, synthetic funds) |
| `docker/Dockerfile.api` | API image: Rust build stage, distroless `cc` runtime as uid 65532 |
| `docker/Dockerfile.worker` | batch worker: CUDA 13.2 devel build stage (PTX for compute_75), CUDA base runtime with the AWS CLI, uid 10001 |
| `docker/batch-entrypoint.sh` | S3 in → `h5i-pay-batch reconcile` → S3 out (reports and audit copy) |
| `docker/docker-compose.yml` | local PostgreSQL + API with the Mock PSP |
| `../.github/workflows/ci.yml` | build, clippy, tests with PostgreSQL, CUDA build, optional GPU runner tests and benchmark, proofs (`h5i app extract --check`, `check`, `mutate`), Terraform checks, images to ECR via OIDC, deploy to `test` with approval |

## Apply (operator)

```sh
cd infra/terraform
cp backend.hcl.example backend.hcl        # state bucket + lock table
terraform init -backend-config=backend.hcl
terraform plan -var-file=environments/test.tfvars -out plan
terraform apply plan
# Test-mode PSP keys, never live ones (the API refuses sk_live_):
aws secretsmanager put-secret-value --secret-id h5i-pay-test/stripe-secret-key --secret-string sk_test_...
```

Before the first GPU job, check the account's quota for "Running On-Demand G
and VT instances" (vCPUs) in the region; `gpu_max_vcpus = 0` disables the
GPU environment and leaves the CPU queue.

## Known gaps (tracked in `docs/security-review.md`)

- **The ADOT collector image is `latest`.** Pin a digest before production.
