# A test-mode environment: Mock PSP (or Stripe test keys), synthetic funds,
# small instances, one NAT gateway. Fill in the certificate and image tags.
environment         = "test"
region              = "us-east-1"
psp_mode            = "mock"
api_image_tag       = "REPLACE_WITH_GIT_SHA"
worker_image_tag    = "REPLACE_WITH_GIT_SHA"
acm_certificate_arn = "arn:aws:acm:us-east-1:111122223333:certificate/REPLACE"
single_nat_gateway  = true
db_instance_class   = "db.t4g.medium"
db_multi_az         = true
gpu_instance_types  = ["g4dn.xlarge", "g5.xlarge"]
gpu_max_vcpus       = 8
gpu_use_spot        = false
alert_email         = ""
