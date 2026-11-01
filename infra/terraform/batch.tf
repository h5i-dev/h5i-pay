# AWS Batch for the GPU batch engine (reconciliation, bulk policy checks).
#
# Two queues, because AWS Batch does not mix EC2 and Fargate compute
# environments in one queue: `batch-gpu` (EC2 GPU instances, scaled from
# zero) and `batch-cpu` (Fargate). The scheduler submits to the GPU queue
# and falls back to the CPU queue when GPU capacity or quota is missing
# (docs/runbooks/batch-jobs.md). Both run the same worker image; without a
# GPU it uses the multithreaded CPU engine (h5i-app-gpu ExecutionPolicy::Auto).
# Workers read inputs from and write reports to S3; they have no database
# access and cannot move money.

data "aws_ssm_parameter" "ecs_gpu_ami" {
  name = "/aws/service/ecs/optimized-ami/amazon-linux-2023/gpu/recommended/image_id"
}

resource "aws_launch_template" "gpu" {
  name_prefix = "${local.name}-gpu-"
  image_id    = data.aws_ssm_parameter.ecs_gpu_ami.value
  metadata_options {
    http_tokens = "required"
    # 1: containers cannot reach IMDS (and the instance role); jobs get
    # their own role from the ECS agent's credential endpoint.
    http_put_response_hop_limit = 1
  }
  block_device_mappings {
    device_name = "/dev/xvda"
    ebs {
      volume_size = 60
      volume_type = "gp3"
      encrypted   = true
      kms_key_id  = aws_kms_key.main.arn
    }
  }
}

resource "aws_batch_compute_environment" "gpu" {
  compute_environment_name_prefix = "${local.name}-gpu-"
  type                            = "MANAGED"
  service_role                    = aws_iam_role.batch_service.arn
  state                           = var.gpu_max_vcpus > 0 ? "ENABLED" : "DISABLED"

  compute_resources {
    type                = var.gpu_use_spot ? "SPOT" : "EC2"
    allocation_strategy = var.gpu_use_spot ? "SPOT_PRICE_CAPACITY_OPTIMIZED" : "BEST_FIT_PROGRESSIVE"
    min_vcpus           = 0
    max_vcpus           = max(var.gpu_max_vcpus, 4)
    instance_type       = var.gpu_instance_types
    instance_role       = aws_iam_instance_profile.batch_instance.arn
    subnets             = aws_subnet.private[*].id
    security_group_ids  = [aws_security_group.batch.id]
    launch_template {
      launch_template_id = aws_launch_template.gpu.id
      version            = "$Latest"
    }
    ec2_configuration {
      image_type = "ECS_AL2023_NVIDIA"
    }
  }

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_batch_compute_environment" "cpu" {
  compute_environment_name_prefix = "${local.name}-cpu-"
  type                            = "MANAGED"
  service_role                    = aws_iam_role.batch_service.arn
  compute_resources {
    type               = "FARGATE"
    max_vcpus          = var.cpu_fallback_max_vcpus
    subnets            = aws_subnet.private[*].id
    security_group_ids = [aws_security_group.batch.id]
  }
  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_batch_job_queue" "gpu" {
  name     = "${local.name}-batch-gpu"
  state    = "ENABLED"
  priority = 10
  compute_environment_order {
    order               = 1
    compute_environment = aws_batch_compute_environment.gpu.arn
  }
}

resource "aws_batch_job_queue" "cpu" {
  name     = "${local.name}-batch-cpu"
  state    = "ENABLED"
  priority = 5
  compute_environment_order {
    order               = 1
    compute_environment = aws_batch_compute_environment.cpu.arn
  }
}

locals {
  worker_image = "${aws_ecr_repository.repo["worker"].repository_url}:${var.worker_image_tag}"
  worker_env = [
    { name = "INPUT_BUCKET", value = aws_s3_bucket.b["settlements"].bucket },
    { name = "OUTPUT_BUCKET", value = aws_s3_bucket.b["batch"].bucket },
    { name = "AUDIT_BUCKET", value = aws_s3_bucket.b["audit"].bucket },
  ]
  worker_logs = {
    logDriver = "awslogs"
    options = {
      awslogs-group         = aws_cloudwatch_log_group.batch.name
      awslogs-region        = var.region
      awslogs-stream-prefix = "reconcile"
    }
  }
}

# Parameters: internal and external are S3 keys in INPUT_BUCKET; the report
# goes to OUTPUT_BUCKET under the job id and is copied to AUDIT_BUCKET.
resource "aws_batch_job_definition" "reconcile_gpu" {
  name                  = "${local.name}-reconcile-gpu"
  type                  = "container"
  platform_capabilities = ["EC2"]
  parameters            = { internal = "", external = "", engine = "auto" }
  retry_strategy {
    attempts = 2
    evaluate_on_exit {
      on_status_reason = "Host EC2*"
      action           = "RETRY"
    }
    evaluate_on_exit {
      on_reason = "*"
      action    = "EXIT"
    }
  }
  timeout {
    attempt_duration_seconds = 3600
  }
  container_properties = jsonencode({
    image      = local.worker_image
    command    = ["reconcile", "Ref::internal", "Ref::external", "Ref::engine"]
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

resource "aws_batch_job_definition" "reconcile_cpu" {
  name                  = "${local.name}-reconcile-cpu"
  type                  = "container"
  platform_capabilities = ["FARGATE"]
  parameters            = { internal = "", external = "", engine = "parallel" }
  timeout {
    attempt_duration_seconds = 3600
  }
  container_properties = jsonencode({
    image            = local.worker_image
    command          = ["reconcile", "Ref::internal", "Ref::external", "Ref::engine"]
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
