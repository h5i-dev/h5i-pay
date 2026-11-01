# Nightly batch reconciliation (item C): one Step Functions execution per
# night, its name the run id.
#
#   1. export  — ECS task (ops image): `h5i-pay export`, every tenant's
#                internal records (payment kernel) and external records (PSP
#                settlement report) to s3://<settlements>/runs/<run>/.
#   2. worker  — AWS Batch (GPU queue, CPU fallback on failure):
#                `reconcile-run <run>`, reports to s3://<batch>/runs/<run>/reports/
#                and the Object-Lock audit bucket.
#   3. ingest  — ECS task (ops image): `h5i-pay ingest <run>`, reports and
#                exceptions into the `ops` schema; operators read them with
#                GET /v1/reconcile/reports.
#
# Nothing in it moves money (docs/runbooks/reconciliation-exceptions.md).

variable "ops_image_tag" {
  description = "Tag of the ops image (export/ingest) in ECR; CI pushes the git SHA."
  type        = string
  default     = ""
}

variable "reconcile_schedule" {
  description = "When the nightly reconciliation runs (EventBridge Scheduler expression); empty disables it."
  type        = string
  default     = "cron(30 3 * * ? *)"
}

locals {
  ops_image = "${aws_ecr_repository.repo["ops"].repository_url}:${var.ops_image_tag == "" ? var.api_image_tag : var.ops_image_tag}"
  # Database and PSP, as the API has them; no token keys (the jobs verify no tokens).
  ops_secrets = [for s in local.api_secrets : s if !startswith(s.name, "H5I_APP_SECRET") && !startswith(s.name, "H5I_PAY_OPERATOR_SECRET")]
  ops_container = {
    name      = "ops"
    image     = local.ops_image
    essential = true
    user      = "10002:10002"
    environment = concat(
      [for e in local.api_container.environment : e if e.name != "LISTEN" && e.name != "OTEL_EXPORTER_OTLP_ENDPOINT"],
      [
        { name = "INPUT_BUCKET", value = aws_s3_bucket.b["settlements"].bucket },
        { name = "OUTPUT_BUCKET", value = aws_s3_bucket.b["batch"].bucket },
      ],
    )
    secrets = local.ops_secrets
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.batch.name
        awslogs-region        = var.region
        awslogs-stream-prefix = "ops"
      }
    }
  }
}

resource "aws_ecs_task_definition" "ops" {
  family                   = "${local.name}-ops"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = 1024
  memory                   = 4096
  execution_role_arn       = aws_iam_role.task_execution.arn
  task_role_arn            = aws_iam_role.ops_task.arn
  runtime_platform {
    operating_system_family = "LINUX"
    cpu_architecture        = "X86_64"
  }
  ephemeral_storage {
    size_in_gib = 50
  }
  container_definitions = jsonencode([local.ops_container])
}

resource "aws_iam_role" "ops_task" {
  name               = "${local.name}-ops-task"
  assume_role_policy = data.aws_iam_policy_document.ecs_tasks_assume.json
}

data "aws_iam_policy_document" "ops_task" {
  statement {
    actions   = ["s3:PutObject"]
    resources = ["${aws_s3_bucket.b["settlements"].arn}/runs/*"]
  }
  statement {
    actions   = ["s3:GetObject"]
    resources = ["${aws_s3_bucket.b["batch"].arn}/runs/*"]
  }
  statement {
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.b["batch"].arn]
    condition {
      test     = "StringLike"
      variable = "s3:prefix"
      values   = ["runs/*"]
    }
  }
  statement {
    actions   = ["kms:Decrypt", "kms:GenerateDataKey"]
    resources = [aws_kms_key.main.arn]
  }
}

resource "aws_iam_role_policy" "ops_task" {
  name   = "ops"
  role   = aws_iam_role.ops_task.id
  policy = data.aws_iam_policy_document.ops_task.json
}

# The worker for a whole run (all tenants' pairs in one job).
resource "aws_batch_job_definition" "reconcile_run_gpu" {
  name                  = "${local.name}-reconcile-run-gpu"
  type                  = "container"
  platform_capabilities = ["EC2"]
  parameters            = { run = "", engine = "auto" }
  timeout {
    attempt_duration_seconds = 7200
  }
  container_properties = jsonencode({
    image      = local.worker_image
    command    = ["reconcile-run", "Ref::run", "Ref::engine"]
    jobRoleArn = aws_iam_role.batch_job.arn
    resourceRequirements = [
      { type = "VCPU", value = "4" },
      { type = "MEMORY", value = "14000" },
      { type = "GPU", value = "1" },
    ]
    environment            = local.worker_env
    readonlyRootFilesystem = false
    logConfiguration       = local.worker_logs
  })
}

resource "aws_batch_job_definition" "reconcile_run_cpu" {
  name                  = "${local.name}-reconcile-run-cpu"
  type                  = "container"
  platform_capabilities = ["FARGATE"]
  parameters            = { run = "", engine = "parallel" }
  timeout {
    attempt_duration_seconds = 7200
  }
  container_properties = jsonencode({
    image            = local.worker_image
    command          = ["reconcile-run", "Ref::run", "Ref::engine"]
    jobRoleArn       = aws_iam_role.batch_job.arn
    executionRoleArn = aws_iam_role.task_execution.arn
    resourceRequirements = [
      { type = "VCPU", value = "4" },
      { type = "MEMORY", value = "16384" },
    ]
    fargatePlatformConfiguration = { platformVersion = "LATEST" }
    networkConfiguration         = { assignPublicIp = "DISABLED" }
    environment                  = local.worker_env
    logConfiguration             = local.worker_logs
  })
}

locals {
  ops_network = {
    AwsvpcConfiguration = {
      Subnets        = aws_subnet.private[*].id
      SecurityGroups = [aws_security_group.api.id]
      AssignPublicIp = "DISABLED"
    }
  }
  ops_task = {
    LaunchType           = "FARGATE"
    Cluster              = aws_ecs_cluster.main.arn
    TaskDefinition       = aws_ecs_task_definition.ops.arn_without_revision
    NetworkConfiguration = local.ops_network
  }
  # The run id is the execution's name (a UUID, or what an operator passes).
  worker_parameters = { "run.$" = "$$.Execution.Name", "engine" = "auto" }
}

resource "aws_sfn_state_machine" "reconcile" {
  name     = "${local.name}-reconcile"
  role_arn = aws_iam_role.sfn_reconcile.arn
  definition = jsonencode({
    Comment = "Nightly reconciliation: export, worker, ingest"
    StartAt = "Export"
    States = {
      Export = {
        Type     = "Task"
        Resource = "arn:aws:states:::ecs:runTask.sync"
        Parameters = merge(local.ops_task, {
          Overrides = { ContainerOverrides = [{ Name = "ops", "Command.$" = "States.Array('export', $$.Execution.Name)" }] }
        })
        Retry      = [{ ErrorEquals = ["States.TaskFailed"], MaxAttempts = 1, IntervalSeconds = 60 }]
        ResultPath = null
        Next       = "WorkerGpu"
      }
      WorkerGpu = {
        Type     = "Task"
        Resource = "arn:aws:states:::batch:submitJob.sync"
        Parameters = {
          JobName       = "reconcile-run"
          JobQueue      = aws_batch_job_queue.gpu.arn
          JobDefinition = aws_batch_job_definition.reconcile_run_gpu.arn
          Parameters    = local.worker_parameters
        }
        TimeoutSeconds = 10800
        Catch          = [{ ErrorEquals = ["States.ALL"], ResultPath = null, Next = "WorkerCpu" }]
        ResultPath     = null
        Next           = "Ingest"
      }
      # No GPU capacity, or the GPU job failed: the same run on Fargate CPUs.
      WorkerCpu = {
        Type     = "Task"
        Resource = "arn:aws:states:::batch:submitJob.sync"
        Parameters = {
          JobName       = "reconcile-run-cpu"
          JobQueue      = aws_batch_job_queue.cpu.arn
          JobDefinition = aws_batch_job_definition.reconcile_run_cpu.arn
          Parameters    = { "run.$" = "$$.Execution.Name", "engine" = "parallel" }
        }
        ResultPath = null
        Next       = "Ingest"
      }
      Ingest = {
        Type     = "Task"
        Resource = "arn:aws:states:::ecs:runTask.sync"
        Parameters = merge(local.ops_task, {
          Overrides = { ContainerOverrides = [{ Name = "ops", "Command.$" = "States.Array('ingest', $$.Execution.Name)" }] }
        })
        Retry = [{ ErrorEquals = ["States.TaskFailed"], MaxAttempts = 2, IntervalSeconds = 60 }]
        End   = true
      }
    }
  })
}

resource "aws_iam_role" "sfn_reconcile" {
  name = "${local.name}-sfn-reconcile"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "states.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = { StringEquals = { "aws:SourceAccount" = data.aws_caller_identity.current.account_id } }
    }]
  })
}

resource "aws_iam_role_policy" "sfn_reconcile" {
  role = aws_iam_role.sfn_reconcile.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect    = "Allow"
        Action    = ["ecs:RunTask"]
        Resource  = ["${aws_ecs_task_definition.ops.arn_without_revision}:*", aws_ecs_task_definition.ops.arn_without_revision]
        Condition = { ArnEquals = { "ecs:cluster" = aws_ecs_cluster.main.arn } }
      },
      { Effect = "Allow", Action = ["ecs:StopTask", "ecs:DescribeTasks"], Resource = "*" },
      {
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = [aws_iam_role.task_execution.arn, aws_iam_role.ops_task.arn]
        Condition = { StringEquals = { "iam:PassedToService" = "ecs-tasks.amazonaws.com" } }
      },
      {
        Effect = "Allow"
        Action = ["batch:SubmitJob"]
        Resource = [
          aws_batch_job_queue.gpu.arn, aws_batch_job_queue.cpu.arn,
          "${aws_batch_job_definition.reconcile_run_gpu.arn_prefix}:*", "${aws_batch_job_definition.reconcile_run_cpu.arn_prefix}:*",
        ]
      },
      { Effect = "Allow", Action = ["batch:DescribeJobs", "batch:TerminateJob"], Resource = "*" },
      # The .sync integrations wait through EventBridge managed rules.
      {
        Effect   = "Allow"
        Action   = ["events:PutTargets", "events:PutRule", "events:DescribeRule"]
        Resource = ["arn:aws:events:${var.region}:${data.aws_caller_identity.current.account_id}:rule/StepFunctions*"]
      },
    ]
  })
}

resource "aws_scheduler_schedule" "reconcile" {
  name        = "${local.name}-reconcile"
  group_name  = "default"
  description = "Nightly batch reconciliation (export, worker, ingest)"
  state       = var.reconcile_schedule == "" ? "DISABLED" : "ENABLED"

  schedule_expression          = var.reconcile_schedule == "" ? "cron(30 3 * * ? *)" : var.reconcile_schedule
  schedule_expression_timezone = "UTC"
  flexible_time_window {
    mode = "OFF"
  }
  target {
    arn      = aws_sfn_state_machine.reconcile.arn
    role_arn = aws_iam_role.scheduler_reconcile.arn
    input    = jsonencode({})
  }
}

resource "aws_iam_role" "scheduler_reconcile" {
  name = "${local.name}-scheduler-reconcile"
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

resource "aws_iam_role_policy" "scheduler_reconcile" {
  role = aws_iam_role.scheduler_reconcile.id
  policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "states:StartExecution", Resource = aws_sfn_state_machine.reconcile.arn }]
  })
}

# Failed nightly runs page like any other alarm.
resource "aws_cloudwatch_metric_alarm" "reconcile_failed" {
  alarm_name          = "${local.name}-reconcile-failed"
  namespace           = "AWS/States"
  metric_name         = "ExecutionsFailed"
  dimensions          = { StateMachineArn = aws_sfn_state_machine.reconcile.arn }
  statistic           = "Sum"
  period              = 86400
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
}
