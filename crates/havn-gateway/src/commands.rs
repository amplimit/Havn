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
    /// `about` in Chinese (issue #250), for `/help` to an agent in Chinese.
    pub about_zh: &'static str,
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
        about_zh: "开始一个新对话，旧的保留",
        admin: false,
    },
    Command {
        name: "stop",
        aliases: &[],
        args: "",
        about: "Stop the reply being written",
        about_zh: "停止正在写的回复",
        admin: false,
    },
    Command {
        name: "retry",
        aliases: &[],
        args: "",
        about: "Answer your last message again",
        about_zh: "重新回答你的上一条消息",
        admin: false,
    },
    Command {
        name: "undo",
        aliases: &[],
        args: "",
        about: "Take back your last message and its reply",
        about_zh: "撤回你的上一条消息和它的回复",
        admin: false,
    },
    Command {
        name: "compact",
        aliases: &[],
        args: "",
        about: "Summarise the earlier part of this chat to make room",
        about_zh: "把这个对话较早的部分压缩成摘要，腾出空间",
        admin: false,
    },
    Command {
        name: "status",
        aliases: &[],
        args: "",
        about: "This chat: the agent, its model, its messages",
        about_zh: "这个对话：agent、模型和消息数",
        admin: false,
    },
    Command {
        name: "model",
        aliases: &[],
        args: "[name | number | default]",
        about: "Show the models, or switch this chat's",
        about_zh: "列出模型，或切换这个对话的模型",
        admin: true,
    },
    Command {
        name: "agent_model",
        aliases: &[],
        args: "[name | number | default]",
        about: "Show or switch the agent's model, for every chat",
        about_zh: "列出或切换 agent 的模型，对所有对话生效",
        admin: true,
    },
    Command {
        name: "think",
        aliases: &[],
        args: "[off | on | level | tokens | default]",
        about: "Show or set how much this chat's model thinks",
        about_zh: "查看或设置这个对话的模型思考多少",
        admin: true,
    },
    Command {
        name: "usage",
        aliases: &[],
        args: "[days]",
        about: "Tokens the agent used",
        about_zh: "agent 用掉的 token",
        admin: true,
    },
    Command {
        name: "approve",
        aliases: &[],
        args: "[always] [id]",
        about: "Let the step waiting for approval run",
        about_zh: "让等待批准的步骤执行",
        admin: true,
    },
    Command {
        name: "deny",
        aliases: &[],
        args: "[id]",
        about: "Refuse the step waiting for approval",
        about_zh: "拒绝等待批准的步骤",
        admin: true,
    },
    Command {
        name: "allow",
        aliases: &[],
        args: "<sender>",
        about: "Let someone the bot turned away talk to it",
        about_zh: "让被机器人拒绝的人可以和它对话",
        admin: true,
    },
    Command {
        name: "disconnect",
        aliases: &[],
        args: "",
        about: "Disconnect this chat account from its Havn account",
        about_zh: "断开这个聊天账号和 Havn 账号的关联",
        admin: false,
    },
    Command {
        name: "whoami",
        aliases: &[],
        args: "",
        about: "Who you are to this agent",
        about_zh: "你对这个 agent 来说是谁",
        admin: false,
    },
    Command {
        name: "help",
        aliases: &["commands"],
        args: "",
        about: "List the commands",
        about_zh: "列出所有命令",
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
    pub fn refusal(&self, language: havn_core::Language) -> Option<String> {
        match self {
            Self::Known(..) => None,
            Self::Misused(c) => Some(language.pick(
                format!(
                    "(/{0} takes nothing after it: send /{0} on its own.)",
                    c.name
                ),
                format!("（/{0} 后面不带任何内容：单独发送 /{0}。）", c.name),
            )),
            Self::Unknown(name) => Some(language.pick(
                format!("(There is no /{name} command. Send /help for the list.)"),
                format!("（没有 /{name} 这个命令。发 /help 查看命令列表。）"),
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
    about_zh: "用按钮或投票回答 agent 提的问题",
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
pub fn help(admin: bool, language: havn_core::Language) -> String {
    let mut out = String::from(language.pick("Commands:\n", "命令：\n"));
    for c in COMMANDS {
        out.push('/');
        out.push_str(c.name);
        if !c.args.is_empty() {
            out.push(' ');
            out.push_str(c.args);
        }
        out.push_str(" - ");
        out.push_str(language.pick(c.about, c.about_zh));
        if !c.aliases.is_empty() {
            out.push_str(language.pick(" (also /", "（也可以用 /"));
            out.push_str(&c.aliases.join(", /"));
            out.push_str(language.pick(")", "）"));
        }
        if c.admin && !admin {
            out.push_str(language.pick(" [admins only]", " [仅限管理员]"));
        }
        out.push('\n');
    }
    out.push_str(language.pick(
        "Anything else you write goes to the agent.",
        "其他任何内容都会发给 agent。",
    ));
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
    /// How the audit log names the sender.
    pub who: String,
    /// How `/whoami` names the sender, in the agent's language.
    pub who_shown: String,
    /// May use the commands marked admin.
    pub admin: bool,
    /// Is the agent's owner, who decides from their own conversation what
    /// may run and be read in someone else's (issue #255).
    pub owner: bool,
    /// What the sender would need to be one, for a refusal.
    pub admin_hint: &'static str,
    /// The agent's language, which its replies are in (issue #250).
    pub language: havn_core::Language,
}

/// The language of agent `agent_id` (issue #250); English when it cannot
/// be read.
pub async fn language_of(state: &AppState, agent_id: AgentId) -> havn_core::Language {
    agents_repo::find_by_id(&state.db, agent_id)
        .await
        .ok()
        .flatten()
        .map(|a| havn_core::Language::of_config(&a.config))
        .unwrap_or_default()
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
pub fn saved_already(
    frame: &havn_proto::GatewayToAgent,
    language: havn_core::Language,
) -> Option<&'static str> {
    match frame {
        havn_proto::GatewayToAgent::ChatCommand(ChatCommand {
            command: ChatCommandKind::AgentModel { .. },
            ..
        }) => Some(language.pick(
            "(Saved. The agent uses that model from its next start.)",
            "（已保存。agent 下次启动时开始使用这个模型。）",
        )),
        _ => None,
    }
}

/// Carry out `command` with `args` for `asker`. `/new`, `/stop` and
/// `/retry` are not handled here: the channel and the web chat each have
/// their own way of doing those.
pub async fn run(state: &AppState, asker: &Asker, command: &Command, args: &str) -> Action {
    let language = asker.language;
    if command.admin && !asker.admin {
        return Action::Reply(language.pick(
            format!("(Only {} can use /{}.)", asker.admin_hint, command.name),
            format!("（只有 {} 可以使用 /{}。）", asker.admin_hint, command.name),
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
        "help" => Action::Reply(help(asker.admin, language)),
        "whoami" => Action::Reply(match language {
            havn_core::Language::English => format!(
                "You are {}. {}",
                asker.who_shown,
                if asker.admin {
                    "You can use every command, the admin ones included."
                } else {
                    "You can use the commands not marked [admins only] in /help."
                }
            ),
            havn_core::Language::Chinese => format!(
                "你是 {}。{}",
                asker.who_shown,
                if asker.admin {
                    "你可以使用所有命令，包括管理员命令。"
                } else {
                    "你可以使用 /help 里没有标「仅限管理员」的命令。"
                }
            ),
        }),
        "undo" => to_agent(ChatCommandKind::Undo),
        "compact" => to_agent(ChatCommandKind::Compact),
        // The gateway does it before a chat-app message gets here.
        "disconnect" => Action::Reply(
            language.pick("(Chat-app accounts are disconnected on an agent's settings page, under chat apps.)", "（聊天应用账号在 agent 设置页的「chat apps」里断开。）")
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
                    language.pick("(Send /think off, on, a level such as high, a number of tokens, or default.)", "（发 /think 加 off、on、一个级别（比如 high）、一个 token 数，或者 default。）")
                        .into(),
                );
            }
            to_agent(ChatCommandKind::Think {
                choice: (!choice.is_empty()).then_some(choice),
            })
        }
        "usage" => Action::Reply(usage(state, asker.agent_id, args, language).await),
        "choose" => match crate::choice::read_answer(args) {
            Some((id, numbers)) => {
                match state
                    .webchat_router
                    .answer_choice(asker.agent_id, &asker.scope, id, &numbers, language)
                {
                    Ok(text) => Action::Say {
                        text,
                        question: id.to_string(),
                    },
                    Err(why) => Action::Reply(why),
                }
            }
            None => Action::Reply(language.pick("(Choose with the buttons or the poll.)", "（请用按钮或投票选择。）").into()),
        },
        "approve" | "deny" => {
            Action::Reply(decide(state, asker, command.name == "approve", args).await)
        }
        "allow" => Action::Reply(allow(state, asker, args).await),
        other => {
            warn!(command = other, "a chat command with no handler");
            Action::Reply(language.pick(format!("(/{other} does not work here.)"), format!("（/{other} 在这里不能用。）")))
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
    let language = asker.language;
    let name = if agent_wide { "agent_model" } else { "model" };
    let Some(models) = usable_models(state, asker.agent_id).await else {
        return Action::Reply(
            language
                .pick("(The agent could not be found.)", "（找不到这个 agent。）")
                .into(),
        );
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
                language
                    .pick(
                        "The agent uses the gateway's default model.\n",
                        "agent 使用网关的默认模型。\n",
                    )
                    .to_string()
            } else {
                language.pick(
                    format!("The agent uses {model}.\n"),
                    format!("agent 使用 {model}。\n"),
                )
            }
        } else {
            language
                .pick(
                    "/status shows the model this chat uses.\n",
                    "/status 显示这个对话使用的模型。\n",
                )
                .to_string()
        };
        let list = listed(&models);
        return Action::Reply(match (language, agent_wide) {
            (havn_core::Language::English, wide) => format!(
                "{current}Models this agent may use:\n{list}\nSend /{name} with a number or a \
                 name to switch{}, or /{name} default to go back to {}.",
                if wide {
                    " every chat without a model of its own"
                } else {
                    " this chat"
                },
                if wide {
                    "the gateway's default"
                } else {
                    "the agent's model"
                },
            ),
            (havn_core::Language::Chinese, true) => format!(
                "{current}这个 agent 可以使用的模型：\n{list}\n发 /{name} 加编号或名字，切换所有\
                 没有单独设置模型的对话；发 /{name} default 恢复为网关的默认模型。"
            ),
            (havn_core::Language::Chinese, false) => format!(
                "{current}这个 agent 可以使用的模型：\n{list}\n发 /{name} 加编号或名字，切换这个\
                 对话的模型；发 /{name} default 恢复为 agent 的模型。"
            ),
        });
    }
    let chosen = if matches!(args.to_ascii_lowercase().as_str(), "default" | "reset") {
        None
    } else {
        match pick(&models, args) {
            Some(m) => Some(m.id.clone()),
            None => {
                return Action::Reply(language.pick(format!(
                    "(No model this agent may use is called {args:?}. Send /{name} for the list.)"
                ), format!(
                    "（这个 agent 能用的模型里没有叫 {args:?} 的。发 /{name} 查看列表。）"
                )));
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
            return Action::Reply(
                language
                    .pick(
                        "(The model could not be saved; nothing changed.)",
                        "（模型没能保存，什么都没改。）",
                    )
                    .into(),
            );
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

async fn usage(
    state: &AppState,
    agent_id: AgentId,
    args: &str,
    language: havn_core::Language,
) -> String {
    let days = if args.is_empty() {
        USAGE_DAYS
    } else {
        match args.parse::<u32>() {
            Ok(d) if (1..=MAX_USAGE_DAYS).contains(&d) => d,
            _ => {
                return language.pick(
                    format!("(Give a number of days from 1 to {MAX_USAGE_DAYS}.)"),
                    format!("（请给出 1 到 {MAX_USAGE_DAYS} 之间的天数。）"),
                );
            }
        }
    };
    let since = (chrono::Utc::now() - chrono::Duration::days(i64::from(days)))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    match havn_db::repo::credential_usage::agent_usage(&state.db, agent_id, &since).await {
        Ok(rows) if rows.is_empty() => language.pick(
            format!("The agent used no tokens in the last {days} days."),
            format!("agent 最近 {days} 天没有用掉 token。"),
        ),
        Ok(rows) => {
            let (calls, input, output) = rows.iter().fold((0, 0, 0), |(c, i, o), r| {
                (c + r.calls, i + r.tokens_in, o + r.tokens_out)
            });
            let mut out = language.pick(format!(
                "The agent's use in the last {days} days: {calls} model calls, {input} tokens in, \
                 {output} tokens out."
            ), format!(
                "agent 最近 {days} 天的用量：调用模型 {calls} 次，输入 {input} 个 token，输出 {output} 个 token。"
            ));
            for r in &rows {
                out.push_str(&language.pick(
                    format!(
                        "\n{}: {} calls, {} in, {} out",
                        r.model, r.calls, r.tokens_in, r.tokens_out
                    ),
                    format!(
                        "\n{}：{} 次调用，输入 {}，输出 {}",
                        r.model, r.calls, r.tokens_in, r.tokens_out
                    ),
                ));
            }
            out
        }
        Err(e) => {
            warn!(%agent_id, error = %e, "could not read the agent's usage");
            language
                .pick(
                    "(The usage could not be read right now.)",
                    "（现在读不到用量。）",
                )
                .into()
        }
    }
}

/// What `/approve` or `/deny` was given: whether to allow the step for
/// the rest of the chat, the request it names (issue #203, as a button
/// sends it), and, after the request, a note for an approval of a call in
/// someone else's conversation (issue #255); `None` when the arguments are
/// not those.
fn decision_args(approve: bool, args: &str) -> Option<(bool, Option<&str>, Option<&str>)> {
    let mut remember = false;
    let mut rest = args.trim_start();
    if approve
        && let Some((word, after)) = rest.split_once(char::is_whitespace).or(Some((rest, "")))
        && word.eq_ignore_ascii_case("always")
    {
        remember = true;
        rest = after.trim_start();
    }
    if rest.is_empty() {
        return Some((remember, None, None));
    }
    let (id, note) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let note = note.trim();
    if !approve && !note.is_empty() {
        return None;
    }
    Some((remember, Some(id), (!note.is_empty()).then_some(note)))
}

/// `/allow <sender>` (issue #231): the agent's owner lets a sender the
/// bot turned away talk to it, from their own chat with the bot, where
/// the notice's button sends it.
async fn allow(state: &AppState, asker: &Asker, args: &str) -> String {
    let language = asker.language;
    if !asker.owner {
        return language
            .pick(
                "(Only the agent's owner can let someone in.)",
                "（只有 agent 的所有者能放行别人。）",
            )
            .into();
    }
    let Some((channel, account_id, _)) = crate::channel_id_codec::decode(&asker.scope) else {
        return language
            .pick(
                "(Send it in your chat with the bot they wrote to, or use the agent's settings page.)",
                "（请在对方联系的那个机器人的对话里发送，或者用 agent 的设置页。）",
            )
            .into();
    };
    let sender = args.trim();
    if sender.is_empty()
        || sender.chars().any(char::is_whitespace)
        || crate::channel_id_codec::validate_channel_id_part("sender_id", sender).is_err()
    {
        return language
            .pick(
                "(Send /allow and who, such as /allow tg:123.)",
                "（发 /allow 加对方，比如 /allow tg:123。）",
            )
            .into();
    }
    let Ok(Some(agent)) = agents_repo::find_by_id(&state.db, asker.agent_id).await else {
        return language
            .pick("(The agent is gone.)", "（这个 agent 不在了。）")
            .into();
    };
    match crate::api::channel::admit_sender(state, &agent, channel, account_id, sender, &asker.who)
        .await
    {
        Ok(true) => language.pick(
            format!("({sender} can talk to this bot now.)"),
            format!("（{sender} 现在可以和这个机器人对话了。）"),
        ),
        Ok(false) => language.pick(
            format!("({sender} could already talk to this bot.)"),
            format!("（{sender} 本来就可以和这个机器人对话。）"),
        ),
        Err(e) => language.pick(
            format!("(Could not let {sender} in: {e}.)"),
            format!("（没能放行 {sender}：{e}。）"),
        ),
    }
}

async fn decide(state: &AppState, asker: &Asker, approve: bool, args: &str) -> String {
    let language = asker.language;
    let Some((remember, id, note)) = decision_args(approve, args) else {
        return if approve {
            language.pick("(Send /approve, or /approve always to allow the same step for the rest of the \
             chat.)", "（发 /approve，或者发 /approve always，在这个对话剩下的时间里都允许同样的步骤。）")
                .into()
        } else {
            language
                .pick(
                    "(Send /deny, or /deny with the id of the step.)",
                    "（发 /deny，或者 /deny 加步骤的 id。）",
                )
                .into()
        };
    };
    let waiting = match id {
        Some(id) => state
            .webchat_router
            .approval(asker.agent_id, &asker.scope, id)
            // A call in someone else's conversation waits there; the
            // owner decides it from their own.
            .or_else(|| {
                asker
                    .owner
                    .then(|| state.webchat_router.owner_approval(asker.agent_id, id))
                    .flatten()
            }),
        None => state
            .webchat_router
            .oldest_approval(asker.agent_id, &asker.scope),
    };
    // Decided by the owner only, never in the conversation it is for.
    if let Some(request) = &waiting
        && !request.kind.is_empty()
        && !asker.owner
    {
        return language
            .pick(
                "(Only the agent's owner decides this, from their own chat.)",
                "（这个只能由 agent 的所有者在自己的对话里决定。）",
            )
            .into();
    }
    let Some(request) = waiting else {
        return if id.is_some() {
            language
                .pick(
                    "(That step was already decided, or it ended.)",
                    "（那个步骤已经有了决定，或者已经结束。）",
                )
                .into()
        } else {
            language
                .pick(
                    "(Nothing here is waiting for approval.)",
                    "（这里没有等待批准的步骤。）",
                )
                .into()
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
        note.map(str::to_string),
    )
    .await;
    if !decided {
        return language
            .pick(
                "(That step was decided or ended meanwhile; nothing changed.)",
                "（那个步骤在这期间已经有了决定或已经结束，什么都没改。）",
            )
            .into();
    }
    match (approve, remember) {
        (true, true) if request.kind == "result" => language.pick(
            format!(
                "(Allowed: {} answers in that conversation from now on, without asking.)",
                request.tool
            ),
            format!(
                "（已允许：从现在起，那个对话里的 {} 不再询问，直接使用。）",
                request.tool
            ),
        ),
        (true, true) if request.kind == "call" => language.pick(
            format!(
                "(Allowed: this exact {} call runs in that conversation from now on, without \
                 asking.)",
                request.tool
            ),
            format!(
                "（已允许：从现在起，那个对话里完全相同的这一条 {} 不再询问，直接运行。）",
                request.tool
            ),
        ),
        (true, false) if !request.kind.is_empty() => language
            .pick("(Allowed this once.)", "（已允许这一次。）")
            .to_string(),
        (true, true) => language.pick(
            format!(
                "(Approved {}, and the same step for the rest of the chat.)",
                request.tool
            ),
            format!(
                "（已批准 {}，在这个对话剩下的时间里同样的步骤也会被允许。）",
                request.tool
            ),
        ),
        (true, false) => language.pick(
            format!("(Approved {}.)", request.tool),
            format!("（已批准 {}。）", request.tool),
        ),
        (false, _) => language.pick(
            format!("(Refused {}.)", request.tool),
            format!("（已拒绝 {}。）", request.tool),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_in_chinese_answers_commands_in_chinese() {
        let zh = havn_core::Language::Chinese;
        let help = help(false, zh);
        assert!(help.starts_with("命令："), "{help}");
        assert!(help.contains("/new - 开始一个新对话，旧的保留（也可以用 /reset, /clear）"));
        assert!(help.contains("[仅限管理员]"));
        assert!(!help.contains("Start a new chat"));
        assert!(
            COMMANDS.iter().all(|c| !c.about_zh.is_empty()),
            "every command has one"
        );
        let unknown = parse("/nope")
            .expect("a command")
            .refusal(zh)
            .expect("refused");
        assert_eq!(unknown, "（没有 /nope 这个命令。发 /help 查看命令列表。）");
    }

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
        assert_eq!(decision_args(true, ""), Some((false, None, None)));
        assert_eq!(decision_args(true, "always"), Some((true, None, None)));
        assert_eq!(
            decision_args(true, "ALWAYS r-1"),
            Some((true, Some("r-1"), None))
        );
        assert_eq!(decision_args(true, "r-1"), Some((false, Some("r-1"), None)));
        assert_eq!(
            decision_args(false, "r-1"),
            Some((false, Some("r-1"), None))
        );
        // After the request, a note for an approval of a call in someone
        // else's conversation (issue #255).
        assert_eq!(
            decision_args(true, "r-1  not my address "),
            Some((false, Some("r-1"), Some("not my address")))
        );
        assert_eq!(
            decision_args(true, "always r-1 别提我的工资"),
            Some((true, Some("r-1"), Some("别提我的工资")))
        );
        assert_eq!(
            decision_args(false, "r-1 why"),
            None,
            "a refusal takes none"
        );
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
            misused.refusal(havn_core::Language::English).as_deref(),
            Some("(/stop takes nothing after it: send /stop on its own.)")
        );
        assert!(
            parse("/model 2")
                .expect("a command")
                .refusal(havn_core::Language::English)
                .is_none()
        );
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
        assert!(help(false, havn_core::Language::English).contains("/model [name | number | default] - Show the models, or switch this chat's [admins only]"));
        assert!(!help(true, havn_core::Language::English).contains("[admins only]"));
        assert!(help(true, havn_core::Language::English).contains("(also /reset, /clear)"));
    }
}
