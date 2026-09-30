use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Missing or unreadable configuration.
    Config,
    /// The worker rejected the API token.
    Unauthorized,
    NotFound,
    /// The worker rejected the request as invalid (HTTP 400, 409 or 422).
    BadRequest,
    /// The worker returned another error status.
    Api,
    /// The worker could not be reached.
    Network,
    /// The worker's response could not be understood.
    Decode,
    /// A linked account (HEY, …) needs signing in again.
    AccountAuth,
    /// A linked account's tool is missing, failed, or answered something unexpected.
    AccountUnavailable,
}

impl ErrorKind {
    /// Stable machine-readable code, used in JSON error envelopes.
    pub fn code(self) -> &'static str {
        match self {
            ErrorKind::Config => "not_configured",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::NotFound => "not_found",
            ErrorKind::BadRequest => "bad_request",
            ErrorKind::Api => "api_error",
            ErrorKind::Network => "network_error",
            ErrorKind::Decode => "bad_response",
            ErrorKind::AccountAuth => "account_unauthorized",
            ErrorKind::AccountUnavailable => "account_unavailable",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<Error> for String {
    fn from(e: Error) -> String {
        e.message
    }
}

pub type Result<T> = std::result::Result<T, Error>;
