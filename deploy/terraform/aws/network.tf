resource "aws_vpc" "this" {
  count                = local.create_vpc ? 1 : 0
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = { Name = local.name }
}

resource "aws_internet_gateway" "this" {
  count  = local.create_vpc ? 1 : 0
  vpc_id = aws_vpc.this[0].id

  tags = { Name = local.name }
}

resource "aws_subnet" "public" {
  count                   = local.create_vpc ? min(3, length(data.aws_availability_zones.available.names)) : 0
  vpc_id                  = aws_vpc.this[0].id
  cidr_block              = cidrsubnet(var.vpc_cidr, 4, count.index)
  availability_zone       = data.aws_availability_zones.available.names[count.index]
  map_public_ip_on_launch = false

  tags = { Name = "${local.name}-public-${count.index}" }
}

resource "aws_route_table" "public" {
  count  = local.create_vpc ? 1 : 0
  vpc_id = aws_vpc.this[0].id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this[0].id
  }

  tags = { Name = "${local.name}-public" }
}

resource "aws_route_table_association" "public" {
  count          = local.create_vpc ? length(aws_subnet.public) : 0
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public[0].id
}

resource "aws_security_group" "node" {
  name        = "${local.name}-node"
  description = "Aurix node: HTTPS (Caddy), media UDP, cascade, TURN"
  vpc_id      = local.vpc_id

  tags = { Name = "${local.name}-node" }
}

resource "aws_vpc_security_group_ingress_rule" "http" {
  security_group_id = aws_security_group.node.id
  description       = "ACME HTTP-01 challenge and redirect to HTTPS"
  ip_protocol       = "tcp"
  from_port         = 80
  to_port           = 80
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_ingress_rule" "https" {
  security_group_id = aws_security_group.node.id
  description       = "REST API and WebSocket through Caddy"
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_ingress_rule" "media" {
  security_group_id = aws_security_group.node.id
  description       = "Native AURX media and WebRTC ICE"
  ip_protocol       = "udp"
  from_port         = 10000
  to_port           = 10000
  cidr_ipv4         = "0.0.0.0/0"
}

# Cascade traffic is node-to-node only. Nodes in *other* regions have their own security
# groups, so the rule allows the cascade port from anywhere; the packets themselves are
# authenticated with the shared cascade secret and unknown sources are dropped.
resource "aws_vpc_security_group_ingress_rule" "cascade" {
  security_group_id = aws_security_group.node.id
  description       = "SFU-to-SFU cascade relay"
  ip_protocol       = "udp"
  from_port         = 10001
  to_port           = 10001
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_ingress_rule" "turn" {
  for_each          = var.turn_enabled ? toset(["udp", "tcp"]) : toset([])
  security_group_id = aws_security_group.node.id
  description       = "TURN/STUN ${each.key}"
  ip_protocol       = each.key
  from_port         = 3478
  to_port           = 3478
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_ingress_rule" "turn_relay" {
  count             = var.turn_enabled ? 1 : 0
  security_group_id = aws_security_group.node.id
  description       = "TURN relay allocations"
  ip_protocol       = "udp"
  from_port         = var.turn_port_range.min
  to_port           = var.turn_port_range.max
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_ingress_rule" "ssh" {
  for_each          = toset(var.ssh_cidrs)
  security_group_id = aws_security_group.node.id
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = each.value
}

resource "aws_vpc_security_group_egress_rule" "all" {
  security_group_id = aws_security_group.node.id
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_security_group" "data" {
  count       = local.create_db || local.create_redis ? 1 : 0
  name        = "${local.name}-data"
  description = "PostgreSQL and Redis, reachable from the Aurix nodes only"
  vpc_id      = local.vpc_id

  tags = { Name = "${local.name}-data" }
}

resource "aws_vpc_security_group_ingress_rule" "postgres" {
  count                        = local.create_db ? 1 : 0
  security_group_id            = aws_security_group.data[0].id
  ip_protocol                  = "tcp"
  from_port                    = 5432
  to_port                      = 5432
  referenced_security_group_id = aws_security_group.node.id
}

resource "aws_vpc_security_group_ingress_rule" "redis" {
  count                        = local.create_redis ? 1 : 0
  security_group_id            = aws_security_group.data[0].id
  ip_protocol                  = "tcp"
  from_port                    = 6379
  to_port                      = 6379
  referenced_security_group_id = aws_security_group.node.id
}
