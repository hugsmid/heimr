//! Error type.

use jiff::SignedDuration;

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Transport-level failure (DNS, TLS, connection, timeout).
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// Open-Meteo rejected the request: `{"error": true, "reason": "…"}`,
    /// normally with HTTP 400 (unknown variable, date out of range, …).
    #[error("Open-Meteo error (HTTP {status}): {reason}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// The `reason` string from the body.
        reason: String,
    },

    /// HTTP 429: a minutely, hourly or daily request limit was exceeded.
    #[error("rate limited (retry after {retry_after:?}): {reason:?}")]
    RateLimited {
        /// Server-suggested wait (`Retry-After`), if given.
        retry_after: Option<SignedDuration>,
        /// The `reason` string, if the body had one.
        reason: Option<String>,
    },

    /// Any other non-success HTTP status.
    #[error("HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated to 512 bytes).
        body: String,
    },

    /// The response was not valid JSON for the expected shape.
    #[error("unexpected response: {0}")]
    Decode(String),

    /// A requested hourly column is absent from the response.
    #[error("missing hourly column `{0}`")]
    MissingColumn(String),

    /// A column's length differs from `hourly.time`.
    #[error("column `{column}` has {actual} values, `time` has {expected}")]
    LengthMismatch {
        /// Column name.
        column: String,
        /// Length of `hourly.time`.
        expected: usize,
        /// Length of the column.
        actual: usize,
    },

    /// `hourly_units` reports a unit other than the one requested.
    #[error("column `{column}` is in `{actual}`, expected `{expected}`")]
    UnitMismatch {
        /// Column name (`time` for the time format).
        column: String,
        /// The unit this crate requested.
        expected: &'static str,
        /// The unit the response reported (empty if missing).
        actual: String,
    },

    /// `hourly.time` is not strictly increasing, or holds an out-of-range
    /// value.
    #[error("bad hourly time axis: {0}")]
    TimeAxis(String),

    /// Invalid input (coordinates, dates, forecast length).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// A URL could not be built.
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
}

/// Result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
