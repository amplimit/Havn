//! Error types shared across havn crates.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid identifier: {0}")]
    InvalidId(String),

    #[error("invalid policy: {0}")]
    InvalidPolicy(String),

    #[error("invalid message content: {0}")]
    InvalidMessage(String),
}
