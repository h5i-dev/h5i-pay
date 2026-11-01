# Logs, alarms, and backups with restore tests.

resource "aws_cloudwatch_log_group" "api" {
  name              = "/h5i-pay/${var.environment}/api"
  retention_in_days = 365
  kms_key_id        = aws_kms_key.main.arn
}

resource "aws_cloudwatch_log_group" "batch" {
  name              = "/h5i-pay/${var.environment}/batch"
  retention_in_days = 365
  kms_key_id        = aws_kms_key.main.arn
}

resource "aws_cloudwatch_log_group" "flow" {
  #checkov:skip=CKV_AWS_338:flow logs are high-volume; 30 days by decision
  name              = "/h5i-pay/${var.environment}/vpc-flow"
  retention_in_days = 30
  kms_key_id        = aws_kms_key.main.arn
}

resource "aws_sns_topic" "alerts" {
  name              = "${local.name}-alerts"
  kms_master_key_id = aws_kms_key.main.arn
}

resource "aws_sns_topic_subscription" "email" {
  count     = var.alert_email == "" ? 0 : 1
  topic_arn = aws_sns_topic.alerts.arn
  protocol  = "email"
  endpoint  = var.alert_email
}

# --- Application signals from logs -------------------------------------------
# The API logs "ledger invariant violated" when /v1/ledger/audit finds rows
# that break what the Lean proofs guarantee, and "ledger posting refused"
# when the orchestrator cannot post a transfer the payment state implies.

resource "aws_cloudwatch_log_metric_filter" "ledger_violation" {
  name           = "ledger-invariant-violated"
  log_group_name = aws_cloudwatch_log_group.api.name
  pattern        = "\"ledger invariant violated\""
  metric_transformation {
    name      = "LedgerInvariantViolations"
    namespace = "h5i-pay/${var.environment}"
    value     = "1"
  }
}

resource "aws_cloudwatch_log_metric_filter" "posting_refused" {
  name           = "ledger-posting-refused"
  log_group_name = aws_cloudwatch_log_group.api.name
  pattern        = "\"ledger posting refused\""
  metric_transformation {
    name      = "LedgerPostingRefused"
    namespace = "h5i-pay/${var.environment}"
    value     = "1"
  }
}

resource "aws_cloudwatch_log_metric_filter" "unmatched_settlement" {
  name           = "unmatched-settlement"
  log_group_name = aws_cloudwatch_log_group.batch.name
  pattern        = "{ $.exceptions_count > 0 }"
  metric_transformation {
    name      = "ReconciliationExceptions"
    namespace = "h5i-pay/${var.environment}"
    value     = "$.exceptions_count"
  }
}

resource "aws_cloudwatch_metric_alarm" "ledger_violation" {
  alarm_name          = "${local.name}-ledger-invariant-violated"
  alarm_description   = "A proven ledger invariant does not hold on the stored rows. Page: docs/runbooks/ledger-imbalance.md"
  namespace           = "h5i-pay/${var.environment}"
  metric_name         = "LedgerInvariantViolations"
  statistic           = "Sum"
  period              = 60
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "posting_refused" {
  alarm_name          = "${local.name}-ledger-posting-refused"
  alarm_description   = "The ledger refused a posting a payment implies. docs/runbooks/ledger-imbalance.md"
  namespace           = "h5i-pay/${var.environment}"
  metric_name         = "LedgerPostingRefused"
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "unmatched_settlement" {
  alarm_name          = "${local.name}-unmatched-settlement"
  alarm_description   = "Reconciliation found exceptions. docs/runbooks/reconciliation-exceptions.md"
  namespace           = "h5i-pay/${var.environment}"
  metric_name         = "ReconciliationExceptions"
  statistic           = "Sum"
  period              = 3600
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "api_5xx" {
  alarm_name          = "${local.name}-api-5xx"
  alarm_description   = "Target 5xx above 1% of requests. Also rolls back ECS deployments."
  evaluation_periods  = 3
  threshold           = 1
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
  metric_query {
    id          = "rate"
    expression  = "100 * errors / MAX([errors, requests])"
    label       = "5xx %"
    return_data = true
  }
  metric_query {
    id = "errors"
    metric {
      namespace   = "AWS/ApplicationELB"
      metric_name = "HTTPCode_Target_5XX_Count"
      period      = 60
      stat        = "Sum"
      dimensions  = { LoadBalancer = aws_lb.api.arn_suffix }
    }
  }
  metric_query {
    id = "requests"
    metric {
      namespace   = "AWS/ApplicationELB"
      metric_name = "RequestCount"
      period      = 60
      stat        = "Sum"
      dimensions  = { LoadBalancer = aws_lb.api.arn_suffix }
    }
  }
}

resource "aws_cloudwatch_metric_alarm" "unhealthy_targets" {
  alarm_name          = "${local.name}-unhealthy-targets"
  namespace           = "AWS/ApplicationELB"
  metric_name         = "UnHealthyHostCount"
  statistic           = "Maximum"
  period              = 60
  evaluation_periods  = 3
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { LoadBalancer = aws_lb.api.arn_suffix, TargetGroup = aws_lb_target_group.api.arn_suffix }
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "dlq" {
  alarm_name          = "${local.name}-jobs-dlq"
  alarm_description   = "Jobs failed five times. docs/runbooks/batch-jobs.md"
  namespace           = "AWS/SQS"
  metric_name         = "ApproximateNumberOfMessagesVisible"
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { QueueName = aws_sqs_queue.jobs_dlq.name }
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "queue_lag" {
  alarm_name          = "${local.name}-jobs-lag"
  alarm_description   = "Oldest job waiting over 15 minutes."
  namespace           = "AWS/SQS"
  metric_name         = "ApproximateAgeOfOldestMessage"
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 2
  threshold           = 900
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { QueueName = aws_sqs_queue.jobs.name }
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "db_cpu" {
  alarm_name          = "${local.name}-db-cpu"
  namespace           = "AWS/RDS"
  metric_name         = "CPUUtilization"
  statistic           = "Average"
  period              = 300
  evaluation_periods  = 3
  threshold           = 80
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { DBInstanceIdentifier = aws_db_instance.main.identifier }
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "db_storage" {
  alarm_name          = "${local.name}-db-free-storage"
  namespace           = "AWS/RDS"
  metric_name         = "FreeStorageSpace"
  statistic           = "Minimum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 5 * 1024 * 1024 * 1024
  comparison_operator = "LessThanThreshold"
  dimensions          = { DBInstanceIdentifier = aws_db_instance.main.identifier }
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

# --- Backups, and tests that they restore ------------------------------------

resource "aws_backup_vault" "main" {
  name        = local.name
  kms_key_arn = aws_kms_key.main.arn
}

resource "aws_backup_plan" "db" {
  name = "${local.name}-db"
  rule {
    rule_name         = "daily"
    target_vault_name = aws_backup_vault.main.name
    schedule          = "cron(0 16 * * ? *)"
    lifecycle {
      delete_after = 35
    }
  }
}

resource "aws_iam_role" "backup" {
  name = "${local.name}-backup"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "sts:AssumeRole", Principal = { Service = "backup.amazonaws.com" } }]
  })
}

resource "aws_iam_role_policy_attachment" "backup" {
  role       = aws_iam_role.backup.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSBackupServiceRolePolicyForBackup"
}

resource "aws_iam_role_policy_attachment" "restore" {
  role       = aws_iam_role.backup.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSBackupServiceRolePolicyForRestores"
}

resource "aws_backup_selection" "db" {
  name         = "${local.name}-db"
  plan_id      = aws_backup_plan.db.id
  iam_role_arn = aws_iam_role.backup.arn
  resources    = [aws_db_instance.main.arn]
}

# AWS Backup restores the latest recovery point into a throwaway instance
# every week; docs/runbooks/backup-restore.md adds the ledger audit on it.
resource "aws_backup_restore_testing_plan" "db" {
  name                         = replace("${local.name}_db_restore", "-", "_")
  schedule_expression          = "cron(0 2 ? * SUN *)"
  start_window_hours           = 4
  schedule_expression_timezone = "Etc/UTC"
  recovery_point_selection {
    algorithm             = "LATEST_WITHIN_WINDOW"
    include_vaults        = [aws_backup_vault.main.arn]
    recovery_point_types  = ["SNAPSHOT"]
    selection_window_days = 7
  }
}

resource "aws_backup_restore_testing_selection" "db" {
  name                      = "rds"
  restore_testing_plan_name = aws_backup_restore_testing_plan.db.name
  protected_resource_type   = "RDS"
  iam_role_arn              = aws_iam_role.backup.arn
  protected_resource_arns   = [aws_db_instance.main.arn]
  restore_metadata_overrides = {
    dbsubnetgroupname   = aws_db_subnet_group.main.name
    vpcsecuritygroupids = jsonencode([aws_security_group.db.id])
  }
  validation_window_hours = 4
}

# WAF request logs (the log group name must start with aws-waf-logs-).
resource "aws_cloudwatch_log_group" "waf" {
  #checkov:skip=CKV_AWS_338:WAF request logs; 90 days by decision
  name              = "aws-waf-logs-${local.name}"
  retention_in_days = 90
  kms_key_id        = aws_kms_key.main.arn
}

resource "aws_wafv2_web_acl_logging_configuration" "api" {
  resource_arn            = aws_wafv2_web_acl.api.arn
  log_destination_configs = [aws_cloudwatch_log_group.waf.arn]
  redacted_fields {
    single_header {
      name = "authorization"
    }
  }
}

resource "aws_cloudwatch_log_group" "sfn" {
  name              = "/h5i-pay/${var.environment}/states"
  retention_in_days = 365
  kms_key_id        = aws_kms_key.main.arn
}
