//! Error type.

use jiff::SignedDuration;

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Transport-level failure (DNS, TLS, connection, timeout).
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// HTTP 429, or a Kraken `KT-CT-1199` rate-limit error.
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited {
        /// Server-suggested wait, if given.
        retry_after: Option<SignedDuration>,
    },

    /// HTTP 403 with a non-JSON body: Octopus' CDN/WAF throttling, not a
    /// credential problem. Back off for a long time (hours).
    #[error("throttled by CDN/WAF (HTTP 403, non-JSON body)")]
    Throttled,

    /// HTTP 401/403 with a JSON body, or a Kraken authentication error.
    #[error("unauthorised: {0}")]
    Unauthorised(String),

    /// An endpoint that needs an API key was called without one.
    #[error("this endpoint needs an API key")]
    MissingApiKey,

    /// Any other non-success HTTP status.
    #[error("HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated to 512 bytes).
        body: String,
    },

    /// Kraken GraphQL returned errors.
    #[error("GraphQL error {code:?}: {message}")]
    GraphQl {
        /// Kraken error code, e.g. `KT-CT-1111`.
        code: Option<String>,
        /// Human-readable message.
        message: String,
    },

    /// The response did not match the expected schema.
    #[error("unexpected response: {0}")]
    Decode(String),

    /// A URL could not be built.
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
}

/// Result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
