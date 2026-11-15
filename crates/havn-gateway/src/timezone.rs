//! Each user's time zone (issue #103): agents tell the time in it. A user
//! without one gets the gateway host's. Set on the dashboard, through
//! `PUT /me/timezone`, or by an agent with the `set_timezone` tool when the
//! user says where they are; running agents of the user get the change at
//! once.

use std::str::FromStr as _;

use havn_core::{AgentId, UserId};
use havn_proto::{GatewayToAgent, GatewayToolSpec, TimezoneUpdate};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tracing::warn;

use crate::agent_registry::AgentRegistry;

pub const TOOL_NAME: &str = "set_timezone";

/// The canonical IANA name of `name`, if it is one (`asia/shanghai` gives
/// `Asia/Shanghai`).
pub fn canonical(name: &str) -> Result<String, String> {
    let name = name.trim();
    chrono_tz::Tz::from_str(name)
        .ok()
        .or_else(|| {
            chrono_tz::TZ_VARIANTS
                .iter()
                .copied()
                .find(|tz| tz.name().eq_ignore_ascii_case(name))
        })
        .map(|tz| tz.name().to_string())
        .ok_or_else(|| {
            format!("{name:?} is not an IANA time zone such as Europe/Berlin or Asia/Shanghai")
        })
}

/// The gateway host's time zone: `TZ`, `/etc/timezone`, the target of
/// `/etc/localtime`, else UTC.
#[must_use]
pub fn host() -> String {
    let from_env = std::env::var("TZ")
        .ok()
        .map(|tz| tz.trim_start_matches(':').to_string());
    let from_file = || {
        std::fs::read_to_string("/etc/timezone")
            .ok()
            .map(|s| s.trim().to_string())
    };
    let from_link = || {
        let target = std::fs::read_link("/etc/localtime").ok()?;
        let target = target.to_string_lossy();
        target
            .split_once("zoneinfo/")
            .map(|(_, name)| name.to_string())
    };
    from_env
        .into_iter()
        .chain(from_file())
        .chain(from_link())
        .find_map(|name| canonical(&name).ok())
        .unwrap_or_else(|| "UTC".to_string())
}

/// `user`'s time zone, or the host's.
pub async fn for_user(db: &SqlitePool, user: UserId) -> String {
    match havn_db::repo::users::timezone(db, user).await {
        Ok(Some(name)) => canonical(&name).unwrap_or_else(|_| host()),
        Ok(None) => host(),
        Err(e) => {
            warn!(%user, error = %e, "time zone lookup failed; using the host's");
            host()
        }
    }
}

/// The time zone of `agent`'s owner.
pub async fn for_agent(db: &SqlitePool, agent: AgentId) -> String {
    match havn_db::repo::agents::find_by_id(db, agent).await {
        Ok(Some(row)) => for_user(db, row.owner_id).await,
        _ => host(),
    }
}

/// Store `name` (canonical, or `None` for the host's) as `user`'s zone and
/// tell their running agents. Returns the zone now in effect.
pub async fn set(
    db: &SqlitePool,
    registry: &AgentRegistry,
    user: UserId,
    name: Option<&str>,
) -> Result<String, String> {
    let stored = name.map(canonical).transpose()?;
    havn_db::repo::users::set_timezone(db, user, stored.as_deref())
        .await
        .map_err(|e| format!("saving the time zone failed: {e}"))?;
    let effective = stored.unwrap_or_else(host);
    match havn_db::repo::agents::list_for_owner(db, user).await {
        Ok(agents) => {
            for agent in agents {
                if registry.is_connected(agent.id).await {
                    let update = GatewayToAgent::TimezoneUpdate(TimezoneUpdate {
                        timezone: effective.clone(),
                    });
                    // Never wait on a stalled agent; its next Welcome
                    // carries the zone anyway.
                    if let Err(e) = registry.try_send(agent.id, update).await {
                        warn!(agent_id = %agent.id, error = ?e, "could not send the new time zone");
                    }
                }
            }
        }
        Err(e) => warn!(%user, error = %e, "could not list agents for a time zone change"),
    }
    Ok(effective)
}

#[must_use]
pub fn spec() -> GatewayToolSpec {
    GatewayToolSpec {
        name: TOOL_NAME.into(),
        description: "Set the user's time zone, in which you and all their other agents \
                      tell the time. Use it when the user tells you where they are or which \
                      zone they want, not to convert a single time. Give an IANA name such as \
                      Asia/Shanghai, Europe/London or America/New_York."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "IANA time zone name, e.g. Asia/Shanghai."
                }
            },
            "required": ["timezone"],
            "additionalProperties": false
        }),
    }
}

/// `set_timezone` calls one user's agents may make per minute, together.
pub const TOOL_PER_USER_PER_MINUTE: u32 = 3;

/// Run `set_timezone` for `agent`, owned by `owner`. Audited as
/// `user.timezone.updated` with the agent.
pub async fn run_tool(
    db: &SqlitePool,
    registry: &AgentRegistry,
    rate: &crate::web_search::UserRate,
    owner: UserId,
    agent: AgentId,
    input: &Value,
) -> Result<String, String> {
    let name = input
        .get("timezone")
        .and_then(Value::as_str)
        .ok_or("set_timezone needs a timezone")?;
    let wanted = canonical(name)?;
    if havn_db::repo::users::timezone(db, owner)
        .await
        .ok()
        .flatten()
        .as_deref()
        == Some(&wanted)
    {
        return Ok(format!("The user's time zone already is {wanted}."));
    }
    if !rate.admit(owner).await {
        return Err(format!(
            "the time zone was changed {TOOL_PER_USER_PER_MINUTE} times in the last minute; \
             ask the user to set it on the dashboard"
        ));
    }
    let now = set(db, registry, owner, Some(&wanted)).await?;
    crate::audit::record(
        db,
        owner,
        None,
        Some(agent),
        "user.timezone.updated",
        json!({ "timezone": now, "by": "set_timezone" }),
    )
    .await;
    Ok(format!(
        "The user's time zone is now {now}. Their agents tell the time in it from the next turn; \
         they can change it on the dashboard."
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn names_are_checked_and_made_canonical() {
        assert_eq!(canonical("Asia/Shanghai").unwrap(), "Asia/Shanghai");
        assert_eq!(canonical(" asia/shanghai ").unwrap(), "Asia/Shanghai");
        assert_eq!(canonical("UTC").unwrap(), "UTC");
        assert!(canonical("Mars/Olympus").is_err());
        assert!(canonical("+08:00").is_err());
    }

    #[test]
    fn the_host_zone_is_a_valid_name() {
        assert!(canonical(&host()).is_ok());
    }

    #[tokio::test]
    async fn a_users_zone_is_stored_and_falls_back_to_the_hosts() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let user =
            havn_db::repo::users::create(&db, havn_db::repo::users::NewUser { display_name: "u" })
                .await
                .expect("user")
                .id;
        let registry = AgentRegistry::new();
        assert_eq!(for_user(&db, user).await, host());
        assert_eq!(
            set(&db, &registry, user, Some("europe/berlin"))
                .await
                .unwrap(),
            "Europe/Berlin"
        );
        assert_eq!(for_user(&db, user).await, "Europe/Berlin");
        assert!(set(&db, &registry, user, Some("nowhere")).await.is_err());
        assert_eq!(
            for_user(&db, user).await,
            "Europe/Berlin",
            "a bad name changes nothing"
        );
        assert_eq!(set(&db, &registry, user, None).await.unwrap(), host());
        assert_eq!(for_user(&db, user).await, host());
    }
}
