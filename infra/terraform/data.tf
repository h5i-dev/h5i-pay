# KMS, secrets, the database, object storage and queues.

data "aws_caller_identity" "current" {}

resource "aws_kms_key" "main" {
  description             = "${local.name}: RDS, S3, SQS, Secrets Manager, CloudWatch Logs"
  deletion_window_in_days = 30
  enable_key_rotation     = true
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "AccountAdmin"
        Effect    = "Allow"
        Principal = { AWS = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:root" }
        Action    = "kms:*"
        Resource  = "*"
      },
      {
        Sid       = "CloudWatchLogs"
        Effect    = "Allow"
        Principal = { Service = "logs.${var.region}.amazonaws.com" }
        Action    = ["kms:Encrypt*", "kms:Decrypt*", "kms:ReEncrypt*", "kms:GenerateDataKey*", "kms:Describe*"]
        Resource  = "*"
      },
    ]
  })
}

resource "aws_kms_alias" "main" {
  name          = "alias/${local.name}"
  target_key_id = aws_kms_key.main.key_id
}

# --- Secrets -----------------------------------------------------------
# Values that Terraform generates live in Secrets Manager only; PSP keys are
# put there by an operator (aws secretsmanager put-secret-value), never in
# Terraform variables, state or git.

# Bearer-token keys (SEC-02): one for user tokens, one for operator tokens.
# Each secret is JSON {"current": ..., "previous": ...}; the task injects both
# fields (H5I_APP_SECRET / H5I_APP_SECRET_PREVIOUS, H5I_PAY_OPERATOR_SECRET /
# H5I_PAY_OPERATOR_SECRET_PREVIOUS). Outside a rotation previous = current.
# Rotation is done out of band (docs/runbooks/secret-rotation.md), so
# Terraform only sets the initial value.
resource "random_password" "app_secret" {
  length  = 64
  special = false
}

resource "aws_secretsmanager_secret" "app" {
  #checkov:skip=CKV2_AWS_57:token keys rotate by the two-step runbook (current/previous), not a Lambda
  name        = "${local.name}/h5i-app-secret"
  description = "User-token HMAC key: JSON {current, previous}"
  kms_key_id  = aws_kms_key.main.arn
}

resource "aws_secretsmanager_secret_version" "app" {
  secret_id     = aws_secretsmanager_secret.app.id
  secret_string = jsonencode({ current = random_password.app_secret.result, previous = random_password.app_secret.result })
  lifecycle {
    ignore_changes = [secret_string]
  }
}

resource "random_password" "operator_secret" {
  length  = 64
  special = false
}

resource "aws_secretsmanager_secret" "operator" {
  #checkov:skip=CKV2_AWS_57:token keys rotate by the two-step runbook (current/previous), not a Lambda
  name        = "${local.name}/h5i-pay-operator-secret"
  description = "Operator-token HMAC key: JSON {current, previous}. Only the API and whoever issues operator tokens may read it."
  kms_key_id  = aws_kms_key.main.arn
}

resource "aws_secretsmanager_secret_version" "operator" {
  secret_id     = aws_secretsmanager_secret.operator.id
  secret_string = jsonencode({ current = random_password.operator_secret.result, previous = random_password.operator_secret.result })
  lifecycle {
    ignore_changes = [secret_string]
  }
}

resource "aws_secretsmanager_secret" "stripe_secret_key" {
  #checkov:skip=CKV2_AWS_57:issued by Stripe; rotated at Stripe (docs/runbooks/secret-rotation.md)
  name        = "${local.name}/stripe-secret-key"
  description = "Stripe TEST secret key (sk_test_...). Set out of band; the API refuses live keys."
  kms_key_id  = aws_kms_key.main.arn
}

resource "aws_secretsmanager_secret" "stripe_webhook_secret" {
  #checkov:skip=CKV2_AWS_57:issued by Stripe; rotated at Stripe (docs/runbooks/secret-rotation.md)
  name       = "${local.name}/stripe-webhook-secret"
  kms_key_id = aws_kms_key.main.arn
}

resource "aws_secretsmanager_secret" "mock_webhook_secret" {
  #checkov:skip=CKV2_AWS_57:test-only mock secret
  name       = "${local.name}/mock-webhook-secret"
  kms_key_id = aws_kms_key.main.arn
}

resource "random_password" "mock_webhook" {
  length  = 48
  special = false
}

resource "aws_secretsmanager_secret_version" "mock_webhook" {
  secret_id     = aws_secretsmanager_secret.mock_webhook_secret.id
  secret_string = "whsec_${random_password.mock_webhook.result}"
}

# --- PostgreSQL --------------------------------------------------------

resource "aws_db_subnet_group" "main" {
  name       = local.name
  subnet_ids = aws_subnet.data[*].id
}

resource "aws_db_parameter_group" "pg17" {
  name   = "${local.name}-pg17"
  family = "postgres17"

  # The engine runs every request SERIALIZABLE itself; these make
  # contention and slow statements visible.
  parameter {
    name  = "log_min_duration_statement"
    value = "500"
  }
  parameter {
    name  = "log_lock_waits"
    value = "1"
  }
  # Only TLS connections are accepted. The API connects with h5i-app-pg's
  # pool_tls, verifying the server certificate against the RDS global CA
  # bundle and its host name (DB_CA_FILE; docs/security-review.md SEC-01).
  parameter {
    name  = "rds.force_ssl"
    value = "1"
  }
  parameter {
    name  = "idle_in_transaction_session_timeout"
    value = "60000"
  }
}

resource "aws_db_instance" "main" {
  identifier                          = local.name
  engine                              = "postgres"
  engine_version                      = "17"
  instance_class                      = var.db_instance_class
  allocated_storage                   = var.db_allocated_storage
  max_allocated_storage               = var.db_allocated_storage * 4
  storage_type                        = "gp3"
  storage_encrypted                   = true
  kms_key_id                          = aws_kms_key.main.arn
  db_name                             = "h5ipay"
  username                            = "h5ipay_admin"
  manage_master_user_password         = true
  master_user_secret_kms_key_id       = aws_kms_key.main.arn
  iam_database_authentication_enabled = true
  multi_az                            = var.db_multi_az
  db_subnet_group_name                = aws_db_subnet_group.main.name
  vpc_security_group_ids              = [aws_security_group.db.id]
  parameter_group_name                = aws_db_parameter_group.pg17.name
  publicly_accessible                 = false
  backup_retention_period             = var.db_backup_retention_days
  backup_window                       = "17:00-18:00"
  maintenance_window                  = "sun:18:30-sun:19:30"
  copy_tags_to_snapshot               = true
  deletion_protection                 = true
  skip_final_snapshot                 = false
  final_snapshot_identifier           = "${local.name}-final"
  performance_insights_enabled        = true
  performance_insights_kms_key_id     = aws_kms_key.main.arn
  monitoring_interval                 = 30
  monitoring_role_arn                 = aws_iam_role.rds_monitoring.arn
  enabled_cloudwatch_logs_exports     = ["postgresql"]
  auto_minor_version_upgrade          = true
  apply_immediately                   = false
}

# --- S3 ----------------------------------------------------------------

locals {
  buckets = {
    settlements = "PSP settlement files (input to reconciliation)"
    batch       = "Batch job inputs and outputs"
    audit       = "Audit exports: ledger journal, reconciliation reports (Object Lock)"
  }
}

resource "aws_s3_bucket" "b" {
  #checkov:skip=CKV2_AWS_62:no consumer of S3 events; jobs are started by Step Functions
  #checkov:skip=CKV_AWS_18:access logging not set up yet (needs a log bucket); CloudTrail data events are the alternative
  #checkov:skip=CKV_AWS_144:single-region deployment; AWS Backup covers recovery
  #checkov:skip=CKV2_AWS_61:lifecycle rules are separate resources (batch scratch expiry, audit retention via Object Lock)
  #checkov:skip=CKV_AWS_21:versioning is aws_s3_bucket_versioning.b (for_each; not followed by checkov)
  #checkov:skip=CKV_AWS_145:KMS encryption is aws_s3_bucket_server_side_encryption_configuration.b (for_each)
  #checkov:skip=CKV2_AWS_6:public access block is aws_s3_bucket_public_access_block.b (for_each)
  for_each            = local.buckets
  bucket              = "${local.name}-${each.key}-${data.aws_caller_identity.current.account_id}"
  object_lock_enabled = each.key == "audit"
  tags                = { Purpose = each.value }
}

resource "aws_s3_bucket_public_access_block" "b" {
  for_each                = aws_s3_bucket.b
  bucket                  = each.value.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "b" {
  for_each = aws_s3_bucket.b
  bucket   = each.value.id
  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_versioning" "b" {
  for_each = aws_s3_bucket.b
  bucket   = each.value.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "b" {
  for_each = aws_s3_bucket.b
  bucket   = each.value.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm     = "aws:kms"
      kms_master_key_id = aws_kms_key.main.arn
    }
    bucket_key_enabled = true
  }
}

resource "aws_s3_bucket_policy" "tls_only" {
  for_each = aws_s3_bucket.b
  bucket   = each.value.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "DenyInsecureTransport"
      Effect    = "Deny"
      Principal = "*"
      Action    = "s3:*"
      Resource  = [each.value.arn, "${each.value.arn}/*"]
      Condition = { Bool = { "aws:SecureTransport" = "false" } }
    }]
  })
}

resource "aws_s3_bucket_object_lock_configuration" "audit" {
  bucket = aws_s3_bucket.b["audit"].id
  rule {
    default_retention {
      mode = "COMPLIANCE"
      days = var.audit_retention_days
    }
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "batch" {
  bucket = aws_s3_bucket.b["batch"].id
  rule {
    id     = "expire-scratch"
    status = "Enabled"
    filter {}
    expiration {
      days = 30
    }
    noncurrent_version_expiration {
      noncurrent_days = 7
    }
    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

# --- SQS ---------------------------------------------------------------

resource "aws_sqs_queue" "jobs_dlq" {
  name                      = "${local.name}-jobs-dlq"
  message_retention_seconds = 1209600
  kms_master_key_id         = aws_kms_key.main.arn
}

resource "aws_sqs_queue" "jobs" {
  name                       = "${local.name}-jobs"
  visibility_timeout_seconds = 300
  message_retention_seconds  = 345600
  kms_master_key_id          = aws_kms_key.main.arn
  redrive_policy = jsonencode({
    deadLetterTargetArn = aws_sqs_queue.jobs_dlq.arn
    maxReceiveCount     = 5
  })
}
