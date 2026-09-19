//! One-time action tokens: short-lived JWTs bound to a single operation (login, join,
//! kick, mute, unmute) whose `jti` may be consumed exactly once cluster-wide.
//!
//! Consumption is an atomic `SET NX EX` in Redis when it is configured; without Redis a
//! per-node registry provides the same guarantee for a single node.

use crate::redis_store::RedisStore;
use aurix_auth::{ActionTokenSpec, JwtService, ValidatedActionToken};
use aurix_common::config::AuthConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A token that passed signature/expiry checks but has not been consumed yet.
#[derive(Debug, Clone)]
pub struct PendingClaim {
    pub app_id: AppId,
    pub jti: String,
    pub exp: i64,
}

pub struct ActionTokenService {
    jwt: Arc<JwtService>,
    redis: Option<Arc<RedisStore>>,
    local: DashMap<String, Instant>,
    default_ttl: i64,
    max_ttl: i64,
    required: bool,
}

impl ActionTokenService {
    pub fn new(jwt: Arc<JwtService>, redis: Option<Arc<RedisStore>>, cfg: &AuthConfig) -> Self {
        Self {
            jwt,
            redis,
            local: DashMap::new(),
            default_ttl: cfg.action_token_ttl_secs,
            max_ttl: cfg.action_token_max_ttl_secs,
            required: cfg.require_action_tokens,
        }
    }

    /// Whether session JWTs are refused for WebSocket login / channel join.
    pub fn required(&self) -> bool {
        self.required
    }

    pub fn default_ttl(&self) -> i64 {
        self.default_ttl
    }

    /// Clamps a requested TTL into `1..=max_ttl`, falling back to the default.
    pub fn resolve_ttl(&self, requested: Option<i64>) -> Result<i64> {
        match requested {
            None => Ok(self.default_ttl),
            Some(t) if t >= 1 && t <= self.max_ttl => Ok(t),
            Some(_) => Err(AurixError::Validation(format!(
                "ttl_secs must be within 1..={}",
                self.max_ttl
            ))),
        }
    }

    /// Mints a token; returns `(token, jti, exp)`.
    pub fn mint(&self, spec: &ActionTokenSpec) -> Result<(String, String, i64)> {
        self.jwt.generate_action_token(spec)
    }

    /// Verifies signature/expiry and that the token authorises `expected`, without
    /// consuming it. Follow up with [`consume`](Self::consume) once the operation is
    /// about to be performed.
    pub fn verify(&self, token: &str, expected: &[ActionKind]) -> Result<ValidatedActionToken> {
        let v = self.jwt.validate_action_token(token)?;
        if !expected.contains(&v.action) {
            return Err(AurixError::AuthorizationDenied(format!(
                "Action token authorises '{}', not this operation",
                v.action
            )));
        }
        Ok(v)
    }

    pub fn pending(v: &ValidatedActionToken) -> PendingClaim {
        PendingClaim {
            app_id: v.app_id,
            jti: v.jti.clone(),
            exp: v.exp,
        }
    }

    /// Claims the token's `jti`. `Err(TokenReused)` when it was already consumed.
    /// The claim lives as long as the token could still validate (exp + leeway) so a
    /// replay can never slip in after the record expires.
    pub async fn consume(&self, claim: &PendingClaim) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let ttl = (claim.exp - now).max(1) as u64 + 60;
        let key = format!("action:{}:{}", claim.app_id, claim.jti);
        let fresh = match &self.redis {
            Some(redis) => redis.claim_once(&key, ttl).await?,
            None => self.claim_local(&key, ttl),
        };
        if fresh {
            Ok(())
        } else {
            Err(AurixError::TokenReused)
        }
    }

    fn claim_local(&self, key: &str, ttl_secs: u64) -> bool {
        let now = Instant::now();
        if self.local.len() > 4096 {
            self.local.retain(|_, until| *until > now);
        }
        match self.local.entry(key.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut e) if *e.get() <= now => {
                e.insert(now + Duration::from_secs(ttl_secs));
                true
            }
            dashmap::mapref::entry::Entry::Occupied(_) => false,
            dashmap::mapref::entry::Entry::Vacant(e) => {
                e.insert(now + Duration::from_secs(ttl_secs));
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(required: bool) -> ActionTokenService {
        let cfg = AuthConfig {
            jwt_secret: "unit-test-secret-0123456789abcdef0123456789".into(),
            require_action_tokens: required,
            ..Default::default()
        };
        let jwt = Arc::new(JwtService::new(&cfg).unwrap());
        ActionTokenService::new(jwt, None, &cfg)
    }

    fn spec(action: ActionKind) -> ActionTokenSpec {
        ActionTokenSpec {
            action,
            user_id: UserId::new(),
            app_id: AppId::new(),
            display_name: "Alice".into(),
            channel_id: Some(ChannelId::new()),
            target_user_id: Some(UserId::new()),
            speak: true,
            receive: true,
            moderate: false,
            ad_hoc: None,
            metadata: None,
            ttl_secs: 90,
        }
    }

    #[tokio::test]
    async fn first_use_succeeds_second_is_rejected() {
        let s = svc(false);
        let (tok, _, _) = s.mint(&spec(ActionKind::Kick)).unwrap();
        let v = s.verify(&tok, &[ActionKind::Kick]).unwrap();
        let claim = ActionTokenService::pending(&v);
        s.consume(&claim).await.unwrap();
        assert!(matches!(
            s.consume(&claim).await,
            Err(AurixError::TokenReused)
        ));
        // A different jti in the same tenant is unaffected.
        let (tok2, _, _) = s.mint(&spec(ActionKind::Kick)).unwrap();
        let v2 = s.verify(&tok2, &[ActionKind::Kick]).unwrap();
        s.consume(&ActionTokenService::pending(&v2)).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_double_use_permits_exactly_one_claim() {
        let s = Arc::new(svc(false));
        let (tok, _, _) = s.mint(&spec(ActionKind::Join)).unwrap();
        let v = s.verify(&tok, &[ActionKind::Join]).unwrap();
        let claim = ActionTokenService::pending(&v);
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let s = s.clone();
            let c = claim.clone();
            tasks.push(tokio::spawn(async move { s.consume(&c).await.is_ok() }));
        }
        let mut wins = 0;
        for t in tasks {
            if t.await.unwrap() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1);
    }

    #[test]
    fn wrong_action_is_refused_before_consumption() {
        let s = svc(false);
        let (tok, _, _) = s.mint(&spec(ActionKind::Mute)).unwrap();
        assert!(matches!(
            s.verify(&tok, &[ActionKind::Kick]),
            Err(AurixError::AuthorizationDenied(_))
        ));
        assert!(s
            .verify(&tok, &[ActionKind::Mute, ActionKind::Unmute])
            .is_ok());
    }

    #[test]
    fn ttl_is_clamped_to_configured_bounds() {
        let s = svc(true);
        assert!(s.required());
        assert_eq!(s.resolve_ttl(None).unwrap(), 90);
        assert_eq!(s.resolve_ttl(Some(5)).unwrap(), 5);
        assert!(s.resolve_ttl(Some(0)).is_err());
        assert!(s.resolve_ttl(Some(601)).is_err());
    }

    #[test]
    fn local_registry_prunes_expired_claims() {
        let s = svc(false);
        assert!(s.claim_local("k", 1));
        assert!(!s.claim_local("k", 1));
        for i in 0..5000 {
            s.local
                .insert(format!("old{i}"), Instant::now() - Duration::from_secs(1));
        }
        assert!(s.claim_local("k2", 1));
        assert!(s.local.len() < 100);
    }
}
