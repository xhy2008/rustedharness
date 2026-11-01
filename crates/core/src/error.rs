use serde_json::{json, Value};

/// Errors surfaced by the harness. Tool failures carry a stable machine code so
/// that both the model and the frontends can react deterministically.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("{code}: {message}")]
    Tool {
        code: String,
        message: String,
        details: Option<Value>,
    },
    #[error("path escapes workspace root: {path}")]
    PathEscape { path: String, root: String },
    #[error("provider error: {0}")]
    Provider(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("cancelled")]
    Cancelled,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, HarnessError>;

impl HarnessError {
    pub fn tool(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn tool_details(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self::Tool {
            code: code.into(),
            message: message.into(),
            details: Some(details),
        }
    }

    pub fn code(&self) -> &str {
        match self {
            HarnessError::Tool { code, .. } => code,
            HarnessError::PathEscape { .. } => "E_PATH_ESCAPE",
            HarnessError::Provider(_) => "E_PROVIDER",
            HarnessError::Config(_) => "E_CONFIG",
            HarnessError::Cancelled => "E_CANCELLED",
            HarnessError::Io(_) => "E_IO",
            HarnessError::Other(_) => "E_OTHER",
        }
    }

    /// Rendering used when a tool failure is fed back to the model.
    pub fn to_model_json(&self) -> String {
        match self {
            HarnessError::Tool {
                code,
                message,
                details,
            } => json!({
                "ok": false,
                "code": code,
                "message": message,
                "details": details,
            })
            .to_string(),
            other => json!({
                "ok": false,
                "code": other.code(),
                "message": other.to_string(),
            })
            .to_string(),
        }
    }
}

impl From<anyhow::Error> for HarnessError {
    fn from(value: anyhow::Error) -> Self {
        HarnessError::Other(value.to_string())
    }
}

impl From<serde_json::Error> for HarnessError {
    fn from(value: serde_json::Error) -> Self {
        HarnessError::tool("E_INVALID_ARGUMENT", value.to_string())
    }
}

impl From<rusqlite::Error> for HarnessError {
    fn from(value: rusqlite::Error) -> Self {
        HarnessError::tool("E_DB", value.to_string())
    }
}
