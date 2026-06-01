//! `havn team {add,list,delete,add-member,remove-member,list-members}` —
//! operator-side team provisioning (spec §10.3).
//!
//! Same direct-DB pattern as `havn user` (cli/src/user.rs): the
//! operator runs these on the same host as the gateway and shouldn't
//! need to bootstrap an HTTP identity to provision teams. Reads are
//! always safe; destructive ops should not race with a running
//! gateway (SQLite WAL allows concurrent readers but not concurrent
//! writers when `team_memberships` cascades trigger).

use std::str::FromStr as _;

use anyhow::Context as _;
use havn_core::{Policy, RoleId, TeamId, UserId};
use havn_db::repo::roles::{NewRole, create as create_role};
use havn_db::repo::team_memberships;
use havn_db::repo::teams::{self as teams, NewTeam};

use crate::user::open_db_pub;

pub async fn add(name: &str, admin_user_id: Option<&str>) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("--name must be non-empty");
    }
    let pool = open_db_pub().await?;
    let team = teams::create(&pool, NewTeam { name: name.trim() })
        .await
        .with_context(|| format!("creating team {name:?}"))?;

    // Mirror the REST `POST /teams` flow: seed admin + member roles
    // for the team so the dashboard's role picker has something to
    // bind to. The CLI doesn't go through the API so this duplication
    // is necessary; both paths pin to the same default policies via
    // havn-core::Policy::default + targeted overrides below.
    let admin_policy = admin_default_policy();
    let member_policy = member_default_policy();
    let admin_role = create_role(
        &pool,
        NewRole {
            team_id: Some(team.id),
            name: "admin",
            policy: &admin_policy,
        },
    )
    .await
    .context("creating admin role")?;
    create_role(
        &pool,
        NewRole {
            team_id: Some(team.id),
            name: "member",
            policy: &member_policy,
        },
    )
    .await
    .context("creating member role")?;

    println!("created team {} ({})", team.id, team.name);
    println!("  - admin role: {}", admin_role.id);

    // Optional: pre-add an initial admin so the team is usable
    // immediately. Without this the only way to add a first member is
    // the REST API by an existing admin — which doesn't exist yet for
    // a freshly-created team. CLI-driven setup makes this easy.
    if let Some(uid) = admin_user_id {
        let user_id = UserId::from_str(uid)
            .map_err(|_| anyhow::anyhow!("--admin must be a UUID v7 (got {uid:?})"))?;
        team_memberships::add(&pool, user_id, team.id, admin_role.id)
            .await
            .with_context(|| format!("adding {user_id} as admin of {}", team.id))?;
        println!("  - admin: {user_id}");
    }
    Ok(())
}

pub async fn list() -> anyhow::Result<()> {
    let pool = open_db_pub().await?;
    let rows = teams::list_all(&pool).await.context("listing teams")?;
    if rows.is_empty() {
        println!("(no teams)");
        return Ok(());
    }
    for t in rows {
        println!("{}  {:24}  {}", t.id, t.name, t.created_at);
    }
    Ok(())
}

pub async fn delete(id: &str) -> anyhow::Result<()> {
    let team_id = TeamId::from_str(id)
        .map_err(|_| anyhow::anyhow!("team id must be a UUID v7 (got {id:?})"))?;
    let pool = open_db_pub().await?;
    teams::delete(&pool, team_id)
        .await
        .with_context(|| format!("deleting team {team_id}"))?;
    println!("deleted team {team_id} (members and team-scoped roles cascaded)");
    Ok(())
}

pub async fn add_member(
    team: &str,
    user: &str,
    role: Option<&str>,
    role_name: Option<&str>,
) -> anyhow::Result<()> {
    let team_id = TeamId::from_str(team)
        .map_err(|_| anyhow::anyhow!("team id must be a UUID v7 (got {team:?})"))?;
    let user_id = UserId::from_str(user)
        .map_err(|_| anyhow::anyhow!("user id must be a UUID v7 (got {user:?})"))?;
    let pool = open_db_pub().await?;

    // Resolve the role: explicit --role-id wins; --as-admin / --as-member
    // resolve to the team's seeded role of that name.
    let role_id = match (role, role_name) {
        (Some(r), _) => RoleId::from_str(r)
            .map_err(|_| anyhow::anyhow!("--role must be a UUID v7 (got {r:?})"))?,
        (None, Some(name)) => {
            havn_db::repo::roles::find_by_team_and_name(&pool, Some(team_id), name)
                .await
                .with_context(|| format!("looking up team role {name:?}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "team {team_id} has no role named {name:?} — pass --role <id> instead"
                    )
                })?
                .id
        }
        (None, None) => {
            anyhow::bail!("specify either --role <role-id> or --as-admin / --as-member")
        }
    };

    team_memberships::add(&pool, user_id, team_id, role_id)
        .await
        .with_context(|| format!("adding {user_id} to {team_id}"))?;
    println!("added user {user_id} to team {team_id} as role {role_id}");
    Ok(())
}

pub async fn remove_member(team: &str, user: &str) -> anyhow::Result<()> {
    let team_id = TeamId::from_str(team)
        .map_err(|_| anyhow::anyhow!("team id must be a UUID v7 (got {team:?})"))?;
    let user_id = UserId::from_str(user)
        .map_err(|_| anyhow::anyhow!("user id must be a UUID v7 (got {user:?})"))?;
    let pool = open_db_pub().await?;
    team_memberships::remove(&pool, user_id, team_id)
        .await
        .with_context(|| format!("removing {user_id} from {team_id}"))?;
    println!("removed user {user_id} from team {team_id}");
    Ok(())
}

pub async fn list_members(team: &str) -> anyhow::Result<()> {
    let team_id = TeamId::from_str(team)
        .map_err(|_| anyhow::anyhow!("team id must be a UUID v7 (got {team:?})"))?;
    let pool = open_db_pub().await?;
    let rows = team_memberships::list_for_team(&pool, team_id)
        .await
        .with_context(|| format!("listing members of {team_id}"))?;
    if rows.is_empty() {
        println!("(no members)");
        return Ok(());
    }
    for m in rows {
        println!(
            "{}  {:24}  role={:8}  joined={}",
            m.user_id, m.user_display_name, m.role_name, m.joined_at
        );
    }
    Ok(())
}

// Defaults mirror the REST handler's so `havn team add` and
// `POST /teams` produce identical seeded roles. Promote to a shared
// helper if a third caller appears.
fn admin_default_policy() -> Policy {
    let mut p = Policy::default();
    p.max_agents = 50;
    p.permissions.can_use_shell = true;
    p.permissions.can_spawn_subagents = true;
    p.admin_visibility.can_view_audit_log = true;
    p
}

fn member_default_policy() -> Policy {
    let mut p = Policy::default();
    p.max_agents = 5;
    p.permissions.can_use_shell = false;
    p.permissions.can_spawn_subagents = false;
    p.admin_visibility.can_view_audit_log = false;
    p
}
