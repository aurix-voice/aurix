use crate::errors::ApiError;
use crate::state::AppState;
use aurix_common::error::AurixError;
use axum::{
    extract::{Request, State},
    http::header::AUTHORIZATION,
    middleware::Next,
    response::Response,
};
use std::time::Instant;

pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let auth_header = request.headers().get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AurixError::AuthenticationFailed("Missing Authorization header".into()))?;
    let token = auth_header.strip_prefix("Bearer ")
        .ok_or(AurixError::AuthenticationFailed("Invalid Authorization format".into()))?;
    let validated = state.control.validate_token(token)?;
    request.extensions_mut().insert(validated);
    Ok(next.run(request).await)
}

pub async fn api_key_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let api_key = request.headers().get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            request.headers().get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        })
        .ok_or(AurixError::AuthenticationFailed("Missing API key".into()))?;
    let key_row = state.control.api_keys.validate_key(api_key).await?;
    request.extensions_mut().insert(aurix_common::types::AppId::from_uuid(key_row.app_id));
    Ok(next.run(request).await)
}

/// Middleware that validates an admin JWT (from `Authorization: Bearer <admin_token>`)
/// and injects `AdminContext` into request extensions.
pub async fn admin_auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let auth_header = request.headers().get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AurixError::AuthenticationFailed("Missing Authorization header".into()))?;
    let token = auth_header.strip_prefix("Bearer ")
        .ok_or(AurixError::AuthenticationFailed("Invalid Authorization format".into()))?;
    let admin_ctx = state.control.admin_auth.validate_admin_token(token)?;
    request.extensions_mut().insert(admin_ctx);
    Ok(next.run(request).await)
}

pub async fn rate_limit_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let ip = request.headers().get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or("unknown").trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let key = format!("api:{}", ip);

    // Use distributed Redis rate limiter when available, fall back to local
    let allowed = if let Some(ref redis) = state.control.redis {
        redis.check_rate_limit(
            &key,
            state.control.config.rate_limiting.requests_per_second,
            1,
        ).await.unwrap_or_else(|_| state.control.rate_limiter.check(&key))
    } else {
        state.control.rate_limiter.check(&key)
    };

    if !allowed {
        aurix_metrics::RATE_LIMIT_HITS.inc();
        return Err(AurixError::RateLimitExceeded("API rate limit exceeded".into()).into());
    }
    Ok(next.run(request).await)
}

pub async fn metrics_middleware(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let start = Instant::now();
    let response = next.run(request).await;
    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();
    aurix_metrics::API_REQUESTS_TOTAL.with_label_values(&[&method, &path, &status]).inc();
    aurix_metrics::API_REQUEST_DURATION.with_label_values(&[&method, &path]).observe(duration);
    response
}