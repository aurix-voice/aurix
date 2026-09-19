variable "project" {
  description = "Name prefix for every resource."
  type        = string
  default     = "aurix"
}

variable "aws_region" {
  description = "AWS region to deploy this node pool into."
  type        = string
}

variable "aurix_region" {
  description = "Aurix region label of this pool (server.region): us_east, us_west, eu_west, eu_central, asia_pacific, south_america, australia, middle_east, africa."
  type        = string

  validation {
    condition     = contains(["us_east", "us_west", "eu_west", "eu_central", "asia_pacific", "south_america", "australia", "middle_east", "africa"], var.aurix_region)
    error_message = "aurix_region must be one of the Aurix region labels."
  }
}

variable "location" {
  description = "Approximate WGS-84 coordinates of the pool, advertised for distance-based region ordering. null skips it."
  type = object({
    latitude  = number
    longitude = number
  })
  default = null
}

variable "node_count" {
  description = "Number of Aurix nodes (EC2 instances, one Elastic IP each) in this pool."
  type        = number
  default     = 2

  validation {
    condition     = var.node_count >= 1 && var.node_count <= 50
    error_message = "node_count must be between 1 and 50."
  }
}

variable "instance_type" {
  description = "EC2 instance type for the nodes. The SFU is CPU/network bound; c-family or network-optimised types work best."
  type        = string
  default     = "c6i.large"
}

variable "ami_id" {
  description = "AMI for the nodes. null selects the latest official Debian 12 arm64/amd64 image matching the instance type."
  type        = string
  default     = null
}

variable "image" {
  description = "Aurix server container image (build from the repository root and push to your registry)."
  type        = string
}

variable "domain" {
  description = "Public DNS zone the pool lives in, e.g. voice.example.com. Nodes become <project>-<aurix_region>-<n>.<domain>, the pool api-<aurix_region>.<domain>."
  type        = string
}

variable "route53_zone_id" {
  description = "Hosted zone id of `domain`."
  type        = string
}

variable "cors_origins" {
  description = "Browser origins allowed to call the API (server.cors_origins)."
  type        = list(string)
}

variable "vpc_id" {
  description = "Existing VPC to deploy into. null creates a small VPC with one public subnet per AZ."
  type        = string
  default     = null
}

variable "subnet_ids" {
  description = "Public subnets (with an internet gateway route) for the nodes and the data stores when vpc_id is set."
  type        = list(string)
  default     = []
}

variable "vpc_cidr" {
  description = "CIDR of the VPC created when vpc_id is null."
  type        = string
  default     = "10.60.0.0/16"
}

variable "ssh_cidrs" {
  description = "CIDRs allowed to SSH to the nodes. Empty disables SSH ingress (use SSM Session Manager)."
  type        = list(string)
  default     = []
}

variable "key_name" {
  description = "EC2 key pair name for SSH; null for none."
  type        = string
  default     = null
}

variable "turn_enabled" {
  description = "Run the built-in TURN server on every node (opens 3478 and the relay range)."
  type        = bool
  default     = true
}

variable "turn_port_range" {
  description = "UDP relay port range for TURN."
  type = object({
    min = number
    max = number
  })
  default = {
    min = 49152
    max = 65535
  }
}

variable "acme_email" {
  description = "Contact e-mail for the Let's Encrypt certificates Caddy obtains on each node."
  type        = string
}

variable "database_url" {
  description = "Existing PostgreSQL URL (postgres://user:pass@host:5432/db). null creates an RDS instance in this region. Every Aurix region must share one database."
  type        = string
  default     = null
  sensitive   = true
}

variable "redis_url" {
  description = "Existing Redis URL (redis:// or rediss://). null creates an ElastiCache replication group with TLS. Every Aurix region must share one Redis."
  type        = string
  default     = null
  sensitive   = true
}

variable "secrets_arn" {
  description = "Existing Secrets Manager secret holding the JSON keys jwt_secret, turn_secret, cascade_secret, admin_bootstrap_token. null generates them here (the first region does; the others reuse its ARN so the fleet shares one cascade secret)."
  type        = string
  default     = null
}

variable "db_instance_class" {
  type    = string
  default = "db.t4g.medium"
}

variable "db_allocated_storage" {
  type    = number
  default = 50
}

variable "redis_node_type" {
  type    = string
  default = "cache.t4g.small"
}

variable "log_level" {
  type    = string
  default = "info"
}

variable "extra_env" {
  description = "Additional AURIX__* environment variables for the server container."
  type        = map(string)
  default     = {}
}
