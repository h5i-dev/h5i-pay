# G3: an optional Cognito user pool as the identity provider. The API
# accepts its access tokens (RS256) next to the HMAC tokens; the claims it
# needs (h5i_tenant, h5i_operator or h5i_user) are added by a pre-token-
# generation Lambda (cognito_pre_token_lambda_arn), which is the operator
# directory's own code and not part of this repository.

locals {
  # The API's scopes (payment_api::auth::SCOPE_NAMES).
  api_scopes = ["payments:read", "payments:write", "refunds:write", "ledger:read", "ledger:write", "reconcile:read", "recover", "tokens:revoke"]
  cognito    = var.create_cognito_user_pool
  oidc_issuer = var.oidc_issuer_url != "" ? var.oidc_issuer_url : (
    local.cognito ? "https://cognito-idp.${var.region}.amazonaws.com/${aws_cognito_user_pool.main[0].id}" : ""
  )
  oidc_audience = var.oidc_audience != "" ? var.oidc_audience : (
    local.cognito ? aws_cognito_user_pool_client.operators[0].id : ""
  )
  oidc_scope_prefix = var.oidc_scope_prefix != "" ? var.oidc_scope_prefix : (local.cognito ? "h5i-pay/" : "")
  oidc_env = local.oidc_issuer == "" ? [] : [
    { name = "H5I_PAY_OIDC_ISSUER", value = local.oidc_issuer },
    { name = "H5I_PAY_OIDC_AUDIENCE", value = local.oidc_audience },
    { name = "H5I_PAY_OIDC_SCOPE_PREFIX", value = local.oidc_scope_prefix },
  ]
}

resource "aws_cognito_user_pool" "main" {
  count               = local.cognito ? 1 : 0
  name                = local.name
  deletion_protection = "ACTIVE"
  mfa_configuration   = "ON"
  # No self-service sign-up: operators are created by an administrator.
  admin_create_user_config {
    allow_admin_create_user_only = true
  }
  software_token_mfa_configuration {
    enabled = true
  }
  password_policy {
    minimum_length                   = 14
    require_lowercase                = true
    require_uppercase                = true
    require_numbers                  = true
    require_symbols                  = true
    temporary_password_validity_days = 1
  }
  user_pool_add_ons {
    advanced_security_mode = "ENFORCED"
  }
  dynamic "lambda_config" {
    for_each = var.cognito_pre_token_lambda_arn == "" ? [] : [1]
    content {
      pre_token_generation_config {
        lambda_arn     = var.cognito_pre_token_lambda_arn
        lambda_version = "V2_0"
      }
    }
  }
}

resource "aws_cognito_resource_server" "api" {
  count        = local.cognito ? 1 : 0
  user_pool_id = aws_cognito_user_pool.main[0].id
  identifier   = "h5i-pay"
  name         = "h5i-pay API"
  dynamic "scope" {
    for_each = local.api_scopes
    content {
      scope_name        = scope.value
      scope_description = "h5i-pay ${scope.value}"
    }
  }
}

# Operators sign in through the hosted UI (authorization code with PKCE)
# and get access tokens of at most one hour, within the API's 12-hour cap.
resource "aws_cognito_user_pool_client" "operators" {
  count                                = local.cognito ? 1 : 0
  name                                 = "${local.name}-operators"
  user_pool_id                         = aws_cognito_user_pool.main[0].id
  generate_secret                      = false
  allowed_oauth_flows_user_pool_client = true
  allowed_oauth_flows                  = ["code"]
  allowed_oauth_scopes                 = [for s in local.api_scopes : "h5i-pay/${s}"]
  callback_urls                        = ["https://${var.api_domain_name}/oauth/callback"]
  supported_identity_providers         = ["COGNITO"]
  access_token_validity                = 60
  id_token_validity                    = 60
  refresh_token_validity               = 12
  token_validity_units {
    access_token  = "minutes"
    id_token      = "minutes"
    refresh_token = "hours"
  }
  enable_token_revocation       = true
  prevent_user_existence_errors = "ENABLED"
  depends_on                    = [aws_cognito_resource_server.api]
}
