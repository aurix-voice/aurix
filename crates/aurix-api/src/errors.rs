use aurix_common::error::AurixError;
use aurix_control::Throttled;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::{header::RETRY_AFTER, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub error: AurixError,
    /// `Retry-After` (seconds) for throttled requests.
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(error: AurixError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

/// `axum::Json` whose rejections are rendered as the standard `{"error":{...}}` envelope.
#[derive(axum::extract::FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct Json<T>(pub T);

impl<T: serde::Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// `axum::extract::Path` with JSON-formatted rejections.
#[derive(axum::extract::FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct Path<T>(pub T);

/// `axum::extract::Query` with JSON-formatted rejections.
#[derive(axum::extract::FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct Query<T>(pub T);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.error.status_code())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if self.error.is_internal() {
            tracing::error!(error = %self.error, code = self.error.error_code(), "request failed");
        }
        let body = json!({
            "error": {
                "code": self.error.error_code(),
                "message": self.error.public_message(),
            }
        });
        let mut response = (status, axum::Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}

impl From<AurixError> for ApiError {
    fn from(e: AurixError) -> Self {
        ApiError::new(e)
    }
}

impl From<Throttled> for ApiError {
    fn from(t: Throttled) -> Self {
        ApiError {
            error: AurixError::RateLimitExceeded(format!("Rate limit exceeded ({})", t.scope)),
            retry_after: Some(t.retry_after_secs()),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::new(AurixError::Database(e.to_string()))
    }
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        ApiError::new(AurixError::Validation(r.body_text()))
    }
}

impl From<PathRejection> for ApiError {
    fn from(r: PathRejection) -> Self {
        ApiError::new(AurixError::Validation(r.body_text()))
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        ApiError::new(AurixError::Validation(r.body_text()))
    }
}
