//! havn — single-binary CLI for setup, single-user mode, and management.
//!
//! Talks to a running gateway over the management REST API for the
//! mutating subcommands (`agent`, `credential`); the `setup` and `start`
//! subcommands work locally without a running gateway.
//!
//! Override the gateway URL via `HAVN_GATEWAY`; override the gateway
//! binary path (used by `havn start`) via `HAVN_GATEWAY_BIN`.

#![allow(clippy::print_stdout, reason = "CLI legitimately writes to stdout")]

mod agent;
mod api;
mod credential;
mod doctor;
mod gateway;
mod logs;
#[cfg(target_os = "linux")]
mod nested_ns;
mod paths;
mod role;
mod setup;
mod start;
mod status;
mod team;
mod user;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "havn",
    version,
    about = "havn — multi-tenant autonomous agent platform"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initial setup: write config, prepare data directories.
    Setup {
        /// Don't prompt; accept all defaults. Use in CI / install scripts.
        #[arg(long)]
        non_interactive: bool,
    },
    /// Launch the gateway in foreground (replaces the CLI process via `exec` on Unix).
    /// Deprecated alias for `gateway start` — kept so existing scripts keep working.
    Start,
    /// Gateway lifecycle: start / stop / restart / status.
    Gateway {
        #[command(subcommand)]
        action: GatewayAction,
    },
    /// One-page system status: gateway, agents, credentials.
    Status,
    /// Tail the gateway log file (only populated when started with `gateway start --detach`).
    Logs {
        /// Number of trailing lines to print before following.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Stream new lines as they're written, like `tail -F`.
        #[arg(short, long)]
        follow: bool,
    },
    /// Diagnostic checklist: env, binaries, cgroup, DB, gateway reachability.
    Doctor,
    /// Manage agents through the gateway's management API.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Manage LLM provider credentials through the gateway's management API.
    Credential {
        #[command(subcommand)]
        action: CredentialAction,
    },
    /// Provision havn users (spec §1.7). Operator pre-populates the
    /// users table with the X-User-IDs the upstream proxy will emit;
    /// any header value that doesn't match an existing user gets 403.
    /// CLI talks directly to the gateway DB — does not require the
    /// gateway to be running, and bypasses the normal auth path.
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Manage teams (spec §10.3). Same direct-DB approach as `user`.
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },
    /// Manage roles within a team (spec §6, §10.3). Policies are
    /// JSON files passed via path; operators version their dotfiles.
    Role {
        #[command(subcommand)]
        action: RoleAction,
    },
}

#[derive(Debug, Subcommand)]
enum GatewayAction {
    /// Run the gateway. Defaults to foreground; pass --detach to background.
    Start {
        /// Run as a background daemon, redirecting stdout/stderr into
        /// `${data_dir}/gateway.log`. Foreground mode is the default and
        /// hands off to systemd / docker / dev-shell.
        #[arg(long)]
        detach: bool,
    },
    /// Send SIGTERM to the detached gateway via its PID file; SIGKILL after 10s.
    Stop,
    /// Stop (if running) then start. `--detach` controls the new instance.
    Restart {
        #[arg(long)]
        detach: bool,
    },
    /// Show pid / listen / uptime for the detached gateway.
    Status,
}

#[derive(Debug, Subcommand)]
enum UserAction {
    /// Add a user with the given X-User-ID.
    Add {
        /// X-User-ID value (UUID v7) the upstream proxy will emit.
        /// Generate one with `uuidgen`, your SSO subject mapping, etc.
        id: String,
        /// Friendly name shown in the dashboard sidebar.
        #[arg(long)]
        display_name: String,
    },
    /// List all provisioned users.
    List,
    /// Hard-delete a user. Cascades through their agents, credentials,
    /// memberships. Unrecoverable — be sure.
    Delete {
        /// X-User-ID value to remove.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum TeamAction {
    /// Create a new team. Auto-creates `admin` and `member` roles.
    Add {
        /// Team display name.
        name: String,
        /// Optional first admin user id. If omitted, the team has no
        /// members yet — add them via `add-member` next.
        #[arg(long)]
        admin: Option<String>,
    },
    /// List all teams.
    List,
    /// Delete a team. Cascades through its roles and memberships.
    Delete {
        /// Team id (UUID v7).
        id: String,
    },
    /// Add a user to a team. Role is required: pass either
    /// `--role <role-id>` (any role in the team) or one of
    /// `--as-admin` / `--as-member` to use the seeded role of that name.
    AddMember {
        /// Team id (UUID v7).
        team: String,
        /// User id (UUID v7) — must already exist (`havn user add`).
        user: String,
        /// Specific role id to assign.
        #[arg(long, conflicts_with_all = ["as_admin", "as_member"])]
        role: Option<String>,
        /// Convenience: use the team's seeded `admin` role.
        #[arg(long, conflicts_with = "as_member")]
        as_admin: bool,
        /// Convenience: use the team's seeded `member` role.
        #[arg(long)]
        as_member: bool,
    },
    /// Remove a user from a team.
    RemoveMember {
        /// Team id (UUID v7).
        team: String,
        /// User id (UUID v7).
        user: String,
    },
    /// List members of a team.
    ListMembers {
        /// Team id (UUID v7).
        team: String,
    },
}

#[derive(Debug, Subcommand)]
enum RoleAction {
    /// Add a new role to a team. If `--policy` is omitted the role
    /// gets `Policy::default()` (narrow); broaden via `set-policy`.
    Add {
        /// Team id (UUID v7).
        team: String,
        /// Role name (e.g. "auditor", "ops-readonly").
        name: String,
        /// Path to a JSON file containing the policy.
        #[arg(long)]
        policy: Option<String>,
    },
    /// List roles in a team.
    List {
        /// Team id (UUID v7).
        team: String,
    },
    /// Show one role's full details (id + policy JSON).
    Show {
        /// Role id (UUID v7).
        id: String,
    },
    /// Replace a role's policy from a JSON file.
    SetPolicy {
        /// Role id (UUID v7).
        id: String,
        /// Path to the JSON policy file.
        policy_file: String,
    },
    /// Delete a role. Refuses if any member still holds it.
    Delete {
        /// Role id (UUID v7).
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum AgentAction {
    /// List the current user's agents.
    List,
    /// Create a new agent. (Use `agent start <id>` to launch its runtime.)
    Create {
        /// Human-readable agent name.
        name: String,
    },
    /// Start an agent's runtime process.
    Start {
        /// Agent ID (UUID).
        agent_id: String,
    },
    /// Stop a running agent.
    Stop {
        /// Agent ID (UUID).
        agent_id: String,
    },
    /// Delete an agent and remove its workspace.
    Delete {
        /// Agent ID (UUID).
        agent_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum CredentialAction {
    /// List configured credentials.
    List,
    /// Add a credential.
    Add {
        /// Provider name (e.g. `anthropic`, `openai`, `channel:telegram`).
        /// v0.2 namespaces: `llm:*` (LLM keys), `channel:*` (adapter
        /// tokens), `saas:*` (OAuth2). Bare names auto-prefix to
        /// `llm:*` at gateway startup if matching the known LLM list.
        provider: String,
        /// API key (or adapter token / OAuth2 access token).
        api_key: String,
        /// Operator-supplied stable name handle (spec §7.3). Required
        /// for v0.2 channel adapter tokens and OAuth2 SaaS rows so
        /// they're addressable as `secret:<provider>:<name>`. Leave
        /// unset for v0.1-style LLM-fallback-chain rows.
        #[arg(long)]
        name: Option<String>,
        /// Priority — higher number = tried first.
        #[arg(long, default_value_t = 10)]
        priority: i32,
    },
    /// Delete a credential.
    Delete {
        /// Credential ID (UUID).
        credential_id: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Setup { non_interactive } => setup::run(!non_interactive),
        Command::Start => start::run(),
        Command::Gateway { action } => match action {
            GatewayAction::Start { detach } => gateway::start(detach),
            GatewayAction::Stop => gateway::stop(),
            GatewayAction::Restart { detach } => gateway::restart(detach),
            GatewayAction::Status => gateway::status(),
        },
        Command::Status => async_runtime()?.block_on(status::run()),
        Command::Logs { lines, follow } => logs::run(follow, lines),
        Command::Doctor => async_runtime()?.block_on(doctor::run()),
        Command::Agent { action } => async_runtime()?.block_on(run_agent(action)),
        Command::Credential { action } => async_runtime()?.block_on(run_credential(action)),
        Command::User { action } => async_runtime()?.block_on(run_user(action)),
        Command::Team { action } => async_runtime()?.block_on(run_team(action)),
        Command::Role { action } => async_runtime()?.block_on(run_role(action)),
    }
}

async fn run_team(action: TeamAction) -> anyhow::Result<()> {
    match action {
        TeamAction::Add { name, admin } => team::add(&name, admin.as_deref()).await,
        TeamAction::List => team::list().await,
        TeamAction::Delete { id } => team::delete(&id).await,
        TeamAction::AddMember {
            team: t,
            user,
            role,
            as_admin,
            as_member,
        } => {
            let role_name = if as_admin {
                Some("admin")
            } else if as_member {
                Some("member")
            } else {
                None
            };
            team::add_member(&t, &user, role.as_deref(), role_name).await
        }
        TeamAction::RemoveMember { team: t, user } => team::remove_member(&t, &user).await,
        TeamAction::ListMembers { team: t } => team::list_members(&t).await,
    }
}

async fn run_role(action: RoleAction) -> anyhow::Result<()> {
    match action {
        RoleAction::Add {
            team: t,
            name,
            policy,
        } => role::add(&t, &name, policy.as_deref()).await,
        RoleAction::List { team: t } => role::list(&t).await,
        RoleAction::Show { id } => role::show(&id).await,
        RoleAction::SetPolicy { id, policy_file } => role::set_policy(&id, &policy_file).await,
        RoleAction::Delete { id } => role::delete(&id).await,
    }
}

async fn run_user(action: UserAction) -> anyhow::Result<()> {
    match action {
        UserAction::Add { id, display_name } => user::add(&id, &display_name).await,
        UserAction::List => user::list().await,
        UserAction::Delete { id } => user::delete(&id).await,
    }
}

fn async_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}

async fn run_agent(action: AgentAction) -> anyhow::Result<()> {
    match action {
        AgentAction::List => agent::list().await,
        AgentAction::Create { name } => agent::create(&name).await,
        AgentAction::Start { agent_id } => agent::start(&agent_id).await,
        AgentAction::Stop { agent_id } => agent::stop(&agent_id).await,
        AgentAction::Delete { agent_id } => agent::delete(&agent_id).await,
    }
}

async fn run_credential(action: CredentialAction) -> anyhow::Result<()> {
    match action {
        CredentialAction::List => credential::list().await,
        CredentialAction::Add {
            provider,
            api_key,
            name,
            priority,
        } => credential::add(&provider, &api_key, priority, name.as_deref()).await,
        CredentialAction::Delete { credential_id } => credential::delete(&credential_id).await,
    }
}
