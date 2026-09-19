# Generated once per fleet: the cascade secret must be identical on every node of every
# region, so additional regions pass `secrets_arn` instead of generating their own.
resource "random_password" "jwt" {
  count   = local.create_secret ? 1 : 0
  length  = 64
  special = false
}

resource "random_password" "turn" {
  count   = local.create_secret ? 1 : 0
  length  = 64
  special = false
}

resource "random_password" "cascade" {
  count   = local.create_secret ? 1 : 0
  length  = 48
  special = false
}

resource "random_password" "bootstrap" {
  count   = local.create_secret ? 1 : 0
  length  = 48
  special = false
}

resource "aws_secretsmanager_secret" "this" {
  count                   = local.create_secret ? 1 : 0
  name                    = "${var.project}/aurix"
  description             = "Aurix fleet secrets (shared by every region)"
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "this" {
  count     = local.create_secret ? 1 : 0
  secret_id = aws_secretsmanager_secret.this[0].id
  secret_string = jsonencode({
    jwt_secret            = random_password.jwt[0].result
    turn_secret           = random_password.turn[0].result
    cascade_secret        = random_password.cascade[0].result
    admin_bootstrap_token = random_password.bootstrap[0].result
  })
}

# Per-pool secret with the data-store URLs (they embed the database password).
resource "aws_secretsmanager_secret" "pool" {
  name                    = "${var.project}/aurix/${var.aurix_region}"
  description             = "Aurix ${var.aurix_region} pool: database and Redis URLs"
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "pool" {
  secret_id = aws_secretsmanager_secret.pool.id
  secret_string = jsonencode({
    database_url = local.database_url
    redis_url    = local.redis_url
  })
}

data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "node" {
  name               = "${local.name}-node"
  assume_role_policy = data.aws_iam_policy_document.assume.json
}

data "aws_iam_policy_document" "node" {
  statement {
    sid       = "ReadSecrets"
    actions   = ["secretsmanager:GetSecretValue"]
    resources = [local.secrets_arn, aws_secretsmanager_secret.pool.arn]
  }
}

resource "aws_iam_role_policy" "node" {
  name   = "secrets"
  role   = aws_iam_role.node.id
  policy = data.aws_iam_policy_document.node.json
}

# Session Manager instead of SSH.
resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.node.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "node" {
  name = "${local.name}-node"
  role = aws_iam_role.node.name
}
