resource "aws_route53_record" "node" {
  count   = var.node_count
  zone_id = var.route53_zone_id
  name    = local.node_hosts[count.index]
  type    = "A"
  ttl     = 60
  records = [aws_eip.node[count.index].public_ip]
}

# Shared entry point for backend REST calls: DNS round-robin over the pool. Players are
# handed a per-node `wss://` URL by region discovery, so they never depend on this name.
resource "aws_route53_record" "api" {
  zone_id = var.route53_zone_id
  name    = local.api_host
  type    = "A"
  ttl     = 60
  records = aws_eip.node[*].public_ip
}
