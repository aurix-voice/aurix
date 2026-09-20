use crate::handlers;
use crate::middleware;
use crate::state::AppState;
use crate::streams;
use crate::webhooks;
use axum::{
    extract::DefaultBodyLimit,
    http::{header, HeaderValue, Method},
    middleware as axum_middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use std::time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

fn cors_layer(state: &AppState) -> CorsLayer {
    let origins = &state.control.config.server.cors_origins;
    let allow_origin = if origins.iter().any(|o| o == "*") {
        // Rejected by config validation in production; convenient for local development.
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()))
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::HeaderName::from_static("x-api-key"),
            header::HeaderName::from_static("x-bootstrap-token"),
        ])
        .max_age(Duration::from_secs(600))
}

pub fn create_router(state: AppState) -> Router {
    let cors = cors_layer(&state);
    let body_limit = DefaultBodyLimit::max(state.control.config.server.max_body_bytes);
    let timeout = TimeoutLayer::new(Duration::from_secs(
        state.control.config.server.request_timeout_secs,
    ));

    let public_routes = Router::new()
        .route("/health", get(handlers::health))
        .route("/ready", get(handlers::ready))
        .route("/openapi.json", get(handlers::openapi));

    // Bootstrap is gated inside the handler (no admins yet, or X-Bootstrap-Token).
    let admin_public = Router::new()
        .route("/admin/login", post(handlers::admin_login))
        .route("/admin/setup", post(handlers::admin_setup))
        .route("/admin/auth/methods", get(handlers::admin_auth_methods))
        .route("/admin/oidc/login", get(handlers::admin_oidc_login))
        .route("/admin/oidc/callback", get(handlers::admin_oidc_callback));

    // Every handler here starts with `admin.require(<permission>)`; the middleware only
    // authenticates. Routes without a `require` call are open to any active administrator.
    let admin_routes = Router::new()
        .route("/admin/me", get(handlers::admin_me))
        .route("/admin/me/password", post(handlers::change_own_password))
        .route("/admin/logout-all", post(handlers::admin_logout_all))
        .route(
            "/admin/admins",
            post(handlers::create_admin).get(handlers::list_admins),
        )
        .route(
            "/admin/admins/:admin_id",
            get(handlers::get_admin).patch(handlers::update_admin),
        )
        .route(
            "/admin/admins/:admin_id/password",
            post(handlers::reset_admin_password),
        )
        .route(
            "/admin/admins/:admin_id/logout-all",
            post(handlers::revoke_admin_tokens),
        )
        .route("/admin/audit-log", get(handlers::admin_list_audit_logs))
        .route(
            "/admin/retention/sweep",
            post(handlers::admin_retention_sweep),
        )
        .route(
            "/v1/apps",
            post(handlers::create_app).get(handlers::list_apps),
        )
        .route(
            "/v1/apps/:app_id",
            get(handlers::get_app)
                .patch(handlers::update_app)
                .delete(handlers::delete_app),
        )
        .route(
            "/v1/apps/:app_id/rotate-key",
            post(handlers::admin_rotate_app_key),
        )
        .route("/v1/nodes", get(handlers::list_media_nodes))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::admin_auth_middleware,
        ));

    // Server-to-server (API key). Fine-grained permissions are enforced per handler.
    let api_routes = Router::new()
        .route("/v1/tokens", post(handlers::generate_token))
        .route("/v1/tokens/action", post(handlers::generate_action_token))
        .route("/v1/regions", get(handlers::list_regions))
        .route("/v1/turn/credentials", post(handlers::get_turn_credentials))
        .route(
            "/v1/channels",
            post(handlers::create_channel).get(handlers::list_channels),
        )
        .route(
            "/v1/channels/:channel_id",
            get(handlers::get_channel).delete(handlers::delete_channel),
        )
        .route(
            "/v1/channels/:channel_id/config",
            put(handlers::update_channel),
        )
        .route(
            "/v1/channels/:channel_id/participants",
            get(handlers::get_channel_participants),
        )
        .route(
            "/v1/sessions/:session_id/stats",
            get(handlers::get_session_stats),
        )
        .route("/v1/users", get(handlers::search_users))
        .route(
            "/v1/users/:user_id",
            get(handlers::get_user).delete(handlers::delete_user),
        )
        .route("/v1/users/:user_id/export", get(handlers::export_user))
        .route("/v1/users/:user_id/unban", post(handlers::unban_user_all))
        .route(
            "/v1/users/:user_id/blocks",
            get(handlers::list_user_blocks).post(handlers::add_user_block),
        )
        .route(
            "/v1/users/:user_id/blocks/:blocked_user_id",
            delete(handlers::remove_user_block),
        )
        .route(
            "/v1/channels/:channel_id/messages",
            get(handlers::list_channel_messages).post(handlers::send_channel_message),
        )
        .route(
            "/v1/users/:user_id/messages",
            get(handlers::list_user_messages).post(handlers::send_user_message),
        )
        .route(
            "/v1/users/:user_id/read-markers",
            get(handlers::list_user_read_markers).put(handlers::put_user_read_marker),
        )
        .route(
            "/v1/channels/:channel_id/read-markers",
            get(handlers::list_channel_read_markers),
        )
        .route(
            "/v1/channels/:channel_id/tts",
            post(handlers::announce_in_channel),
        )
        .route("/v1/tts/voices", get(handlers::tts_voices))
        .route("/v1/moderation/ban", post(handlers::ban_user))
        .route("/v1/moderation/bans", get(handlers::list_bans))
        .route("/v1/moderation/bans/:ban_id/revoke", post(handlers::unban))
        .route("/v1/moderation/mute", post(handlers::server_mute))
        .route("/v1/moderation/mute-all", post(handlers::mute_all))
        .route("/v1/moderation/kick", post(handlers::kick_user))
        .route("/v1/moderation/kick-all", post(handlers::kick_all))
        .route("/v1/moderation/report", post(handlers::report_user))
        .route(
            "/v1/moderation/events",
            get(handlers::list_moderation_events),
        )
        .route(
            "/v1/moderation/events/:event_id",
            get(handlers::get_moderation_event),
        )
        .route(
            "/v1/moderation/events/:event_id/resolve",
            post(handlers::resolve_moderation_event),
        )
        .route("/v1/safety/incidents", get(handlers::list_safety_incidents))
        .route(
            "/v1/safety/incidents/:incident_id",
            get(handlers::get_safety_incident),
        )
        .route(
            "/v1/safety/incidents/:incident_id/export",
            get(handlers::export_safety_incident),
        )
        .route(
            "/v1/safety/users/:user_id/risk",
            get(handlers::get_safety_user_risk),
        )
        .route("/v1/analytics", get(handlers::get_analytics))
        .route(
            "/v1/api-keys",
            post(handlers::create_api_key).get(handlers::list_api_keys),
        )
        .route(
            "/v1/api-keys/:key_id",
            patch(handlers::update_api_key).delete(handlers::revoke_api_key),
        )
        .route("/v1/audit-log", get(handlers::list_audit_logs))
        .route("/v1/recordings", get(handlers::list_recordings))
        .route("/v1/recordings/start", post(handlers::start_recording))
        .route("/v1/recordings/mixdown", post(handlers::mixdown_recordings))
        .route(
            "/v1/recordings/:recording_id",
            get(handlers::get_recording).delete(handlers::delete_recording),
        )
        .route(
            "/v1/recordings/:recording_id/download",
            get(handlers::download_recording),
        )
        .route(
            "/v1/recordings/:recording_id/stop",
            post(handlers::stop_recording),
        )
        .route(
            "/v1/recordings/:recording_id/transcribe",
            post(handlers::transcribe_recording),
        )
        .route(
            "/v1/recordings/:recording_id/transcript",
            get(handlers::get_recording_transcript),
        )
        .route("/v1/audio/streams", get(streams::list_streams))
        .route(
            "/v1/channels/:channel_id/audio/streams",
            post(streams::create_stream).get(streams::list_channel_streams),
        )
        .route(
            "/v1/channels/:channel_id/audio/streams/pull",
            get(streams::pull_stream),
        )
        .route(
            "/v1/channels/:channel_id/audio/streams/:stream_id",
            get(streams::get_stream).delete(streams::delete_stream),
        )
        .route(
            "/v1/webhooks",
            post(webhooks::create_webhook).get(webhooks::list_webhooks),
        )
        .route("/v1/webhooks/events", get(webhooks::list_event_types))
        .route(
            "/v1/webhooks/:webhook_id",
            get(webhooks::get_webhook)
                .patch(webhooks::update_webhook)
                .delete(webhooks::delete_webhook),
        )
        .route(
            "/v1/webhooks/:webhook_id/rotate-secret",
            post(webhooks::rotate_webhook_secret),
        )
        .route(
            "/v1/webhooks/:webhook_id/test",
            post(webhooks::test_webhook),
        )
        .route(
            "/v1/webhooks/:webhook_id/resync",
            post(webhooks::resync_webhook),
        )
        .route(
            "/v1/webhooks/:webhook_id/deliveries",
            get(webhooks::list_deliveries),
        )
        .route(
            "/v1/webhooks/:webhook_id/deliveries/:delivery_id",
            get(webhooks::get_delivery),
        )
        .route(
            "/v1/webhooks/:webhook_id/deliveries/:delivery_id/retry",
            post(webhooks::retry_delivery),
        )
        .route("/v1/events", get(webhooks::event_stream))
        .route("/v1/events/snapshot", get(webhooks::event_snapshot))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::api_key_middleware,
        ));

    // Player-facing (end-user JWT).
    let user_routes = Router::new()
        .route(
            "/v1/me/turn-credentials",
            get(handlers::get_turn_credentials_self),
        )
        .route("/v1/me/regions", get(handlers::list_regions_self))
        .route("/v1/me/reports", post(handlers::report_user_self))
        .route(
            "/v1/me/recordings/:recording_id/consent",
            post(handlers::recording_consent_self),
        )
        .route("/v1/webrtc/offer", post(handlers::webrtc_offer))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::auth_middleware,
        ));

    Router::new()
        .merge(public_routes)
        .merge(admin_public)
        .merge(admin_routes)
        .merge(api_routes)
        .merge(user_routes)
        .layer(axum_middleware::from_fn(middleware::metrics_middleware))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::rate_limit_middleware,
        ))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::client_ip_middleware,
        ))
        .layer(cors)
        .layer(timeout)
        .layer(body_limit)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
