use crate::errors::ApiError;
use crate::state::AppState;
use aurix_common::error::AurixError;
use aurix_common::types::{AdminContext, AppId, UserId};
use aurix_db::models::ApiKeyRow;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::header::AUTHORIZATION,
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

/// Identity of a server-to-server caller authenticated with an API key.
#[derive(Clone, Debug)]
pub struct ApiKeyContext {
    pub key_id: Uuid,
    pub app_id: AppId,
    pub permissions: Arc<serde_json::Value>,
}

impl ApiKeyContext {
    fn from_row(row: &ApiKeyRow) -> Self {
        Self {
            key_id: row.id,
            app_id: AppId::from_uuid(row.app_id),
            permissions: Arc::new(row.permissions.clone()),
        }
    }

    /// Audit/moderation actor identity for actions performed with this key.
    pub fn actor(&self) -> UserId {
        UserId::from_uuid(self.key_id)
    }

    /// Permissions are a JSON array of strings (`"*"` = everything). The legacy object form
    /// `{"all": true}` is accepted as a wildcard.
    pub fn has(&self, permission: &str) -> bool {
        match &*self.permissions {
            serde_json::Value::Array(arr) => arr
                .iter()
                .any(|p| p.as_str() == Some("*") || p.as_str() == Some(permission)),
            serde_json::Value::Object(obj) => {
                obj.get("all").and_then(|v| v.as_bool()).unwrap_or(false)
            }
            _ => false,
        }
    }

    pub fn require(&self, permission: &str) -> Result<(), ApiError> {
        if self.has(permission) {
            Ok(())
        } else {
            Err(
                AurixError::AuthorizationDenied(format!("API key lacks permission '{permission}'"))
                    .into(),
            )
        }
    }
}

/// Real client IP after trusted-proxy resolution.
#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

fn resolve_client_ip(state: &AppState, request: &Request) -> IpAddr {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let headers = request.headers();
    aurix_common::net::client_ip(
        peer,
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        headers.get("x-real-ip").and_then(|v| v.to_str().ok()),
        &state.trusted_proxies,
    )
}

/// Resolves the client IP once per request and stores it as `ClientIp`.
pub async fn client_ip_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let ip = resolve_client_ip(&state, &request);
    request.extensions_mut().insert(ClientIp(ip));
    next.run(request).await
}

/// End-user JWT (issued by `POST /v1/tokens`).
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = bearer_token(request.headers())
        .ok_or_else(|| AurixError::AuthenticationFailed("Missing bearer token".into()))?;
    let validated = state.control.validate_token(token).await?;
    request.extensions_mut().insert(validated);
    Ok(next.run(request).await)
}

/// Tenant API key from `X-API-Key` (or `Authorization: Bearer aurx_...`).
pub async fn api_key_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| bearer_token(request.headers()).filter(|t| t.starts_with("aurx_")))
        .ok_or_else(|| AurixError::AuthenticationFailed("Missing API key".into()))?;
    let key_row = state.control.api_keys.validate_key(api_key).await?;
    let ctx = ApiKeyContext::from_row(&key_row);
    request.extensions_mut().insert(ctx.app_id);
    request.extensions_mut().insert(ctx);
    Ok(next.run(request).await)
}

/// Admin JWT; also confirms the account is still active.
pub async fn admin_auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = bearer_token(request.headers())
        .ok_or_else(|| AurixError::AuthenticationFailed("Missing bearer token".into()))?;
    let admin_ctx: AdminContext = state.control.authenticate_admin(token).await?;
    request.extensions_mut().insert(admin_ctx);
    Ok(next.run(request).await)
}

pub async fn rate_limit_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !state.control.config.rate_limiting.enabled {
        return Ok(next.run(request).await);
    }
    let ip = request
        .extensions()
        .get::<ClientIp>()
        .map(|c| c.0)
        .unwrap_or_else(|| resolve_client_ip(&state, &request));
    let key = format!("api:{ip}");
    let allowed = if let Some(ref redis) = state.control.redis {
        redis
            .check_rate_limit(
                &key,
                state.control.config.rate_limiting.requests_per_second,
                1,
            )
            .await
            .unwrap_or_else(|_| state.control.rate_limiter.check(&key))
    } else {
        state.control.rate_limiter.check(&key)
    };
    if !allowed {
        aurix_metrics::RATE_LIMIT_HITS.inc();
        return Err(AurixError::RateLimitExceeded("API rate limit exceeded".into()).into());
    }
    Ok(next.run(request).await)
}

/// Collapse path parameters so metric cardinality stays bounded: UUID segments become `:id`,
/// unknown roots become `other`.
pub fn normalize_path(path: &str) -> String {
    const KNOWN_ROOTS: &[&str] = &["health", "ready", "metrics", "admin", "v1", "ws"];
    let mut out = String::with_capacity(path.len());
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    let Some(root) = segments.next() else {
        return "/".into();
    };
    if !KNOWN_ROOTS.contains(&root) {
        return "/other".into();
    }
    out.push('/');
    out.push_str(root);
    for seg in segments {
        out.push('/');
        if Uuid::parse_str(seg).is_ok() || seg.chars().all(|c| c.is_ascii_digit()) {
            out.push_str(":id");
        } else {
            out.push_str(seg);
        }
    }
    out
}

pub async fn metrics_middleware(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = normalize_path(request.uri().path());
    let start = Instant::now();
    let response = next.run(request).await;
    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();
    aurix_metrics::API_REQUESTS_TOTAL
        .with_label_values(&[&method, &path, &status])
        .inc();
    aurix_metrics::API_REQUEST_DURATION
        .with_label_values(&[&method, &path])
        .observe(duration);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_normalization_bounds_cardinality() {
        assert_eq!(
            normalize_path("/v1/channels/0190f5a6-5c1e-7b8e-9f1c-0f5a6c1e7b8e/participants"),
            "/v1/channels/:id/participants"
        );
        assert_eq!(normalize_path("/wp-admin/anything"), "/other");
        assert_eq!(normalize_path("/health"), "/health");
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn api_key_permissions() {
        let ctx = ApiKeyContext {
            key_id: Uuid::nil(),
            app_id: AppId::from_uuid(Uuid::nil()),
            permissions: Arc::new(serde_json::json!(["channels:read"])),
        };
        assert!(ctx.has("channels:read"));
        assert!(!ctx.has("channels:write"));
        let legacy = ApiKeyContext {
            permissions: Arc::new(serde_json::json!({"all": true})),
            ..ctx.clone()
        };
        assert!(legacy.has("anything"));
        let star = ApiKeyContext {
            permissions: Arc::new(serde_json::json!(["*"])),
            ..ctx
        };
        assert!(star.has("moderation:write"));
    }
}
