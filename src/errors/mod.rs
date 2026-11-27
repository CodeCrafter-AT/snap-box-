use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

/// Unified error type for snap-box orchestration, filesystem backends, API, and CLI.
#[derive(Debug, Error)]
pub enum SnapBoxError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("sandbox '{0}' not found")]
    SandboxNotFound(String),

    #[error("invalid state transition for sandbox '{sandbox_id}': {from} -> {to}")]
    InvalidStateTransition {
        sandbox_id: String,
        from: String,
        to: String,
    },

    #[error("sandbox '{0}' is currently busy")]
    SandboxBusy(String),

    #[error("sandbox pool exhausted ({allocated}/{max_sandboxes} sandboxes allocated)")]
    PoolExhausted {
        max_sandboxes: usize,
        allocated: usize,
    },

    #[error("filesystem error during '{operation}' on '{path}': {source}")]
    Filesystem {
        operation: String,
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("unsupported platform capability: {0}")]
    UnsupportedPlatform(String),

    #[error("security policy violation: {0}")]
    SecurityViolation(String),

    #[error("resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    #[error("command execution timed out after {timeout_ms}ms")]
    ExecutionTimeout { timeout_ms: u64 },

    #[error("command execution failed: {0}")]
    ExecutionFailed(String),

    #[error("daemon error: {0}")]
    Daemon(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl SnapBoxError {
    /// Returns a machine-readable error code for structured JSON API responses.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Config(_) => "CONFIG_ERROR",
            Self::Validation(_) => "VALIDATION_ERROR",
            Self::SandboxNotFound(_) => "SANDBOX_NOT_FOUND",
            Self::InvalidStateTransition { .. } => "INVALID_STATE_TRANSITION",
            Self::SandboxBusy(_) => "SANDBOX_BUSY",
            Self::PoolExhausted { .. } => "POOL_EXHAUSTED",
            Self::Filesystem { .. } => "FILESYSTEM_ERROR",
            Self::UnsupportedPlatform(_) => "UNSUPPORTED_PLATFORM",
            Self::SecurityViolation(_) => "SECURITY_VIOLATION",
            Self::ResourceLimitExceeded(_) => "RESOURCE_LIMIT_EXCEEDED",
            Self::ExecutionTimeout { .. } => "EXECUTION_TIMEOUT",
            Self::ExecutionFailed(_) => "EXECUTION_FAILED",
            Self::Daemon(_) => "DAEMON_ERROR",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    /// Maps each domain error to an appropriate HTTP status code.
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::Config(_) | Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::SandboxNotFound(_) => StatusCode::NOT_FOUND,
            Self::InvalidStateTransition { .. } | Self::SandboxBusy(_) => StatusCode::CONFLICT,
            Self::PoolExhausted { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::SecurityViolation(_) => StatusCode::FORBIDDEN,
            Self::ResourceLimitExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::ExecutionTimeout { .. } => StatusCode::REQUEST_TIMEOUT,
            Self::UnsupportedPlatform(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Filesystem { .. }
            | Self::ExecutionFailed(_)
            | Self::Daemon(_)
            | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ApiErrorEnvelope {
    pub error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
}

impl IntoResponse for SnapBoxError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        let body = ApiErrorEnvelope {
            error: ApiErrorBody {
                code: self.error_code(),
                message: self.to_string(),
            },
        };
        (status, Json(body)).into_response()
    }
}

pub type Result<T> = std::result::Result<T, SnapBoxError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_http_status_mapping_is_accurate() {
        assert_eq!(
            SnapBoxError::SandboxNotFound("abc".into()).http_status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            SnapBoxError::InvalidStateTransition {
                sandbox_id: "1".into(),
                from: "WARM".into(),
                to: "RUNNING".into()
            }
            .http_status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            SnapBoxError::PoolExhausted {
                max_sandboxes: 4,
                allocated: 4
            }
            .http_status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            SnapBoxError::SecurityViolation("escape".into()).http_status(),
            StatusCode::FORBIDDEN
        );
    }
}
