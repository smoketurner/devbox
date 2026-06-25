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
    /// 401 Unauthorized — missing or invalid credential.
    Unauthorized(String),
    /// 403 Forbidden — ownership mismatch.
    Forbidden(String),
    /// 404 Not Found — resource does not exist.
    NotFound(String),
    /// 409 Conflict — state conflict (e.g., no available devboxes).
    Conflict(String),
    /// 500 Internal Server Error — database or serialization failure.
    Internal(anyhow::Error),
}

impl AppError {
    /// A message safe to show an end user. Mirrors the JSON `error` body, with
    /// `Internal` details suppressed — used by the HTML dashboard to surface a
    /// failed claim inline.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::BadRequest(msg)
            | Self::Unauthorized(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::Conflict(msg) => msg.clone(),
            Self::Internal(_) => "internal server error".to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
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

/// Authentication failures map to 401.
///
/// `AuthError::Invalid` wraps third-party error text (JWKS/ALB fetch failures,
/// `jsonwebtoken` internals) that must not reach API clients. Log the detail for
/// operators and return a static, generic message — the same split
/// `AppError::Internal` uses.
impl From<crate::auth::AuthError> for AppError {
    fn from(err: crate::auth::AuthError) -> Self {
        tracing::warn!("auth error: {err}");
        let msg = match err {
            crate::auth::AuthError::Missing => "no authentication credential",
            crate::auth::AuthError::Invalid(_) => "invalid authentication credential",
        };
        Self::Unauthorized(msg.to_string())
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

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: panic on assertion failure is acceptable"
)]
mod tests {
    use super::*;
    use crate::auth::AuthError;

    async fn body_error(err: AppError) -> (StatusCode, String) {
        let response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let error = parsed
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .to_string();
        (status, error)
    }

    #[tokio::test]
    async fn invalid_auth_error_is_sanitized() {
        let leaky = AuthError::Invalid(
            "fetch JWKS: error sending request for url (https://us.vouch.sh/oauth/jwks)"
                .to_string(),
        );
        let (status, body) = body_error(AppError::from(leaky)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "invalid authentication credential");
        assert!(!body.contains("vouch.sh"));
        assert!(!body.contains("JWKS"));
    }

    #[tokio::test]
    async fn missing_auth_error_is_generic() {
        let (status, body) = body_error(AppError::from(AuthError::Missing)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "no authentication credential");
    }
}
