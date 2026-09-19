-- Tenant webhook subscriptions (POST /v1/webhooks) and their delivery log. `secret` is the
-- HMAC-SHA256 signing key; it is returned to the caller only on create/rotate. Deliveries are
-- enqueued by the node that published the event and drained by any node (row lock + lease);
-- finished rows are swept by webhooks.retention_hours.
CREATE TABLE webhook_subscriptions (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    secret TEXT NOT NULL,
    events TEXT[] NOT NULL,
    description TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    consecutive_failures INT NOT NULL DEFAULT 0,
    last_delivery_at TIMESTAMPTZ,
    last_status SMALLINT,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_webhook_subscriptions_app ON webhook_subscriptions(app_id, created_at);

CREATE TABLE webhook_deliveries (
    id UUID PRIMARY KEY,
    subscription_id UUID NOT NULL REFERENCES webhook_subscriptions(id) ON DELETE CASCADE,
    app_id UUID NOT NULL,
    event_id UUID NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    attempts INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    leased_until TIMESTAMPTZ,
    last_status SMALLINT,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    delivered_at TIMESTAMPTZ,
    CHECK (status IN ('pending', 'delivered', 'failed'))
);
CREATE INDEX idx_webhook_deliveries_due ON webhook_deliveries(next_attempt_at)
    WHERE status = 'pending';
CREATE INDEX idx_webhook_deliveries_subscription
    ON webhook_deliveries(subscription_id, created_at DESC);
CREATE INDEX idx_webhook_deliveries_created ON webhook_deliveries(created_at)
    WHERE status <> 'pending';
