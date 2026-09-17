use aurix_common::error::AurixError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub struct ApiError(pub AurixError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status_code())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        let body = json!({
            "error": {
                "code": self.0.error_code(),
                "message": self.0.to_string(),
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