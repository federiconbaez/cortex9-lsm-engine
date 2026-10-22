//! Engine errors.

use crate::vfs::VfsError;
use std::fmt;

/// Engine errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Storage failed (or the simulated power went out).
    Vfs(VfsError),
    /// Durable state that should be valid is not.
    Corruption(String),
    /// A previous write failed; the handle must be reopened.
    Poisoned,
    /// Key or value exceeds the limits.
    TooLarge,
}

impl From<VfsError> for Error {
    fn from(e: VfsError) -> Self {
        Error::Vfs(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Vfs(e) => write!(f, "{e}"),
            Error::Corruption(s) => write!(f, "corruption: {s}"),
            Error::Poisoned => write!(f, "handle poisoned by an earlier write error"),
            Error::TooLarge => write!(f, "key or value too large"),
        }
    }
}

impl std::error::Error for Error {}

/// Shorthand for corruption errors.
pub fn corrupt<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error::Corruption(msg.into()))
}
