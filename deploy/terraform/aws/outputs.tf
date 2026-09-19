output "api_url" {
  description = "Round-robin REST entry point for this pool."
  value       = "https://${local.api_host}"
}

output "node_urls" {
  description = "Per-node HTTPS URLs (WebSocket at /ws) advertised by region discovery."
  value       = [for h in local.node_hosts : "https://${h}"]
}

output "node_public_ips" {
  value = aws_eip.node[*].public_ip
}

output "secrets_arn" {
  description = "Pass as `secrets_arn` to the other regions so the fleet shares one cascade secret."
  value       = local.secrets_arn
}

output "database_url" {
  description = "Pass as `database_url` to the other regions (all regions share one database)."
  value       = local.database_url
  sensitive   = true
}

output "redis_url" {
  description = "Pass as `redis_url` to the other regions."
  value       = local.redis_url
  sensitive   = true
}

output "admin_bootstrap_hint" {
  value = "aws secretsmanager get-secret-value --secret-id ${local.secrets_arn} --query SecretString --output text | jq -r .admin_bootstrap_token   # then POST ${"https://${local.api_host}"}/admin/setup with X-Bootstrap-Token"
}
