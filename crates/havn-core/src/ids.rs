//! Strongly-typed identifiers backed by UUID v7 (time-sortable, indexable).
//!
//! Each ID is a newtype around `Uuid` with `#[serde(transparent)]`, so on
//! the wire and in the database it is just a UUID string — but in Rust
//! code the type system prevents mixing an `AgentId` with a `UserId`.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

macro_rules! id_newtype {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            #[must_use]
            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|e| Error::InvalidId(e.to_string()))
            }
        }
    };
}

id_newtype!(AgentId, "Identifier for a havn agent.");
id_newtype!(UserId, "Identifier for a havn user.");
id_newtype!(TeamId, "Identifier for a team.");
id_newtype!(RoleId, "Identifier for a role within a team.");
id_newtype!(CredentialId, "Identifier for a stored API credential.");
id_newtype!(
    ChannelBindingId,
    "Identifier for an agent's binding to a chat channel."
);
id_newtype!(CronJobId, "Identifier for a scheduled cron job.");
id_newtype!(SkillId, "Identifier for an installed skill.");

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::str::FromStr as _;

    #[test]
    fn agent_id_round_trips_through_string() {
        let id = AgentId::new();
        let parsed = AgentId::from_str(&id.to_string()).expect("parse");
        assert_eq!(id, parsed);
    }

    #[test]
    fn distinct_id_types_are_not_interchangeable() {
        // This test exists for documentation: the assertion below is enforced
        // by the type system at compile time. Uncommenting fails to compile:
        //
        //   let _ = AgentId::new() == UserId::new();
        //
        // We assert at runtime that two freshly-minted IDs of the same type
        // differ — UUID v7 collisions in a single test would indicate a serious bug.
        let a = AgentId::new();
        let b = AgentId::new();
        assert_ne!(a, b);
    }
}
