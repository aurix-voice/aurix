locals {
  name          = "${var.project}-${replace(var.aurix_region, "_", "-")}"
  node_hosts    = [for i in range(var.node_count) : "${local.name}-${i}.${var.domain}"]
  api_host      = "api-${replace(var.aurix_region, "_", "-")}.${var.domain}"
  create_vpc    = var.vpc_id == null
  create_db     = var.database_url == null
  create_redis  = var.redis_url == null
  create_secret = var.secrets_arn == null
  vpc_id        = local.create_vpc ? aws_vpc.this[0].id : var.vpc_id
  subnet_ids    = local.create_vpc ? aws_subnet.public[*].id : var.subnet_ids
  secrets_arn   = local.create_secret ? aws_secretsmanager_secret.this[0].arn : var.secrets_arn
  arm           = can(regex("^(a1|t4g|c6g|c7g|m6g|m7g|r6g|r7g|c6gn|c7gn|im4gn|is4gen)\\.", var.instance_type))

  database_url = local.create_db ? "postgres://${aws_db_instance.this[0].username}:${urlencode(random_password.db[0].result)}@${aws_db_instance.this[0].address}:${aws_db_instance.this[0].port}/${aws_db_instance.this[0].db_name}" : var.database_url
  redis_url    = local.create_redis ? "rediss://${aws_elasticache_replication_group.this[0].primary_endpoint_address}:6379" : var.redis_url
}

data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_ami" "debian" {
  count       = var.ami_id == null ? 1 : 0
  most_recent = true
  owners      = ["136693071363"] # Debian

  filter {
    name   = "name"
    values = ["debian-12-${local.arm ? "arm64" : "amd64"}-*"]
  }
  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}
