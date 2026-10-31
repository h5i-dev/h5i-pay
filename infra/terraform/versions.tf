# h5i-pay on AWS (design section 5). This configuration is written to be
# reviewed and applied by an operator; nothing in this repository applies it.
#
#   terraform init -backend-config=backend.hcl
#   terraform plan -var-file=environments/test.tfvars
#
# State lives in an S3 bucket with DynamoDB locking (backend.hcl, not in git).

terraform {
  # 1.9: variable validations that refer to other variables (psp_mode).
  required_version = ">= 1.9.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.80"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }

  backend "s3" {}
}

provider "aws" {
  region = var.region

  default_tags {
    tags = {
      Project     = "h5i-pay"
      Environment = var.environment
      ManagedBy   = "terraform"
      # Test mode / synthetic funds only until the M5 risk review signs off.
      FundsMode = var.psp_mode
    }
  }
}
