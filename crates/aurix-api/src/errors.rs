use aurix_common::error::AurixError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub struct ApiError(pub AurixError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if self.0.is_internal() {
            tracing::error!(error = %self.0, code = self.0.error_code(), "request failed");
        }
        let body = json!({
            "error": {
                "code": self.0.error_code(),
                "message": self.0.public_message(),
            }
        });
        (status, Json(body)).into_response()
    }
}

impl From<AurixError> for ApiError {
    fn from(e: AurixError) -> Self {
        ApiError(e)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError(AurixError::Database(e.to_string()))
    }
}
