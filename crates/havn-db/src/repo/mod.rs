//! Per-entity repository modules.
//!
//! Each module owns the queries for one (or a few closely-related) entities
//! from spec §5.1. Functions return havn-core domain types (e.g. `UserId`,
//! `AgentId`) — internal DTOs use raw `Uuid` and convert at the boundary.

use uuid::Uuid;

use crate::DbError;

pub mod agents;
pub mod audit;
pub mod credential_usage;
pub mod credentials;
pub mod cron;
pub mod cross_agent_queries;
pub mod roles;
pub mod team_memberships;
pub mod teams;
pub mod users;

/// Parse a UUID from a database column we own. Values are written by us as
/// UUID v7, so a parse error indicates a corrupted or externally-modified row.
fn parse_db_uuid(s: &str, column: &'static str) -> Result<Uuid, DbError> {
    Uuid::parse_str(s).map_err(|e| DbError::InvalidValue {
        column,
        message: e.to_string(),
    })
}
