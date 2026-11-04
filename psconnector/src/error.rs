//! Library error type.

/// Errors returned by psconnector.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The console did not answer the DDP status request (powered off or wrong address).
    #[error("no response from {host} (powered off or wrong address)")]
    NoResponse { host: String },

    /// The console is not on, e.g. in rest mode (DDP status code).
    #[error("console is not on (status {code})")]
    NotOn { code: u16 },

    /// Something is wrong with the registration data (profile file, regist key).
    #[error("registration problem: {0}")]
    Profile(String),

    /// Another Remote Play session is already connected. Also happens for a few seconds
    /// while the console cleans up the previous session.
    #[error("another Remote Play session is connected")]
    Busy,

    /// The console rejected the request (auth failure, version mismatch, ...).
    #[error("console rejected the request: {0}")]
    Rejected(String),

    /// Received data was not in the expected format.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// No response within the expected time.
    #[error("timed out: {0}")]
    Timeout(&'static str),

    /// The connection was closed.
    #[error("connection closed: {0}")]
    Closed(String),

    /// Unsupported feature or console.
    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn protocol(msg: impl Into<String>) -> Error {
    Error::Protocol(msg.into())
}

pub(crate) fn profile(msg: impl Into<String>) -> Error {
    Error::Profile(msg.into())
}
