//! Database-layer errors.

use thiserror::Error;

pub type Result<T, E = DbError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DbError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migrate: {0}")]
    Migrate(sqlx::migrate::MigrateError),

    #[error("not found")]
    NotFound,

    #[error("invalid value in column {column}: {message}")]
    InvalidValue {
        column: &'static str,
        message: String,
    },

    #[error("unique-constraint violation on {0}")]
    Conflict(&'static str),
}
