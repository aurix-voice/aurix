use crate::errors::ApiError;
use crate::state::AppState;
use aurix_common::error::AurixError;
use aurix_common::types::{AdminContext, AdminRole, AppId, UserId};
use aurix_control::{Limit, LimitScope};
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

/// Header an administrator uses to act on one application over the API-key routes.
pub const ADMIN_APP_HEADER: &str = "x-aurix-app";

/// Who is behind an application-scoped request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiCaller {
    /// A tenant API key.
    ApiKey { key_id: Uuid },
    /// An administrator acting on the application named in `X-Aurix-App`.
    Admin { admin_id: Uuid, role: AdminRole },
}

/// Identity of a caller on the application-scoped (`/v1/*`) routes: a tenant API key, or an
/// administrator acting on one application.
#[derive(Clone, Debug)]
pub struct ApiKeyContext {
    pub caller: ApiCaller,
    pub app_id: AppId,
    pub permissions: Arc<serde_json::Value>,
}

impl ApiKeyContext {
    fn from_row(row: &ApiKeyRow) -> Self {
        Self {
            caller: ApiCaller::ApiKey { key_id: row.id },
            app_id: AppId::from_uuid(row.app_id),
            permissions: Arc::new(row.permissions.clone()),
        }
    }

    fn for_admin(admin: &AdminContext, app_id: AppId) -> Self {
        Self {
            caller: ApiCaller::Admin {
                admin_id: admin.admin_id,
                role: admin.role,
            },
            app_id,
            permissions: Arc::new(admin_api_permissions(admin.role)),
        }
    }

    /// Audit/moderation actor identity: the API key id, or the administrator id.
    pub fn actor(&self) -> UserId {
        match self.caller {
            ApiCaller::ApiKey { key_id } => UserId::from_uuid(key_id),
            ApiCaller::Admin { admin_id, .. } => UserId::from_uuid(admin_id),
        }
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

/// API-key permissions an administrator role holds when acting on an application through
/// `X-Aurix-App`. Read-only roles never get a write permission; `superadmin` gets everything.
pub fn admin_api_permissions(role: AdminRole) -> serde_json::Value {
    const VIEWER: &[&str] = &[
        "channels:read",
        "users:read",
        "analytics:read",
        "events:read",
        "webhooks:read",
        "recordings:read",
        "audio_streams:read",
    ];
    const MODERATOR: &[&str] = &[
        "moderation:read",
        "moderation:write",
        "chat:read",
        "chat:write",
        "audit:read",
        "users:write",
    ];
    const ADMIN: &[&str] = &[
        "channels:write",
        "webhooks:write",
        "recordings:write",
        "audio_streams:write",
        "keys:manage",
        "tokens:issue",
        "turn:issue",
        "tts:write",
        "users:export",
    ];
    if role >= AdminRole::Superadmin {
        return serde_json::json!(["*"]);
    }
    let mut out: Vec<&str> = VIEWER.to_vec();
    if role >= AdminRole::Moderator {
        out.extend_from_slice(MODERATOR);
    }
    if role >= AdminRole::Admin {
        out.extend_from_slice(ADMIN);
    }
    serde_json::Value::Array(out.into_iter().map(serde_json::Value::from).collect())
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

/// Tenant API key from `X-API-Key` (or `Authorization: Bearer aurx_...`); alternatively an
/// admin JWT plus `X-Aurix-App: <app_id>` — the administrator then acts on that application
/// with the permissions of its role ([`admin_api_permissions`]).
pub async fn api_key_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| bearer_token(request.headers()).filter(|t| t.starts_with("aurx_")));
    let Some(api_key) = api_key else {
        let app_header = request
            .headers()
            .get(ADMIN_APP_HEADER)
            .ok_or_else(|| AurixError::AuthenticationFailed("Missing API key".into()))?
            .to_str()
            .map_err(|_| AurixError::Validation("X-Aurix-App must be an application id".into()))?
            .to_owned();
        let ctx = admin_on_app(&state, request.headers(), &app_header).await?;
        request.extensions_mut().insert(ctx.app_id);
        request.extensions_mut().insert(ctx);
        return Ok(next.run(request).await);
    };
    let key_row = state.control.api_keys.validate_key(api_key).await?;
    let ctx = ApiKeyContext::from_row(&key_row);
    let limits = &state.control.limits;
    if limits.enabled() && limits.config().per_key && key_row.rate_limit > 0 {
        limits
            .check_with(
                LimitScope::ApiKey,
                &key_row.id.to_string(),
                Limit::per_minute(key_row.rate_limit as u32),
                1.0,
            )
            .await?;
    }
    request.extensions_mut().insert(ctx.app_id);
    request.extensions_mut().insert(ctx);
    Ok(next.run(request).await)
}

async fn admin_on_app(
    state: &AppState,
    headers: &HeaderMap,
    app_header: &str,
) -> Result<ApiKeyContext, ApiError> {
    let app_id = Uuid::parse_str(app_header.trim())
        .map_err(|_| AurixError::Validation("X-Aurix-App must be an application id".into()))?;
    let token = bearer_token(headers)
        .filter(|t| !t.starts_with("aurx_"))
        .ok_or_else(|| {
            AurixError::AuthenticationFailed("X-Aurix-App requires an admin bearer token".into())
        })?;
    let admin: AdminContext = state.control.authenticate_admin(token).await?;
    admin.require(aurix_common::types::AdminPermission::AppsRead)?;
    let app = aurix_db::queries::get_app(&state.control.pool, app_id)
        .await?
        .ok_or_else(|| AurixError::NotFound("Application not found".into()))?;
    if !app.active {
        return Err(AurixError::NotFound("Application not found".into()).into());
    }
    Ok(ApiKeyContext::for_admin(&admin, AppId(app_id)))
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
    if !state.control.limits.enabled() {
        return Ok(next.run(request).await);
    }
    let ip = request
        .extensions()
        .get::<ClientIp>()
        .map(|c| c.0)
        .unwrap_or_else(|| resolve_client_ip(&state, &request));
    state
        .control
        .limits
        .check(LimitScope::ApiIp, &state.control.limits.ip_subject(ip))
        .await?;
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
            caller: ApiCaller::ApiKey {
                key_id: Uuid::nil(),
            },
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

    #[test]
    fn admin_roles_map_to_cumulative_api_permissions() {
        let perms = |role: AdminRole| ApiKeyContext {
            caller: ApiCaller::Admin {
                admin_id: Uuid::nil(),
                role,
            },
            app_id: AppId::from_uuid(Uuid::nil()),
            permissions: Arc::new(admin_api_permissions(role)),
        };
        let viewer = perms(AdminRole::Viewer);
        assert!(viewer.has("channels:read"));
        assert!(!viewer.has("channels:write"));
        assert!(!viewer.has("chat:read"));
        assert!(!viewer.has("moderation:write"));
        let moderator = perms(AdminRole::Moderator);
        assert!(moderator.has("channels:read"));
        assert!(moderator.has("moderation:write"));
        assert!(moderator.has("chat:read"));
        assert!(!moderator.has("channels:write"));
        assert!(!moderator.has("webhooks:write"));
        assert!(!moderator.has("users:erase"));
        let admin = perms(AdminRole::Admin);
        assert!(admin.has("webhooks:write"));
        assert!(admin.has("keys:manage"));
        assert!(!admin.has("users:erase"));
        let superadmin = perms(AdminRole::Superadmin);
        assert!(superadmin.has("users:erase"));
        assert_eq!(superadmin.actor(), UserId::from_uuid(Uuid::nil()));
    }
}
