//! Fleet-wide abuse limits: one token bucket per `(scope, subject)` shared by every node
//! through Redis, with node-local buckets when Redis is off or unreachable.
//!
//! Callers pick a [`Scope`] and a subject (client IP, user, API key). The decision is the same
//! on every node because the bucket state lives in Redis and is advanced with the Redis clock;
//! a node that loses Redis keeps enforcing locally (`fail_closed = false`) or refuses
//! (`fail_closed = true`) until the connection is back.

use crate::redis_store::RedisStore;
use aurix_common::config::RateLimitConfig;
use aurix_common::rate_limit::RateLimiter;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// What a bucket protects; also the metrics label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// REST requests from one client IP.
    ApiIp,
    /// REST requests through one API key (its own `rate_limit`).
    ApiKey,
    /// Control-WebSocket connections by one user.
    Connect,
    /// Channel joins by one user.
    Join,
    /// Block/unblock toggles by one user.
    Block,
    /// Player reports by one user.
    Report,
    /// Operator login/setup attempts from one client IP.
    AdminLogin,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiIp => "api_ip",
            Self::ApiKey => "api_key",
            Self::Connect => "connect",
            Self::Join => "join",
            Self::Block => "block",
            Self::Report => "report",
            Self::AdminLogin => "admin_login",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sustained rate and burst of one bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limit {
    pub per_second: f64,
    pub burst: f64,
}

impl Limit {
    pub fn per_second(rate: u32, burst: u32) -> Self {
        Self {
            per_second: rate as f64,
            burst: burst as f64,
        }
    }

    /// `n` events per minute, the whole minute's worth available as a burst.
    pub fn per_minute(n: u32) -> Self {
        Self {
            per_second: n as f64 / 60.0,
            burst: n as f64,
        }
    }
}

/// Where the decision was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Fleet,
    Local,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fleet => "fleet",
            Self::Local => "local",
        }
    }
}

/// A rejected request: how long to wait before it may be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttled {
    pub scope: Scope,
    pub retry_after: Duration,
    pub backend: Backend,
}

impl Throttled {
    /// `Retry-After` header value: whole seconds, at least 1.
    pub fn retry_after_secs(&self) -> u64 {
        if self.retry_after == Duration::MAX {
            return 3600;
        }
        self.retry_after.as_secs_f64().ceil().max(1.0) as u64
    }
}

pub struct FleetLimiter {
    cfg: RateLimitConfig,
    redis: Option<Arc<RedisStore>>,
    local: RateLimiter,
}

impl FleetLimiter {
    pub fn new(cfg: RateLimitConfig, redis: Option<Arc<RedisStore>>) -> Self {
        let local = RateLimiter::new(cfg.requests_per_second, cfg.burst_size);
        let redis = if cfg.fleet { redis } else { None };
        Self { cfg, redis, local }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Periodic eviction of idle node-local buckets.
    pub fn start_cleanup_task(&self) {
        self.local.start_cleanup_task();
    }

    pub fn config(&self) -> &RateLimitConfig {
        &self.cfg
    }

    /// The configured limit for a scope; `ApiKey` is per key and passed by the caller.
    pub fn limit(&self, scope: Scope) -> Limit {
        let c = &self.cfg;
        match scope {
            Scope::ApiIp => Limit::per_second(c.requests_per_second, c.burst_size),
            Scope::ApiKey => Limit::per_second(c.requests_per_second, c.burst_size),
            Scope::Connect => Limit::per_minute(c.connects_per_minute),
            Scope::Join => Limit::per_minute(c.channel_joins_per_minute),
            Scope::Block => Limit::per_minute(c.block_changes_per_minute),
            Scope::Report => Limit::per_minute(c.reports_per_minute),
            Scope::AdminLogin => Limit::per_minute(c.admin_login_per_minute),
        }
    }

    /// Takes one token for `subject` in `scope` with the configured limit.
    pub async fn check(&self, scope: Scope, subject: &str) -> Result<(), Throttled> {
        self.check_with(scope, subject, self.limit(scope), 1.0)
            .await
    }

    /// Takes `cost` tokens for `subject` in `scope` with an explicit limit.
    pub async fn check_with(
        &self,
        scope: Scope,
        subject: &str,
        limit: Limit,
        cost: f64,
    ) -> Result<(), Throttled> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let key = format!("{scope}:{subject}");
        let backend = if let Some(redis) = &self.redis {
            match redis
                .take_tokens(&key, limit.per_second, limit.burst, cost)
                .await
            {
                Ok(None) => return Ok(()),
                Ok(Some(wait)) => {
                    return Err(self.hit(scope, wait, Backend::Fleet));
                }
                Err(e) => {
                    aurix_metrics::RATE_LIMIT_BACKEND_ERRORS.inc();
                    tracing::warn!(scope = %scope, error = %e, "fleet rate limiter unavailable");
                    if self.cfg.fail_closed {
                        return Err(self.hit(scope, Duration::from_secs(1), Backend::Fleet));
                    }
                    Backend::Local
                }
            }
        } else {
            Backend::Local
        };
        match self
            .local
            .try_acquire(&key, limit.per_second, limit.burst, cost)
        {
            Ok(()) => Ok(()),
            Err(wait) => Err(self.hit(scope, wait, backend)),
        }
    }

    fn hit(&self, scope: Scope, retry_after: Duration, backend: Backend) -> Throttled {
        aurix_metrics::RATE_LIMIT_HITS.inc();
        aurix_metrics::RATE_LIMIT_HITS_BY_SCOPE
            .with_label_values(&[scope.as_str(), backend.as_str()])
            .inc();
        Throttled {
            scope,
            retry_after,
            backend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(enabled: bool) -> FleetLimiter {
        FleetLimiter::new(
            RateLimitConfig {
                enabled,
                reports_per_minute: 2,
                ..RateLimitConfig::default()
            },
            None,
        )
    }

    #[tokio::test]
    async fn disabled_admits_everything() {
        let l = limiter(false);
        for _ in 0..1000 {
            assert!(l.check(Scope::Report, "u").await.is_ok());
        }
    }

    #[tokio::test]
    async fn per_minute_scopes_burst_then_throttle_per_subject() {
        let l = limiter(true);
        assert!(l.check(Scope::Report, "alice").await.is_ok());
        assert!(l.check(Scope::Report, "alice").await.is_ok());
        let t = l.check(Scope::Report, "alice").await.unwrap_err();
        assert_eq!(t.scope, Scope::Report);
        assert_eq!(t.backend, Backend::Local);
        // 2/min → one token every 30 s
        assert!(
            t.retry_after > Duration::from_secs(29) && t.retry_after <= Duration::from_secs(30)
        );
        assert_eq!(t.retry_after_secs(), 30);
        assert!(l.check(Scope::Report, "bob").await.is_ok());
        // scopes do not share buckets
        assert!(l.check(Scope::Block, "alice").await.is_ok());
    }

    #[tokio::test]
    async fn explicit_limits_and_costs() {
        let l = limiter(true);
        let lim = Limit::per_minute(10);
        assert!(l.check_with(Scope::ApiKey, "k", lim, 9.0).await.is_ok());
        assert!(l.check_with(Scope::ApiKey, "k", lim, 2.0).await.is_err());
        assert!(l.check_with(Scope::ApiKey, "k", lim, 1.0).await.is_ok());
        let never = l
            .check_with(
                Scope::ApiKey,
                "z",
                Limit {
                    per_second: 0.0,
                    burst: 1.0,
                },
                2.0,
            )
            .await
            .unwrap_err();
        assert_eq!(never.retry_after_secs(), 3600);
    }
}
