resource "random_password" "db" {
  count   = local.create_db ? 1 : 0
  length  = 32
  special = false
}

resource "aws_db_subnet_group" "this" {
  count      = local.create_db ? 1 : 0
  name       = local.name
  subnet_ids = local.subnet_ids
}

resource "aws_db_instance" "this" {
  count                        = local.create_db ? 1 : 0
  identifier                   = local.name
  engine                       = "postgres"
  engine_version               = "16"
  instance_class               = var.db_instance_class
  allocated_storage            = var.db_allocated_storage
  max_allocated_storage        = var.db_allocated_storage * 4
  storage_type                 = "gp3"
  storage_encrypted            = true
  db_name                      = "aurix"
  username                     = "aurix"
  password                     = random_password.db[0].result
  db_subnet_group_name         = aws_db_subnet_group.this[0].name
  vpc_security_group_ids       = [aws_security_group.data[0].id]
  publicly_accessible          = false
  multi_az                     = true
  backup_retention_period      = 7
  deletion_protection          = true
  skip_final_snapshot          = false
  final_snapshot_identifier    = "${local.name}-final"
  auto_minor_version_upgrade   = true
  performance_insights_enabled = true
  copy_tags_to_snapshot        = true
}

resource "aws_elasticache_subnet_group" "this" {
  count      = local.create_redis ? 1 : 0
  name       = local.name
  subnet_ids = local.subnet_ids
}

resource "aws_elasticache_replication_group" "this" {
  count                      = local.create_redis ? 1 : 0
  replication_group_id       = local.name
  description                = "Aurix cross-node event bus, token claims and rate limits"
  engine                     = "redis"
  engine_version             = "7.1"
  node_type                  = var.redis_node_type
  num_cache_clusters         = 2
  automatic_failover_enabled = true
  multi_az_enabled           = true
  port                       = 6379
  subnet_group_name          = aws_elasticache_subnet_group.this[0].name
  security_group_ids         = [aws_security_group.data[0].id]
  at_rest_encryption_enabled = true
  transit_encryption_enabled = true
  apply_immediately          = true
}
