# Scheduled recovery: `h5i-pay recover` as a one-shot Fargate task of the
# API task definition, started by EventBridge Scheduler. One pass asks the
# PSP about every payment and refund left half done (lost responses,
# crashes between the PSP and the database) and re-posts ledger transfers
# a capture implies, for every tenant with such work. Each change goes
# through the kernels in its own SERIALIZABLE transaction, so a pass that
# overlaps the next one, or the API, is safe. It never fails a payment the
# PSP has no record of; that stays an operator's call
# (docs/runbooks/psp-incidents.md).

resource "aws_scheduler_schedule" "recover" {
  #checkov:skip=CKV_AWS_297:the schedule's input is a command override; nothing secret
  name        = "${local.name}-recover"
  group_name  = "default"
  description = "h5i-pay recover: finish payments and refunds left half done (docs/failure-recovery.md)"
  state       = var.recover_schedule == "" ? "DISABLED" : "ENABLED"

  # A disabled schedule still needs a valid expression; the interval is the
  # variable's default, rate(5 minutes).
  schedule_expression          = var.recover_schedule == "" ? "rate(5 minutes)" : var.recover_schedule
  schedule_expression_timezone = "UTC"

  flexible_time_window {
    mode = "OFF"
  }

  target {
    arn      = aws_ecs_cluster.main.arn
    role_arn = aws_iam_role.scheduler_recover.arn

    ecs_parameters {
      # Without the revision: the latest ACTIVE revision, i.e. whatever image
      # the CI deploy job registered last, not the one Terraform created.
      task_definition_arn = aws_ecs_task_definition.api.arn_without_revision
      launch_type         = "FARGATE"
      task_count          = 1
      propagate_tags      = "TASK_DEFINITION"

      network_configuration {
        subnets          = aws_subnet.private[*].id
        security_groups  = [aws_security_group.api.id]
        assign_public_ip = false
      }
    }

    input = jsonencode({
      containerOverrides = [{ name = "api", command = ["recover"] }]
    })

    retry_policy {
      maximum_event_age_in_seconds = 600
      maximum_retry_attempts       = 2
    }
  }
}

# The scheduler may start this task definition family in this cluster and
# pass the task's two roles, nothing else.
resource "aws_iam_role" "scheduler_recover" {
  name = "${local.name}-scheduler-recover"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "scheduler.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = { StringEquals = { "aws:SourceAccount" = data.aws_caller_identity.current.account_id } }
    }]
  })
}

resource "aws_iam_role_policy" "scheduler_recover" {
  role = aws_iam_role.scheduler_recover.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect    = "Allow"
        Action    = "ecs:RunTask"
        Resource  = ["${aws_ecs_task_definition.api.arn_without_revision}:*", aws_ecs_task_definition.api.arn_without_revision]
        Condition = { ArnEquals = { "ecs:cluster" = aws_ecs_cluster.main.arn } }
      },
      {
        Effect    = "Allow"
        Action    = "ecs:TagResource"
        Resource  = "*"
        Condition = { StringEquals = { "ecs:CreateAction" = "RunTask" } }
      },
      {
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = [aws_iam_role.task_execution.arn, aws_iam_role.api_task.arn]
        Condition = { StringEquals = { "iam:PassedToService" = "ecs-tasks.amazonaws.com" } }
      },
    ]
  })
}

# The task prints one JSON line: {"event":"recovery","tenants_failed":…,
# "payments_unresolved":…,…}.
resource "aws_cloudwatch_log_metric_filter" "recovery_failed" {
  name           = "recovery-tenants-failed"
  log_group_name = aws_cloudwatch_log_group.api.name
  pattern        = "{ $.event = \"recovery\" }"
  metric_transformation {
    name      = "RecoveryTenantsFailed"
    namespace = "h5i-pay/${var.environment}"
    value     = "$.tenants_failed"
  }
}

resource "aws_cloudwatch_log_metric_filter" "recovery_unresolved" {
  name           = "recovery-payments-unresolved"
  log_group_name = aws_cloudwatch_log_group.api.name
  pattern        = "{ $.event = \"recovery\" }"
  metric_transformation {
    name      = "RecoveryPaymentsUnresolved"
    namespace = "h5i-pay/${var.environment}"
    value     = "$.payments_unresolved"
  }
}

resource "aws_cloudwatch_log_metric_filter" "recovery_ran" {
  name           = "recovery-ran"
  log_group_name = aws_cloudwatch_log_group.api.name
  pattern        = "{ $.event = \"recovery\" }"
  metric_transformation {
    name      = "RecoveryRuns"
    namespace = "h5i-pay/${var.environment}"
    value     = "1"
  }
}

resource "aws_cloudwatch_metric_alarm" "recovery_failed" {
  alarm_name          = "${local.name}-recovery-failed"
  alarm_description   = "Scheduled recovery could not process a tenant (PSP or database error). docs/runbooks/psp-incidents.md"
  namespace           = "h5i-pay/${var.environment}"
  metric_name         = "RecoveryTenantsFailed"
  statistic           = "Maximum"
  period              = 900
  evaluation_periods  = 2
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

# A payment the PSP has no record of stays PROCESSING until an operator
# decides; an hour of them is worth a look.
resource "aws_cloudwatch_metric_alarm" "recovery_unresolved" {
  alarm_name          = "${local.name}-recovery-unresolved"
  alarm_description   = "Payments still PROCESSING with no PSP record after an hour of recovery passes. docs/runbooks/psp-incidents.md"
  namespace           = "h5i-pay/${var.environment}"
  metric_name         = "RecoveryPaymentsUnresolved"
  statistic           = "Minimum"
  period              = 3600
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

# The schedule itself stopped (task failing to start, role broken).
resource "aws_cloudwatch_metric_alarm" "recovery_missing" {
  count               = var.recover_schedule == "" ? 0 : 1
  alarm_name          = "${local.name}-recovery-not-running"
  alarm_description   = "No scheduled recovery pass reported in an hour. docs/runbooks/psp-incidents.md"
  namespace           = "h5i-pay/${var.environment}"
  metric_name         = "RecoveryRuns"
  statistic           = "Sum"
  period              = 3600
  evaluation_periods  = 1
  threshold           = 1
  comparison_operator = "LessThanThreshold"
  treat_missing_data  = "breaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
}
