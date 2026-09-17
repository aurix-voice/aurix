use crate::handlers;
use crate::middleware;
use crate::state::AppState;
use axum::{
    middleware as axum_middleware,
    routing::{delete, get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // ── Public (no auth) ──
    let public_routes = Router::new()
        .route("/health", get(handlers::health))
        .route("/metrics", get(aurix_metrics::metrics_handler));

    // ── Admin login + first admin creation (no auth) ──
    let admin_login_routes = Router::new()
        .route("/admin/login", post(handlers::admin_login))
        .route("/admin/setup", post(handlers::create_admin));

    // ── Admin-protected routes (admin JWT) ──
    let admin_routes = Router::new()
        .route("/admin/create", post(handlers::create_admin))
        .route("/v1/apps", post(handlers::create_app))
        .route("/v1/apps", get(handlers::list_apps))
        .route("/v1/apps/{app_id}", get(handlers::get_app))
        .route("/v1/apps/{app_id}", delete(handlers::delete_app))
        .route("/v1/nodes", get(handlers::list_media_nodes))
        .layer(axum_middleware::from_fn_with_state(state.clone(), middleware::admin_auth_middleware));

    // ── Token generation (requires API key) ──
    let auth_routes = Router::new()
        .route("/v1/tokens", post(handlers::generate_token))
        .route("/v1/turn/credentials", post(handlers::get_turn_credentials))
        .layer(axum_middleware::from_fn_with_state(state.clone(), middleware::api_key_middleware));

    // ── App-scoped API routes (require API key) ──
    let api_routes = Router::new()
        .route("/v1/channels", post(handlers::create_channel))
        .route("/v1/channels", get(handlers::list_channels))
        .route("/v1/channels/{channel_id}", get(handlers::get_channel))
        .route("/v1/channels/{channel_id}", delete(handlers::delete_channel))
        .route("/v1/channels/{channel_id}/participants", get(handlers::get_channel_participants))
        .route("/v1/users", get(handlers::search_users))
        .route("/v1/users/{user_id}", get(handlers::get_user))
        .route("/v1/moderation/ban", post(handlers::ban_user))
        .route("/v1/moderation/mute", post(handlers::server_mute))
        .route("/v1/moderation/kick", post(handlers::kick_user))
        .route("/v1/moderation/report", post(handlers::report_user))
        .route("/v1/moderation/events", get(handlers::list_moderation_events))
        .route("/v1/analytics", get(handlers::get_analytics))
        .route("/v1/api-keys", post(handlers::create_api_key))
        .route("/v1/api-keys", get(handlers::list_api_keys))
        .route("/v1/api-keys/{key_id}", delete(handlers::revoke_api_key))
        .route("/v1/audit-log", get(handlers::list_audit_logs))
        .route("/v1/recordings/start", post(handlers::start_recording))
        .route("/v1/recordings/{recording_id}", get(handlers::get_recording))
        .route("/v1/recordings/{recording_id}/stop", post(handlers::stop_recording))
        .route("/v1/webrtc/offer", post(handlers::webrtc_offer))
        .layer(axum_middleware::from_fn_with_state(state.clone(), middleware::api_key_middleware));

    Router::new()
        .merge(public_routes)
        .merge(admin_login_routes)
        .merge(admin_routes)
        .merge(auth_routes)
        .merge(api_routes)
        .layer(axum_middleware::from_fn(middleware::metrics_middleware))
        .layer(axum_middleware::from_fn_with_state(state.clone(), middleware::rate_limit_middleware))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}