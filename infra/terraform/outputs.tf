output "api_url" {
  value = var.api_domain_name != "" ? "https://${var.api_domain_name}" : "https://${aws_lb.api.dns_name}"
}

output "ecr_repositories" {
  value = { for k, r in aws_ecr_repository.repo : k => r.repository_url }
}

output "ecs_cluster" {
  value = aws_ecs_cluster.main.name
}

output "ecs_service" {
  value = aws_ecs_service.api.name
}

output "db_endpoint" {
  value = aws_db_instance.main.address
}

output "batch_queues" {
  value = { gpu = aws_batch_job_queue.gpu.name, cpu = aws_batch_job_queue.cpu.name }
}

output "buckets" {
  value = { for k, b in aws_s3_bucket.b : k => b.bucket }
}

output "jobs_queue_url" {
  value = aws_sqs_queue.jobs.url
}

output "ci_deploy_role_arn" {
  value = aws_iam_role.ci_deploy.arn
}

output "secrets_to_fill" {
  description = "Secrets an operator sets by hand (test-mode PSP keys)."
  value       = [aws_secretsmanager_secret.stripe_secret_key.name, aws_secretsmanager_secret.stripe_webhook_secret.name]
}
