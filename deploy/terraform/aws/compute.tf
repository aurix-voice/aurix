resource "aws_eip" "node" {
  count  = var.node_count
  domain = "vpc"

  tags = { Name = local.node_hosts[count.index] }
}

resource "aws_instance" "node" {
  count                       = var.node_count
  ami                         = var.ami_id != null ? var.ami_id : data.aws_ami.debian[0].id
  instance_type               = var.instance_type
  subnet_id                   = local.subnet_ids[count.index % length(local.subnet_ids)]
  vpc_security_group_ids      = [aws_security_group.node.id]
  iam_instance_profile        = aws_iam_instance_profile.node.name
  key_name                    = var.key_name
  associate_public_ip_address = false
  user_data_replace_on_change = true

  metadata_options {
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type = "gp3"
    volume_size = 40
    encrypted   = true
  }

  user_data = templatefile("${path.module}/templates/user-data.sh.tftpl", {
    aws_region  = var.aws_region
    secrets_arn = local.secrets_arn
    pool_arn    = aws_secretsmanager_secret.pool.arn
    node_host   = local.node_hosts[count.index]
    api_host    = local.api_host
    compose = templatefile("${path.module}/templates/compose.yml.tftpl", {
      image = var.image
    })
    caddyfile = templatefile("${path.module}/templates/Caddyfile.tftpl", {
      node_host  = local.node_hosts[count.index]
      api_host   = local.api_host
      acme_email = var.acme_email
    })
    env = merge({
      AURIX__SERVER__ENVIRONMENT      = "production"
      AURIX__SERVER__HOST             = "127.0.0.1"
      AURIX__SERVER__NODE_ID          = uuidv5("dns", local.node_hosts[count.index])
      AURIX__SERVER__REGION           = var.aurix_region
      AURIX__SERVER__EXTERNAL_URL     = "https://${local.node_hosts[count.index]}"
      AURIX__SERVER__EXTERNAL_WS_URL  = "wss://${local.node_hosts[count.index]}/ws"
      AURIX__SERVER__CORS_ORIGINS     = join(",", var.cors_origins)
      AURIX__SERVER__TRUSTED_PROXIES  = "127.0.0.1/32"
      AURIX__DATABASE__RUN_MIGRATIONS = count.index == 0 ? "true" : "false"
      AURIX__MEDIA__EXTERNAL_IP       = aws_eip.node[count.index].public_ip
      AURIX__TURN__ENABLED            = var.turn_enabled ? "true" : "false"
      AURIX__TURN__EXTERNAL_IP        = aws_eip.node[count.index].public_ip
      AURIX__TURN__REALM              = var.domain
      AURIX__TURN__MIN_PORT           = tostring(var.turn_port_range.min)
      AURIX__TURN__MAX_PORT           = tostring(var.turn_port_range.max)
      AURIX__METRICS__PORT            = "4040"
      AURIX__TRACING__LOG_LEVEL       = var.log_level
      AURIX__TRACING__LOG_FORMAT      = "json"
      }, var.location == null ? {} : {
      AURIX__SERVER__LOCATION__LATITUDE  = tostring(var.location.latitude)
      AURIX__SERVER__LOCATION__LONGITUDE = tostring(var.location.longitude)
    }, var.extra_env)
  })

  tags = { Name = local.node_hosts[count.index] }

  lifecycle {
    ignore_changes = [ami]
  }
}

resource "aws_eip_association" "node" {
  count         = var.node_count
  instance_id   = aws_instance.node[count.index].id
  allocation_id = aws_eip.node[count.index].id
}
