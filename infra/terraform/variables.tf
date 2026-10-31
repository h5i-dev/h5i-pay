variable "region" {
  type    = string
  default = "us-east-1"
}

variable "environment" {
  description = "Name of the environment (test, staging, prod). Used in resource names."
  type        = string
  default     = "test"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,15}$", var.environment))
    error_message = "environment must be 2-16 lowercase letters, digits or dashes."
  }
}

variable "psp_mode" {
  description = "PSP the API uses: mock or stripe. Stripe keys must be test-mode keys; the binary refuses live keys."
  type        = string
  default     = "mock"

  validation {
    condition     = contains(["mock", "stripe"], var.psp_mode)
    error_message = "psp_mode must be mock or stripe."
  }
}

variable "vpc_cidr" {
  type    = string
  default = "10.40.0.0/16"
}

variable "az_count" {
  description = "Availability zones to spread subnets, NAT and RDS over."
  type        = number
  default     = 2
}

variable "single_nat_gateway" {
  description = "One NAT gateway for all AZs (cheaper, test only). Production uses one per AZ."
  type        = bool
  default     = true
}

variable "api_image_tag" {
  description = "Tag of the h5i-pay API image in ECR (immutable tags; CI pushes the git SHA)."
  type        = string
}

variable "worker_image_tag" {
  description = "Tag of the batch worker image in ECR."
  type        = string
}

variable "api_cpu" {
  type    = number
  default = 1024
}

variable "api_memory" {
  type    = number
  default = 2048
}

variable "api_desired_count" {
  type    = number
  default = 2
}

variable "api_max_count" {
  type    = number
  default = 6
}

variable "db_instance_class" {
  type    = string
  default = "db.t4g.medium"
}

variable "db_allocated_storage" {
  type    = number
  default = 50
}

variable "db_multi_az" {
  description = "Multi-AZ RDS. Required before anything but synthetic funds."
  type        = bool
  default     = true
}

variable "db_backup_retention_days" {
  type    = number
  default = 14
}

variable "gpu_instance_types" {
  description = "AWS Batch GPU instance types. g4dn.xlarge (T4) first, g5.xlarge (A10G) for comparison; mind the account's G/VT vCPU quota."
  type        = list(string)
  default     = ["g4dn.xlarge", "g5.xlarge"]
}

variable "gpu_max_vcpus" {
  description = "Upper bound of the GPU compute environment. 0 disables GPU jobs (CPU fallback only)."
  type        = number
  default     = 8
}

variable "gpu_use_spot" {
  type    = bool
  default = false
}

variable "cpu_fallback_max_vcpus" {
  type    = number
  default = 16
}

variable "acm_certificate_arn" {
  description = "ACM certificate for the ALB's HTTPS listener."
  type        = string
}

variable "route53_zone_id" {
  description = "Hosted zone for the API record; empty to skip DNS."
  type        = string
  default     = ""
}

variable "api_domain_name" {
  type    = string
  default = ""
}

variable "alert_email" {
  description = "Subscribed to the alarm topic (confirmation required)."
  type        = string
  default     = ""
}

variable "waf_rate_limit_per_5min" {
  description = "Requests per 5 minutes per client IP before WAF blocks."
  type        = number
  default     = 2000
}

variable "audit_retention_days" {
  description = "Object Lock retention (compliance mode) of audit exports."
  type        = number
  default     = 400
}

variable "github_repository" {
  description = "owner/repo allowed to assume the CI deploy role through GitHub OIDC."
  type        = string
  default     = "h5i-dev/h5i-pay"
}

variable "enable_otel_collector" {
  description = "Run an AWS Distro for OpenTelemetry collector sidecar next to the API."
  type        = bool
  default     = true
}
