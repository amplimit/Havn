//! Chat commands (issues #173, #174): a message that is only `/name`,
//! with arguments for some, is answered by the gateway or the agent and
//! never reaches the model. One registry gives the dispatch, `/help` and
//! the command menus of channels (Telegram's `setMyCommands`).

use havn_core::AgentId;
use havn_db::repo::agents as agents_repo;
use havn_proto::{ChatCommand, ChatCommandKind};
use tracing::warn;

use crate::AppState;
use crate::model_catalog::ModelEntry;

/// One chat command.
#[derive(Debug)]
pub struct Command {
    /// Without the slash: lowercase letters, digits and `_`, as Telegram
    /// takes them.
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// How its arguments are written, empty when it takes none.
    pub args: &'static str,
    pub about: &'static str,
    /// Only for whoever administers the agent: it spends the owner's
    /// credentials or lets a step through.
    pub admin: bool,
}

/// Every command, in the order `/help` and the menus list them.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "new",
        aliases: &["reset", "clear"],
        args: "",
        about: "Start a new chat; the old one is kept",
        admin: false,
    },
    Command {
        name: "stop",
        aliases: &[],
        args: "",
        about: "Stop the reply being written",
        admin: false,
    },
    Command {
        name: "retry",
        aliases: &[],
        args: "",
        about: "Answer your last message again",
        admin: false,
    },
    Command {
        name: "undo",
        aliases: &[],
        args: "",
        about: "Take back your last message and its reply",
        admin: false,
    },
    Command {
        name: "compact",
        aliases: &[],
        args: "",
        about: "Summarise the earlier part of this chat to make room",
        admin: false,
    },
    Command {
        name: "status",
        aliases: &[],
        args: "",
        about: "This chat: the agent, its model, its messages",
        admin: false,
    },
    Command {
        name: "model",
        aliases: &[],
        args: "[name | number | default]",
        about: "Show the models, or switch this chat's",
        admin: true,
    },
    Command {
        name: "agent_model",
        aliases: &[],
        args: "[name | number | default]",
        about: "Show or switch the agent's model, for every chat",
        admin: true,
    },
    Command {
        name: "think",
        aliases: &[],
        args: "[off | on | level | tokens | default]",
        about: "Show or set how much this chat's model thinks",
        admin: true,
    },
    Command {
        name: "usage",
        aliases: &[],
        args: "[days]",
        about: "Tokens the agent used",
        admin: true,
    },
    Command {
        name: "approve",
        aliases: &[],
        args: "[always] [id]",
        about: "Let the step waiting for approval run",
        admin: true,
    },
    Command {
        name: "deny",
        aliases: &[],
        args: "[id]",
        about: "Refuse the step waiting for approval",
        admin: true,
    },
    Command {
        name: "disconnect",
        aliases: &[],
        args: "",
        about: "Disconnect this chat account from its Havn account",
        admin: false,
    },
    Command {
        name: "whoami",
        aliases: &[],
        args: "",
        about: "Who you are to this agent",
        admin: false,
    },
    Command {
        name: "help",
        aliases: &["commands"],
        args: "",
        about: "List the commands",
        admin: false,
    },
];

/// Longest command name a message is read as.
const MAX_NAME: usize = 32;
/// Days `/usage` looks back by default, and at most.
const USAGE_DAYS: u32 = 30;
const MAX_USAGE_DAYS: u32 = 365;

/// What a message is, read as a command.
#[derive(Debug)]
pub enum Parsed<'a> {
    /// A command, with what follows its name (trimmed, possibly empty).
    Known(&'static Command, &'a str),
    /// A command that takes no arguments, given some: the gateway says
    /// so, rather than do it or pass it on.
    Misused(&'static Command),
    /// `/name`, which no command is: the gateway says so, and the model
    /// never sees it.
    Unknown(&'a str),
}

impl Parsed<'_> {
    /// What the gateway answers a command it does not carry out.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::Known(..) => None,
            Self::Misused(c) => Some(format!(
                "(/{0} takes nothing after it: send /{0} on its own.)",
                c.name
            )),
            Self::Unknown(name) => Some(format!(
                "(There is no /{name} command. Send /help for the list.)"
            )),
        }
    }
}

/// `text` as a command, or `None` for an ordinary message: one line that
/// starts with `/name` or `/name@bot`, then nothing or arguments after a
/// space. A slash elsewhere, a path such as `/etc/hosts`, or more than one
/// line are ordinary text.
#[must_use]
pub fn parse(text: &str) -> Option<Parsed<'_>> {
    let text = text.trim();
    if text.contains('\n') {
        return None;
    }
    let rest = text.strip_prefix('/')?;
    let (head, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    let name = match head.split_once('@') {
        Some((name, bot)) if !bot.is_empty() && bot.chars().all(word_char) => name,
        Some(_) => return None,
        None => head,
    };
    if name.is_empty() || name.len() > MAX_NAME || !name.chars().all(word_char) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    // Telegram sends it as a user's first message to a bot: the agent
    // greets them.
    if lower == "start" {
        return None;
    }
    Some(match find(&lower) {
        Some(command) if command.args.is_empty() && !args.is_empty() => Parsed::Misused(command),
        Some(command) => Parsed::Known(command, args),
        None => Parsed::Unknown(name),
    })
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `/choose`, what a button or a poll of `ask_choice` sends (issue #204):
/// not listed with the others, as no one types it.
pub const CHOOSE: Command = Command {
    name: "choose",
    aliases: &[],
    args: "<question> <numbers>",
    about: "Answer a question the agent asked with buttons or a poll",
    admin: false,
};

/// The command named `name` (lowercase), by its name or an alias.
#[must_use]
pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS
        .iter()
        .chain(std::iter::once(&CHOOSE))
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// The commands as a channel's menu lists them.
#[must_use]
pub fn menu() -> Vec<havn_proto::channel::ChannelCommand> {
    COMMANDS
        .iter()
        .map(|c| havn_proto::channel::ChannelCommand {
            name: c.name.to_string(),
            description: c.about.to_string(),
        })
        .collect()
}

/// One command as `GET /commands` lists it.
#[derive(Debug, serde::Serialize)]
pub struct CommandView {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub args: &'static str,
    pub about: &'static str,
    pub admin: bool,
}

/// `GET /commands`: the chat commands, for the web chat's suggestions.
pub async fn list(_user: crate::auth::AuthedUser) -> axum::Json<Vec<CommandView>> {
    axum::Json(
        COMMANDS
            .iter()
            .map(|c| CommandView {
                name: c.name,
                aliases: c.aliases,
                args: c.args,
                about: c.about,
                admin: c.admin,
            })
            .collect(),
    )
}

/// `/help`: every command, the ones `admin` may not use marked.
#[must_use]
pub fn help(admin: bool) -> String {
    let mut out = String::from("Commands:\n");
    for c in COMMANDS {
        out.push('/');
        out.push_str(c.name);
        if !c.args.is_empty() {
            out.push(' ');
            out.push_str(c.args);
        }
        out.push_str(" - ");
        out.push_str(c.about);
        if !c.aliases.is_empty() {
            out.push_str(" (also /");
            out.push_str(&c.aliases.join(", /"));
            out.push(')');
        }
        if c.admin && !admin {
            out.push_str(" [admins only]");
        }
        out.push('\n');
    }
    out.push_str("Anything else you write goes to the agent.");
    out
}

/// Who sent a command, and where.
#[derive(Debug, Clone)]
pub struct Asker {
    pub agent_id: AgentId,
    /// The conversation: a channel's encoded id, or a web chat session.
    pub scope: String,
    /// The chat it was sent in, when the sender named one (the web chat).
    pub conversation: Option<String>,
    /// How the audit log and `/whoami` name the sender.
    pub who: String,
    /// May use the commands marked admin.
    pub admin: bool,
    /// What the sender would need to be one, for a refusal.
    pub admin_hint: &'static str,
}

/// What the caller does with a command [`run`] took.
#[derive(Debug)]
pub enum Action {
    /// Show the sender this.
    Reply(String),
    /// Send this to the agent, which answers in the conversation.
    Agent(ChatCommand),
    /// Send this to the agent as the sender's message: the answer to
    /// question `question`, forgotten once it reached the agent.
    Say { text: String, question: String },
}

/// What to tell the sender when `frame` cannot reach the agent but did
/// its part already: `/agent_model` saved the agent's model.
#[must_use]
pub fn saved_already(frame: &havn_proto::GatewayToAgent) -> Option<&'static str> {
    match frame {
        havn_proto::GatewayToAgent::ChatCommand(ChatCommand {
            command: ChatCommandKind::AgentModel { .. },
            ..
        }) => Some("(Saved. The agent uses that model from its next start.)"),
        _ => None,
    }
}

/// Carry out `command` with `args` for `asker`. `/new`, `/stop` and
/// `/retry` are not handled here: the channel and the web chat each have
/// their own way of doing those.
pub async fn run(state: &AppState, asker: &Asker, command: &Command, args: &str) -> Action {
    if command.admin && !asker.admin {
        return Action::Reply(format!(
            "(Only {} can use /{}.)",
            asker.admin_hint, command.name
        ));
    }
    let to_agent = |kind: ChatCommandKind| {
        Action::Agent(ChatCommand {
            channel_id: asker.scope.clone(),
            conversation: asker.conversation.clone(),
            command: kind,
        })
    };
    match command.name {
        "help" => Action::Reply(help(asker.admin)),
        "whoami" => Action::Reply(format!(
            "You are {}. {}",
            asker.who,
            if asker.admin {
                "You can use every command, the admin ones included."
            } else {
                "You can use the commands not marked [admins only] in /help."
            }
        )),
        "undo" => to_agent(ChatCommandKind::Undo),
        "compact" => to_agent(ChatCommandKind::Compact),
        // The gateway does it before a chat-app message gets here.
        "disconnect" => Action::Reply(
            "(Chat-app accounts are disconnected on an agent's settings page, under chat apps.)"
                .into(),
        ),
        "status" => {
            let agent = match agents_repo::find_by_id(&state.db, asker.agent_id).await {
                Ok(Some(agent)) => agent.name,
                _ => asker.agent_id.to_string(),
            };
            to_agent(ChatCommandKind::Status { agent })
        }
        "model" | "agent_model" => model(state, asker, command.name == "agent_model", args).await,
        "think" => {
            let choice = args.to_ascii_lowercase();
            if !choice.is_empty() && !is_thinking_choice(&choice) {
                return Action::Reply(
                    "(Send /think off, on, a level such as high, a number of tokens, or default.)"
                        .into(),
                );
            }
            to_agent(ChatCommandKind::Think {
                choice: (!choice.is_empty()).then_some(choice),
            })
        }
        "usage" => Action::Reply(usage(state, asker.agent_id, args).await),
        "choose" => match crate::choice::read_answer(args) {
            Some((id, numbers)) => {
                match state
                    .webchat_router
                    .answer_choice(asker.agent_id, &asker.scope, id, &numbers)
                {
                    Ok(text) => Action::Say {
                        text,
                        question: id.to_string(),
                    },
                    Err(why) => Action::Reply(why),
                }
            }
            None => Action::Reply("(Choose with the buttons or the poll.)".into()),
        },
        "approve" | "deny" => {
            Action::Reply(decide(state, asker, command.name == "approve", args).await)
        }
        other => {
            warn!(command = other, "a chat command with no handler");
            Action::Reply(format!("(/{other} does not work here.)"))
        }
    }
}

/// Whether `choice` reads as a thinking choice: a short lowercase word
/// or a number of tokens. Whether the chat's model takes it is decided
/// when it is asked.
#[must_use]
pub fn is_thinking_choice(choice: &str) -> bool {
    !choice.is_empty()
        && choice.len() <= 16
        && choice
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The models agent `agent_id` may use, in the gateway's order.
pub async fn usable_models(state: &AppState, agent_id: AgentId) -> Option<Vec<ModelEntry>> {
    let agent = agents_repo::find_by_id(&state.db, agent_id).await.ok()??;
    let policy =
        crate::policy_resolver::for_session(&state.db, &agent, state.trusted_proxies.is_some())
            .await;
    Some(
        state
            .models
            .load()
            .models
            .iter()
            .filter(|m| crate::llm_proxy::model_allowed(&policy.allowed_models, &m.id))
            .cloned()
            .collect(),
    )
}

/// The model `wanted` names among `models`: its number in the list, its
/// id, its label, or its name without the provider when only one has it.
fn pick<'a>(models: &'a [ModelEntry], wanted: &str) -> Option<&'a ModelEntry> {
    if let Ok(n) = wanted.parse::<usize>() {
        return n.checked_sub(1).and_then(|i| models.get(i));
    }
    let lower = wanted.to_lowercase();
    if let Some(m) = models.iter().find(|m| {
        m.id.to_lowercase() == lower
            || m.label
                .as_deref()
                .is_some_and(|l| l.to_lowercase() == lower)
    }) {
        return Some(m);
    }
    let mut short = models.iter().filter(|m| {
        m.id.split_once(':')
            .is_some_and(|(_, name)| name.to_lowercase() == lower)
    });
    match (short.next(), short.next()) {
        (Some(m), None) => Some(m),
        _ => None,
    }
}

fn listed(models: &[ModelEntry]) -> String {
    models
        .iter()
        .enumerate()
        .map(|(i, m)| match &m.label {
            Some(label) => format!("{}. {} ({label})", i + 1, m.id),
            None => format!("{}. {}", i + 1, m.id),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn model(state: &AppState, asker: &Asker, agent_wide: bool, args: &str) -> Action {
    let name = if agent_wide { "agent_model" } else { "model" };
    let Some(models) = usable_models(state, asker.agent_id).await else {
        return Action::Reply("(The agent could not be found.)".into());
    };
    if args.is_empty() {
        let current = if agent_wide {
            let model = agents_repo::find_by_id(&state.db, asker.agent_id)
                .await
                .ok()
                .flatten()
                .and_then(|a| {
                    a.config
                        .get("model")
                        .and_then(|m| m.as_str())
                        .map(String::from)
                })
                .unwrap_or_default();
            if model.is_empty() {
                "The agent uses the gateway's default model.\n".to_string()
            } else {
                format!("The agent uses {model}.\n")
            }
        } else {
            "/status shows the model this chat uses.\n".to_string()
        };
        return Action::Reply(format!(
            "{current}Models this agent may use:\n{}\nSend /{name} with a number or a name to \
             switch{}, or /{name} default to go back to {}.",
            listed(&models),
            if agent_wide {
                " every chat without a model of its own"
            } else {
                " this chat"
            },
            if agent_wide {
                "the gateway's default"
            } else {
                "the agent's model"
            },
        ));
    }
    let chosen = if matches!(args.to_ascii_lowercase().as_str(), "default" | "reset") {
        None
    } else {
        match pick(&models, args) {
            Some(m) => Some(m.id.clone()),
            None => {
                return Action::Reply(format!(
                    "(No model this agent may use is called {args:?}. Send /{name} for the list.)"
                ));
            }
        }
    };
    if !agent_wide {
        if let Ok(Some(agent)) = agents_repo::find_by_id(&state.db, asker.agent_id).await {
            crate::audit::record(
                &state.db,
                agent.owner_id,
                agent.team_id,
                Some(asker.agent_id),
                "chat.model.changed",
                serde_json::json!({ "model": chosen, "by": asker.who, "conversation": asker.scope }),
            )
            .await;
        }
        return Action::Agent(ChatCommand {
            channel_id: asker.scope.clone(),
            conversation: asker.conversation.clone(),
            command: ChatCommandKind::Model { model: chosen },
        });
    }
    // The agent's setting, as its settings page saves it, so the next
    // start keeps it; the running agent switches at once.
    let saved = agents_repo::update_config(&state.db, asker.agent_id, None, |current| {
        let mut config = current.as_object().cloned().unwrap_or_default();
        match &chosen {
            Some(model) => {
                config.insert("model".into(), serde_json::Value::String(model.clone()));
            }
            None => {
                config.remove("model");
            }
        }
        Ok::<_, std::convert::Infallible>(serde_json::Value::Object(config))
    })
    .await;
    let agent = match saved {
        Ok(Ok(agent)) => agent,
        Ok(Err(never)) => match never {},
        Err(e) => {
            warn!(agent_id = %asker.agent_id, error = %e, "could not save the agent's model");
            return Action::Reply("(The model could not be saved; nothing changed.)".into());
        }
    };
    crate::audit::record(
        &state.db,
        agent.owner_id,
        agent.team_id,
        Some(asker.agent_id),
        "agent.model.changed",
        serde_json::json!({ "model": chosen, "by": asker.who }),
    )
    .await;
    Action::Agent(ChatCommand {
        channel_id: asker.scope.clone(),
        conversation: asker.conversation.clone(),
        command: ChatCommandKind::AgentModel {
            model: chosen.unwrap_or_default(),
        },
    })
}

async fn usage(state: &AppState, agent_id: AgentId, args: &str) -> String {
    let days = if args.is_empty() {
        USAGE_DAYS
    } else {
        match args.parse::<u32>() {
            Ok(d) if (1..=MAX_USAGE_DAYS).contains(&d) => d,
            _ => return format!("(Give a number of days from 1 to {MAX_USAGE_DAYS}.)"),
        }
    };
    let since = (chrono::Utc::now() - chrono::Duration::days(i64::from(days)))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    match havn_db::repo::credential_usage::agent_usage(&state.db, agent_id, &since).await {
        Ok(rows) if rows.is_empty() => format!("The agent used no tokens in the last {days} days."),
        Ok(rows) => {
            let (calls, input, output) = rows.iter().fold((0, 0, 0), |(c, i, o), r| {
                (c + r.calls, i + r.tokens_in, o + r.tokens_out)
            });
            let mut out = format!(
                "The agent's use in the last {days} days: {calls} model calls, {input} tokens in, \
                 {output} tokens out."
            );
            for r in &rows {
                out.push_str(&format!(
                    "\n{}: {} calls, {} in, {} out",
                    r.model, r.calls, r.tokens_in, r.tokens_out
                ));
            }
            out
        }
        Err(e) => {
            warn!(%agent_id, error = %e, "could not read the agent's usage");
            "(The usage could not be read right now.)".into()
        }
    }
}

/// What `/approve` or `/deny` was given: whether to allow the step for
/// the rest of the chat, and the request it names (issue #203, as a
/// button sends it); `None` when the arguments are not those.
fn decision_args(approve: bool, args: &str) -> Option<(bool, Option<&str>)> {
    let mut remember = false;
    let mut id = None;
    for word in args.split_whitespace() {
        if approve && !remember && id.is_none() && word.eq_ignore_ascii_case("always") {
            remember = true;
        } else if id.is_none() {
            id = Some(word);
        } else {
            return None;
        }
    }
    Some((remember, id))
}

async fn decide(state: &AppState, asker: &Asker, approve: bool, args: &str) -> String {
    let Some((remember, id)) = decision_args(approve, args) else {
        return if approve {
            "(Send /approve, or /approve always to allow the same step for the rest of the \
             chat.)"
                .into()
        } else {
            "(Send /deny, or /deny with the id of the step.)".into()
        };
    };
    let waiting = match id {
        Some(id) => state
            .webchat_router
            .approval(asker.agent_id, &asker.scope, id),
        None => state
            .webchat_router
            .oldest_approval(asker.agent_id, &asker.scope),
    };
    let Some(request) = waiting else {
        return if id.is_some() {
            "(That step was already decided, or it ended.)".into()
        } else {
            "(Nothing here is waiting for approval.)".into()
        };
    };
    let decided = crate::agent_socket::forward_decision(
        &state.registry,
        &state.db,
        &state.webchat_router,
        asker.agent_id,
        &asker.who,
        &request.request_id,
        approve,
        remember,
    )
    .await;
    if !decided {
        return "(That step was decided or ended meanwhile; nothing changed.)".into();
    }
    match (approve, remember) {
        (true, true) => format!(
            "(Approved {}, and the same step for the rest of the chat.)",
            request.tool
        ),
        (true, false) => format!("(Approved {}.)", request.tool),
        (false, _) => format!("(Refused {}.)", request.tool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_and_deny_take_an_id() {
        assert_eq!(known("/deny r-1"), Some(("deny", "r-1")));
        assert_eq!(
            known("/approve always r-1"),
            Some(("approve", "always r-1"))
        );
        assert_eq!(known("/deny"), Some(("deny", "")));
    }

    #[test]
    fn a_decision_may_name_its_request() {
        assert_eq!(decision_args(true, ""), Some((false, None)));
        assert_eq!(decision_args(true, "always"), Some((true, None)));
        assert_eq!(decision_args(true, "ALWAYS r-1"), Some((true, Some("r-1"))));
        assert_eq!(decision_args(true, "r-1"), Some((false, Some("r-1"))));
        assert_eq!(decision_args(false, "r-1"), Some((false, Some("r-1"))));
        assert_eq!(decision_args(true, "r-1 r-2"), None);
        assert_eq!(decision_args(true, "always always x"), None);
    }

    fn known(text: &str) -> Option<(&'static str, &str)> {
        match parse(text) {
            Some(Parsed::Known(c, args)) => Some((c.name, args)),
            _ => None,
        }
    }

    #[test]
    fn commands_are_read_with_their_arguments() {
        assert_eq!(known("/stop"), Some(("stop", "")));
        assert_eq!(known("  /STOP "), Some(("stop", "")));
        assert_eq!(known("/stop@havn_bot"), Some(("stop", "")));
        assert_eq!(known("/clear"), Some(("new", "")));
        assert_eq!(known("/Reset"), Some(("new", "")));
        assert_eq!(known("/commands"), Some(("help", "")));
        assert_eq!(known("/model 2"), Some(("model", "2")));
        assert_eq!(
            known("/model@bot   deepseek:deepseek-chat "),
            Some(("model", "deepseek:deepseek-chat"))
        );
        assert_eq!(
            known("/agent_model default"),
            Some(("agent_model", "default"))
        );
        assert_eq!(known("/approve always"), Some(("approve", "always")));
    }

    #[test]
    fn ordinary_text_is_not_a_command() {
        for text in [
            "please /stop",
            "/stop@bot\nplease do X",
            "/stop@",
            "/etc/hosts",
            "/",
            "hello",
            "/model 2\nand more",
        ] {
            assert!(parse(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn unknown_and_misused_commands_are_answered_by_the_gateway() {
        assert!(matches!(
            parse("/teleport"),
            Some(Parsed::Unknown("teleport"))
        ));
        assert!(matches!(parse("/Foo@bot"), Some(Parsed::Unknown("Foo"))));
        assert!(matches!(
            parse("/teleport please"),
            Some(Parsed::Unknown("teleport"))
        ));
        let misused = parse("/stop now please").expect("a command");
        assert!(matches!(misused, Parsed::Misused(c) if c.name == "stop"));
        assert_eq!(
            misused.refusal().as_deref(),
            Some("(/stop takes nothing after it: send /stop on its own.)")
        );
        assert!(parse("/model 2").expect("a command").refusal().is_none());
        // Telegram's first message reaches the agent.
        assert!(parse("/start").is_none());
        assert!(parse("/start ref-42").is_none());
    }

    #[test]
    fn names_fit_telegram_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            for name in std::iter::once(&c.name).chain(c.aliases) {
                assert!(
                    name.len() <= MAX_NAME
                        && name
                            .chars()
                            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                    "{name}"
                );
                assert!(seen.insert(*name), "{name} twice");
            }
            assert!(!c.about.is_empty() && c.about.len() <= 256);
        }
        assert_eq!(menu().len(), COMMANDS.len());
    }

    fn entry(id: &str, label: Option<&str>) -> ModelEntry {
        serde_json::from_value(serde_json::json!({ "id": id, "label": label })).expect("entry")
    }

    #[test]
    fn a_model_is_picked_by_number_id_label_or_short_name() {
        let models = [
            entry("deepseek:deepseek-chat", Some("DeepSeek")),
            entry("openrouter:deepseek-chat", None),
            entry("anthropic:claude-x", None),
        ];
        assert_eq!(
            pick(&models, "1").map(|m| m.id.as_str()),
            Some("deepseek:deepseek-chat")
        );
        assert_eq!(
            pick(&models, "3").map(|m| m.id.as_str()),
            Some("anthropic:claude-x")
        );
        assert!(pick(&models, "0").is_none());
        assert!(pick(&models, "4").is_none());
        assert_eq!(
            pick(&models, "Anthropic:Claude-X").map(|m| m.id.as_str()),
            Some("anthropic:claude-x")
        );
        assert_eq!(
            pick(&models, "deepseek").map(|m| m.id.as_str()),
            Some("deepseek:deepseek-chat")
        );
        assert_eq!(
            pick(&models, "claude-x").map(|m| m.id.as_str()),
            Some("anthropic:claude-x")
        );
        assert!(
            pick(&models, "deepseek-chat").is_none(),
            "two providers have it"
        );
        assert!(pick(&models, "gpt").is_none());
    }

    #[test]
    fn help_marks_what_the_sender_may_not_use() {
        assert!(help(false).contains("/model [name | number | default] - Show the models, or switch this chat's [admins only]"));
        assert!(!help(true).contains("[admins only]"));
        assert!(help(true).contains("(also /reset, /clear)"));
    }
}
