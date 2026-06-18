//! Structured error types for route handlers.
//!
//! Provides `AppError`, an enum implementing Axum's `IntoResponse` to produce
//! consistent JSON error bodies across all API endpoints.
//!
//! Also provides `JsonBody<T>`, a custom extractor that wraps `axum::Json<T>`
//! but converts deserialization rejections to 400 Bad Request instead of 422.

use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Structured JSON error body returned by all API error responses.
#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
}

/// Application-level error type for route handlers.
///
/// Each variant maps to a specific HTTP status code and produces a uniform
/// JSON response body of the form `{ "error": "..." }`.
pub enum AppError {
    /// 400 Bad Request — malformed input.
    BadRequest(String),
    /// 403 Forbidden — ownership mismatch.
    Forbidden(String),
    /// 404 Not Found — resource does not exist.
    NotFound(String),
    /// 409 Conflict — state conflict (e.g., no available devboxes).
    Conflict(String),
    /// 500 Internal Server Error — database or serialization failure.
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            Self::Conflict(msg) => (StatusCode::CONFLICT, msg),
            Self::Internal(err) => {
                tracing::error!("internal error: {err:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, axum::Json(ErrorBody { error: message })).into_response()
    }
}

/// Convenience conversion: `anyhow::Error` → `AppError::Internal`.
impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

/// Custom JSON body extractor that returns 400 Bad Request for deserialization
/// failures instead of Axum's default 422 Unprocessable Entity.
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(JsonBody(value)),
            Err(rejection) => Err(AppError::BadRequest(rejection.body_text())),
        }
    }
}
