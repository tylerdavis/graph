use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned {status}: {body}")]
    Api {
        status: u16,
        body: String,
        /// Parsed Retry-After header, when the provider sent one.
        retry_after: Option<u64>,
    },
    #[error("failed to parse provider response: {0}")]
    Parse(String),
    #[error("model output did not match the requested schema: {0}")]
    SchemaMismatch(String),
    #[error("provider '{0}' is not configured")]
    UnknownProvider(String),
    /// Configured, but cannot be used as configured — most commonly an
    /// unset `${VAR}` behind its api_key. The reason names the variable and
    /// the config path, because this message is often all a caller (or an
    /// agent driving `graph mcp serve`) gets to diagnose with.
    #[error("provider '{provider}' is configured but not usable: {reason}")]
    ProviderUnavailable { provider: String, reason: String },
    #[error("no model configured for role '{0}' and no default set")]
    NoModelForRole(String),
    #[error("no model named '{name}' is configured; available names: {available}")]
    UnknownModelName { name: String, available: String },
    #[error("{0}")]
    Unsupported(String),
}
