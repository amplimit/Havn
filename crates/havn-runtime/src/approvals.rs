//! Tool calls that wait for approval (issue #127). A hook's `ask` rule
//! that matches pauses the call: the runtime sends an [`ApprovalRequest`]
//! with the call's whole input, the gateway asks the agent's owner, and
//! their [`ApprovalDecision`] lets the call run or refuses it. With no one
//! to ask (a scheduled run, a heartbeat, another agent's query), an input
//! too long to show, or no answer in time, the call is refused.
//!
//! In a conversation that is not the owner's (issue #255), a tool that
//! runs or changes things waits for the owner before it runs, and one
//! that reads waits after, with all of what the model would get: see
//! [`Review`] and [`guarded`].

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use havn_proto::{AgentToGateway, ApprovalDecision, ApprovalRequest};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::tools::ToolResult;

/// How long a call waits for approval before it is refused.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Longest input that can be approved; the owner sees all of it.
pub const MAX_SHOWN_INPUT: usize = 4000;
/// Characters of an owner's note kept.
const MAX_NOTE_CHARS: usize = 1000;

#[derive(Debug)]
struct Waiting {
    channel_id: String,
    /// The chat a remembered approval holds for.
    chat: String,
    tool: String,
    input: String,
    /// Its "always" is kept by the one that asked, not for the chat.
    kept: bool,
    reply: oneshot::Sender<ApprovalDecision>,
}

/// The owner's answer about a call in someone else's conversation (issue
/// #255).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// Always allow it in that conversation.
    pub always: bool,
    /// What the owner added, such as what not to say.
    pub note: Option<String>,
}

/// When the owner looks at a call in a conversation that is not theirs
/// (issue #255).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Review {
    /// Before it runs, at its whole input: it runs or changes something.
    Call,
    /// After it ran, at all of what the model would get: it reads.
    Result,
}

impl Review {
    /// How the owner looks at `tool` in someone else's conversation;
    /// `None` for a tool that needs no look (memory changes are the
    /// owner's only already).
    #[must_use]
    pub fn of(tool: &str) -> Option<Self> {
        match tool {
            "memory_search" | "file_read" | "file_find" | "file_search" => Some(Self::Result),
            "shell" | "process" | "file_write" | "file_edit" | "skill_manage" | "send_file"
            | "set_timezone" | "view_image" => Some(Self::Call),
            _ if tool.starts_with("mcp__") => Some(Self::Call),
            _ => None,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Result => "result",
        }
    }
}

/// What the owner refused in one turn (issue #255), and its questions,
/// one at a time (calls side by side, subagents).
#[derive(Debug, Default)]
pub struct Refused {
    tools: Mutex<HashSet<String>>,
    asking: tokio::sync::Mutex<()>,
}

impl Refused {
    fn lock_tools(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `run`, `tool` called with `input` in a turn that is not the owner's
/// (issue #255): once the owner, shown who asked and what they wrote,
/// allows it, before it runs for a [`Review::Call`] and after it ran, at
/// all of what the model gets, for a [`Review::Result`]; or when they
/// always allow it in that conversation, unless `must_ask` (a rule asks
/// about every such call). The model gets the owner's note with it.
pub async fn guarded(
    approvals: Option<&Approvals>,
    db: &sqlx::SqlitePool,
    review: Review,
    tool: &str,
    input: &serde_json::Value,
    must_ask: bool,
    run: impl std::future::Future<Output = ToolResult>,
) -> ToolResult {
    let lang = crate::lang();
    let Ok(scope) = crate::tools::gateway::CONVERSATION.try_with(Clone::clone) else {
        return ToolResult::Error(lang.pick(
            format!(
                "{tool} was not run: in a conversation that is not your owner's it needs their \
                 approval, and no one can be asked here (another agent's question, or a task \
                 someone else scheduled)"
            ),
            format!(
                "{tool} 没有运行：在不是所有者的对话里，它需要所有者批准，而这里没有人能问（另一个 \
                 agent 的询问，或者别人建的定时任务）"
            ),
        ));
    };
    let chat = crate::tools::gateway::CHAT.try_with(Clone::clone).ok();
    let whole = input.to_string();
    // "Always" holds for a tool's results, but for one exact call of a
    // tool that runs things: else anyone there could run anything.
    let key = match review {
        Review::Call => whole.as_str(),
        Review::Result => "",
    };
    // What is always allowed does not wait behind a question.
    match allowed(db, &scope, tool, key, must_ask).await {
        Ok(Some(rule)) => return with_rule(run.await, Some(&rule)),
        Ok(None) => {}
        Err(failed) => return failed,
    }
    let refused = crate::tools::gateway::REFUSED
        .try_with(std::sync::Arc::clone)
        .ok();
    let asking = match &refused {
        Some(r) => Some(r.asking.lock().await),
        None => None,
    };
    if refused
        .as_ref()
        .is_some_and(|r| r.lock_tools().contains(tool))
    {
        return refusal(
            review,
            tool,
            lang.pick(
                "your owner did not allow it earlier in this reply",
                "所有者在这次回复里已经没有允许",
            ),
        );
    }
    // Allowed while this call waited for the lock.
    match allowed(db, &scope, tool, key, must_ask).await {
        Ok(Some(rule)) => {
            drop(asking);
            return with_rule(run.await, Some(&rule));
        }
        Ok(None) => {}
        Err(failed) => return failed,
    }
    let Some(approvals) = approvals else {
        return ToolResult::Error(format!("{tool} was not run: no one can be asked"));
    };
    let (from, said) = crate::tools::gateway::ASKED
        .try_with(Clone::clone)
        .unwrap_or_default();
    let shown = |found: Option<&str>| {
        let mut shown =
            serde_json::json!({ "from": from, "said": said, "tool": tool, "input": input });
        if let Some(found) = found {
            shown["found"] = found.into();
        }
        shown.to_string()
    };
    let refuse = |why: &str| {
        if let Some(refused) = &refused {
            refused.lock_tools().insert(tool.to_string());
        }
        refusal(review, tool, why)
    };
    match review {
        Review::Call => {
            let answer = approvals
                .ask_for(&scope, chat.as_deref(), review, tool, &shown(None))
                .await;
            drop(asking);
            match answer {
                Ok(answer) => {
                    if !must_ask {
                        keep(db, &scope, tool, key, &answer).await;
                    }
                    with_rule(run.await, answer.note.as_deref())
                }
                Err(why) => refuse(&why),
            }
        }
        Review::Result => {
            let found = match run.await {
                ToolResult::Ok(found) => found,
                failed @ ToolResult::Error(_) => return failed,
                // The owner looks at text: an image would reach the model
                // unseen.
                ToolResult::Image { .. } => {
                    return ToolResult::Error(
                        "an image cannot be shown to the owner to look at first".into(),
                    );
                }
            };
            // What the owner sees is what the model gets: what does not
            // fit is left out of both, and both are told.
            let total = found.chars().count();
            let left_out = lang.pick(
                "\n(The rest was left out: too long to show.)",
                "\n（其余部分太长，没有显示。）",
            );
            let chars: Vec<char> = found.chars().collect();
            let given_of = |n: usize| {
                let part: String = chars[..n].iter().collect();
                if n < total {
                    format!("{part}{left_out}")
                } else {
                    part
                }
            };
            let fits = |n: usize| shown(Some(&given_of(n))).chars().count() <= MAX_SHOWN_INPUT;
            // All of it when it fits, else the longest part that fits with
            // the note, as shown with its escapes.
            let (mut low, mut high) = (0, total.saturating_sub(1).min(MAX_SHOWN_INPUT));
            if fits(total) {
                low = total;
                high = total;
            }
            while low < high {
                let mid = (low + high).div_ceil(2);
                if fits(mid) {
                    low = mid;
                } else {
                    high = mid - 1;
                }
            }
            let given = given_of(low);
            match approvals
                .ask_for(&scope, chat.as_deref(), review, tool, &shown(Some(&given)))
                .await
            {
                Ok(answer) => {
                    if !must_ask {
                        keep(db, &scope, tool, key, &answer).await;
                    }
                    with_rule(ToolResult::Ok(given), answer.note.as_deref())
                }
                Err(why) => refuse(&why),
            }
        }
    }
}

/// The rule of what is always allowed for `tool` with `key` in `scope`;
/// none under a rule that asks every time.
async fn allowed(
    db: &sqlx::SqlitePool,
    scope: &str,
    tool: &str,
    key: &str,
    must_ask: bool,
) -> Result<Option<String>, ToolResult> {
    if must_ask {
        return Ok(None);
    }
    havn_db::agent::conversations::access_of(db, scope, tool, key)
        .await
        .map_err(|e| ToolResult::Error(format!("{tool} was not run: {e}")))
}

/// Keep what the owner always allows from now on.
async fn keep(db: &sqlx::SqlitePool, scope: &str, tool: &str, key: &str, answer: &Answer) {
    if answer.always
        && let Err(e) = havn_db::agent::conversations::allow(
            db,
            scope,
            tool,
            key,
            answer.note.as_deref().unwrap_or(""),
        )
        .await
    {
        tracing::warn!(tool, error = %e, "could not keep what the owner always allows");
    }
}

/// What the model is told when the owner does not allow `tool`.
fn refusal(review: Review, tool: &str, why: &str) -> ToolResult {
    let lang = crate::lang();
    ToolResult::Error(match review {
        Review::Call => lang.pick(
            format!(
                "{tool} was not run: here it needs your owner's approval, and {why}. Do not call \
                 {tool} again in this reply. This is for this reply only: for a later message \
                 that needs it, call it again, and your owner is asked again"
            ),
            format!(
                "{tool} 没有运行：在这里它需要所有者批准，而{why}。这次回复里不要再调用 {tool}。这只对\
                 这次回复有效：之后的消息如果需要，就再调用，会再问所有者"
            ),
        ),
        Review::Result => lang.pick(
            format!(
                "what {tool} gave needs your owner's approval here, and {why}: answer this \
                 message without it, do not call {tool} again in this reply, and do not say what \
                 it gave. This is for this reply only: for a later message that needs it, call it \
                 again, and your owner is asked again"
            ),
            format!(
                "{tool} 给出的内容在这里需要所有者批准，而{why}。这条消息不用它来回答，这次回复里不要\
                 再调用 {tool}，也不要透露它给出了什么。这只对这次回复有效：之后的消息如果需要，就再\
                 调用，会再问所有者"
            ),
        ),
    })
}

/// `result`, with the owner's rule for what not to say, when there is one.
fn with_rule(result: ToolResult, rule: Option<&str>) -> ToolResult {
    let Some(rule) = rule.map(str::trim).filter(|r| !r.is_empty()) else {
        return result;
    };
    let rule = crate::lang().pick(
        format!("\nYour owner allowed this here, with this rule for what not to say: {rule}\n"),
        format!("\n所有者允许了这一步，规定了不能说什么：{rule}\n"),
    );
    match result {
        ToolResult::Ok(text) => ToolResult::Ok(format!("{text}{rule}")),
        ToolResult::Error(text) => ToolResult::Error(format!("{text}{rule}")),
        ToolResult::Image { text, path } => ToolResult::Image {
            text: format!("{text}{rule}"),
            path,
        },
    }
}

/// The agent's approvals, for a call asked about outside the registry.
static GLOBAL: std::sync::OnceLock<std::sync::Arc<Approvals>> = std::sync::OnceLock::new();

/// Make `approvals` the agent's, for [`global`].
pub fn install(approvals: std::sync::Arc<Approvals>) {
    let _ = GLOBAL.set(approvals);
}

/// The agent's approvals, once [`install`]ed.
pub fn global() -> Option<std::sync::Arc<Approvals>> {
    GLOBAL.get().cloned()
}

#[derive(Debug)]
pub struct Approvals {
    writer_tx: mpsc::Sender<AgentToGateway>,
    /// Requests waiting, oldest first.
    waiting: Mutex<Vec<(String, Waiting)>>,
    /// (chat, tool, input) approved for the rest of a chat, until the agent
    /// restarts.
    remembered: Mutex<HashSet<(String, String, String)>>,
}

impl Approvals {
    #[must_use]
    pub fn new(writer_tx: mpsc::Sender<AgentToGateway>) -> Self {
        Self {
            writer_tx,
            waiting: Mutex::new(Vec::new()),
            remembered: Mutex::new(HashSet::new()),
        }
    }

    /// Wait for the owner to approve calling `tool` with `input` (the whole
    /// input as JSON) in `conversation`, in its chat `chat`. `Err` says why
    /// the call may not run.
    pub async fn ask(
        &self,
        conversation: Option<&str>,
        chat: Option<&str>,
        tool: &str,
        input: &str,
        reason: &str,
    ) -> Result<(), String> {
        let Some(channel_id) = conversation else {
            return Err(
                "it needs approval, and no one can be asked here (a scheduled run, \
                        a heartbeat or another agent's query)"
                    .into(),
            );
        };
        if input.chars().count() > MAX_SHOWN_INPUT {
            return Err(format!(
                "it needs approval, and its input is longer than the {MAX_SHOWN_INPUT} \
                 characters that can be shown for it"
            ));
        }
        let chat = chat.unwrap_or(channel_id);
        let key = (chat.to_string(), tool.to_string(), input.to_string());
        if self.lock_remembered().contains(&key) {
            return Ok(());
        }
        let decision = self.wait(channel_id, chat, tool, input, reason, "").await?;
        if decision.approve {
            Ok(())
        } else {
            Err("it was denied".into())
        }
    }

    /// Wait for the owner to allow `tool` in `conversation`, not theirs,
    /// as `review` says (issue #255); `input` is what they are shown: who
    /// asked, what, the call and, after it, what it gave. `Err` says why
    /// it may not.
    pub async fn ask_for(
        &self,
        conversation: &str,
        chat: Option<&str>,
        review: Review,
        tool: &str,
        input: &str,
    ) -> Result<Answer, String> {
        let lang = crate::lang();
        if input.chars().count() > MAX_SHOWN_INPUT {
            return Err(lang.pick(
                format!(
                    "it could not be put to them: it is longer than the {MAX_SHOWN_INPUT} \
                     characters that can be shown"
                ),
                format!("没法问所有者：内容超过了能显示的 {MAX_SHOWN_INPUT} 个字符"),
            ));
        }
        let chat = chat.unwrap_or(conversation);
        let decision = self
            .wait(
                conversation,
                chat,
                tool,
                input,
                review.kind(),
                review.kind(),
            )
            .await?;
        if !decision.approve {
            return Err(lang
                .pick("your owner did not allow it", "所有者没有允许")
                .into());
        }
        Ok(Answer {
            always: decision.remember,
            note: decision
                .note
                .map(|n| n.trim().chars().take(MAX_NOTE_CHARS).collect::<String>())
                .filter(|n| !n.is_empty()),
        })
    }

    /// Send the request and wait for its decision, for at most
    /// [`APPROVAL_TIMEOUT`].
    async fn wait(
        &self,
        channel_id: &str,
        chat: &str,
        tool: &str,
        input: &str,
        reason: &str,
        kind: &str,
    ) -> Result<ApprovalDecision, String> {
        let request_id = Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        self.lock_waiting().push((
            request_id.clone(),
            Waiting {
                channel_id: channel_id.to_string(),
                chat: chat.to_string(),
                tool: tool.to_string(),
                input: input.to_string(),
                kept: !kind.is_empty(),
                reply: tx,
            },
        ));
        // Removed however the wait ends, also when the turn is stopped.
        let _waiting = Forget {
            approvals: self,
            request_id: &request_id,
        };
        let request = AgentToGateway::ApprovalRequest(ApprovalRequest {
            request_id: request_id.clone(),
            channel_id: channel_id.to_string(),
            tool: tool.to_string(),
            input: input.to_string(),
            reason: reason.to_string(),
            kind: kind.to_string(),
        });
        if self.writer_tx.send(request).await.is_err() {
            return Err("the gateway connection is closed".into());
        }
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_)) => Err("the request was dropped".into()),
            Err(_) => Err(format!(
                "it was not approved within {} minutes",
                APPROVAL_TIMEOUT.as_secs() / 60
            )),
        }
    }

    /// Apply a decision to the request it names. A decision whose
    /// conversation does not match is ignored.
    pub fn decide(&self, decision: &ApprovalDecision) -> bool {
        let found = {
            let mut waiting = self.lock_waiting();
            let at = waiting.iter().position(|(rid, w)| {
                *rid == decision.request_id && w.channel_id == decision.channel_id
            });
            at.map(|i| waiting.remove(i).1)
        };
        let Some(waiting) = found else {
            return false;
        };
        // A call in someone else's conversation keeps its own "always".
        if decision.approve && decision.remember && !waiting.kept {
            self.lock_remembered()
                .insert((waiting.chat, waiting.tool, waiting.input));
        }
        let _ = waiting.reply.send(decision.clone());
        true
    }

    fn lock_waiting(&self) -> std::sync::MutexGuard<'_, Vec<(String, Waiting)>> {
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_remembered(&self) -> std::sync::MutexGuard<'_, HashSet<(String, String, String)>> {
        self.remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct Forget<'a> {
    approvals: &'a Approvals,
    request_id: &'a str,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        let mut waiting = self.approvals.lock_waiting();
        let before = waiting.len();
        waiting.retain(|(id, _)| id != self.request_id);
        // Still waiting, so no decision came: tell the gateway to stop
        // showing it.
        if waiting.len() < before {
            let _ = self
                .approvals
                .writer_tx
                .try_send(AgentToGateway::ApprovalClosed {
                    request_id: self.request_id.to_string(),
                });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::Arc;

    use super::*;

    #[test]
    fn viewing_an_image_in_someone_else_s_chat_is_asked_about_first() {
        assert!(matches!(Review::of("view_image"), Some(Review::Call)));
        assert!(matches!(Review::of("file_read"), Some(Review::Result)));
    }

    fn decision(channel: &str, id: &str, approve: bool, remember: bool) -> ApprovalDecision {
        ApprovalDecision {
            channel_id: channel.into(),
            request_id: id.into(),
            approve,
            remember,
            note: None,
        }
    }

    #[tokio::test]
    async fn what_memory_found_waits_for_the_owner_with_their_note() {
        let (tx, mut rx) = mpsc::channel(8);
        let approvals = Arc::new(Approvals::new(tx));
        let ask = |a: &Arc<Approvals>| {
            let a = Arc::clone(a);
            tokio::spawn(async move {
                a.ask_for(
                    "tg:1",
                    None,
                    Review::Result,
                    "memory_search",
                    r#"{"found":"x"}"#,
                )
                .await
            })
        };
        let call = ask(&approvals);
        let request = next_request(&mut rx).await;
        assert_eq!(request.kind, "result");
        let mut used = decision("tg:1", &request.request_id, true, true);
        used.note = Some("  not my address  ".into());
        assert!(approvals.decide(&used));
        assert_eq!(
            call.await.unwrap(),
            Ok(Answer {
                always: true,
                note: Some("not my address".into())
            })
        );
        // Its "always" is not the tools': the next search asks again here.
        let call = ask(&approvals);
        let request = next_request(&mut rx).await;
        assert!(approvals.decide(&decision("tg:1", &request.request_id, false, false)));
        assert!(call.await.unwrap().is_err());
        assert!(
            approvals
                .ask_for(
                    "tg:1",
                    None,
                    Review::Call,
                    "shell",
                    &"x".repeat(MAX_SHOWN_INPUT + 1)
                )
                .await
                .is_err()
        );
    }

    async fn next_request(rx: &mut mpsc::Receiver<AgentToGateway>) -> ApprovalRequest {
        match rx.recv().await {
            Some(AgentToGateway::ApprovalRequest(r)) => r,
            other => panic!("expected an ApprovalRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_call_waits_for_the_decision_on_its_request() {
        let (tx, mut rx) = mpsc::channel(8);
        let approvals = Arc::new(Approvals::new(tx));
        let call = tokio::spawn({
            let a = Arc::clone(&approvals);
            async move {
                a.ask(Some("c1"), None, "shell", r#"{"command":"deploy"}"#, "rule")
                    .await
            }
        });
        let request = next_request(&mut rx).await;
        assert_eq!(request.channel_id, "c1");
        assert_eq!(
            request.input, r#"{"command":"deploy"}"#,
            "the whole input is shown"
        );
        assert!(
            !approvals.decide(&decision("c2", &request.request_id, true, false)),
            "another conversation"
        );
        assert!(approvals.decide(&decision("c1", &request.request_id, true, true)));
        assert_eq!(call.await.unwrap(), Ok(()));

        // Remembered for this conversation: no second question.
        let again = approvals.ask(Some("c1"), None, "shell", r#"{"command":"deploy"}"#, "rule");
        assert_eq!(again.await, Ok(()));
        assert!(rx.try_recv().is_err());

        // Not in a new chat of the same conversation, though.
        let fresh = tokio::spawn({
            let a = Arc::clone(&approvals);
            async move {
                a.ask(
                    Some("c1"),
                    Some("chat-2"),
                    "shell",
                    r#"{"command":"deploy"}"#,
                    "rule",
                )
                .await
            }
        });
        let request = next_request(&mut rx).await;
        assert!(approvals.decide(&decision("c1", &request.request_id, true, false)));
        assert_eq!(fresh.await.unwrap(), Ok(()));

        let denied = tokio::spawn({
            let a = Arc::clone(&approvals);
            async move {
                a.ask(Some("c1"), None, "shell", r#"{"command":"rm"}"#, "rule")
                    .await
            }
        });
        let request = next_request(&mut rx).await;
        assert!(approvals.decide(&decision("c1", &request.request_id, false, false)));
        assert!(denied.await.unwrap().unwrap_err().contains("denied"));
        assert!(approvals.lock_waiting().is_empty());
        assert!(
            rx.try_recv().is_err(),
            "a decided request is not closed again"
        );
    }

    #[tokio::test]
    async fn without_a_conversation_a_short_input_or_an_answer_calls_are_refused() {
        let (tx, mut rx) = mpsc::channel(8);
        let approvals = Approvals::new(tx);
        let none = approvals
            .ask(None, None, "shell", "x", "rule")
            .await
            .unwrap_err();
        assert!(none.contains("scheduled run"), "{none}");
        let long = "x".repeat(MAX_SHOWN_INPUT + 1);
        let too_long = approvals
            .ask(Some("c1"), None, "shell", &long, "rule")
            .await
            .unwrap_err();
        assert!(too_long.contains("longer"), "{too_long}");

        tokio::time::pause();
        let wait = approvals.ask(Some("c1"), None, "shell", "x", "rule");
        tokio::pin!(wait);
        tokio::select! {
            _ = &mut wait => panic!("answered without a decision"),
            r = rx.recv() => assert!(r.is_some()),
        }
        tokio::time::advance(APPROVAL_TIMEOUT + Duration::from_secs(1)).await;
        assert!(wait.await.unwrap_err().contains("not approved"));
        assert!(approvals.lock_waiting().is_empty());
        assert!(
            matches!(rx.try_recv(), Ok(AgentToGateway::ApprovalClosed { .. })),
            "the gateway is told the request ended"
        );
    }

    /// `tool` called with `input` in conversation `tg:1`, not the owner's,
    /// in a turn of `refused`; giving `output` when it runs, counted in
    /// `ran`.
    #[allow(clippy::too_many_arguments)]
    async fn in_turn(
        approvals: Arc<Approvals>,
        db: sqlx::SqlitePool,
        refused: Arc<Refused>,
        review: Review,
        tool: &'static str,
        input: serde_json::Value,
        output: String,
        ran: Arc<std::sync::atomic::AtomicUsize>,
    ) -> ToolResult {
        crate::tools::gateway::CONVERSATION
            .scope(
                "tg:1".into(),
                crate::tools::gateway::REFUSED.scope(
                    refused,
                    guarded(
                        Some(&approvals),
                        &db,
                        review,
                        tool,
                        &input,
                        false,
                        async move {
                            ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            ToolResult::Ok(output)
                        },
                    ),
                ),
            )
            .await
    }

    #[tokio::test]
    async fn someone_else_s_call_waits_for_the_owner_before_it_runs() {
        let (tx, mut rx) = mpsc::channel(8);
        let approvals = Arc::new(Approvals::new(tx));
        let db = havn_db::agent::connect_in_memory().await.expect("db");
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = || ran.load(std::sync::atomic::Ordering::SeqCst);
        let shell = |refused: &Arc<Refused>, command: &str| {
            tokio::spawn(in_turn(
                Arc::clone(&approvals),
                db.clone(),
                Arc::clone(refused),
                Review::Call,
                "shell",
                serde_json::json!({ "command": command }),
                "done".into(),
                Arc::clone(&ran),
            ))
        };
        let turn = Arc::new(Refused::default());
        let call = shell(&turn, "ls");
        let request = next_request(&mut rx).await;
        assert_eq!(request.kind, "call");
        assert_eq!(request.tool, "shell");
        let shown: serde_json::Value = serde_json::from_str(&request.input).expect("json");
        assert_eq!(shown["input"]["command"], "ls");
        assert_eq!(count(), 0, "nothing runs before the owner answers");
        assert!(approvals.decide(&decision("tg:1", &request.request_id, true, true)));
        assert_eq!(call.await.expect("join").text(), "done");
        assert_eq!(count(), 1);

        // Always allowed: that exact call, and no other.
        let again = shell(&turn, "ls").await.expect("join");
        assert_eq!(again.text(), "done");
        assert_eq!(count(), 2);
        assert!(rx.try_recv().is_err(), "the owner is not asked again");
        let other = shell(&turn, "rm -rf notes");
        let request = next_request(&mut rx).await;
        assert!(approvals.decide(&decision("tg:1", &request.request_id, false, false)));
        assert!(other.await.expect("join").is_error());
        assert_eq!(count(), 2);

        // Refused once, the tool is refused for the rest of the turn.
        let next = shell(&turn, "pwd").await.expect("join");
        assert!(next.is_error());
        assert!(rx.try_recv().is_err(), "without asking");
        assert_eq!(count(), 2);

        // With no one to ask, nothing runs.
        let alone = guarded(
            Some(&approvals),
            &db,
            Review::Call,
            "shell",
            &serde_json::json!({ "command": "ls" }),
            false,
            async { ToolResult::Ok("ran".into()) },
        )
        .await;
        assert!(alone.is_error());
    }

    #[tokio::test]
    async fn what_a_reading_tool_gave_someone_else_waits_for_the_owner() {
        let (tx, mut rx) = mpsc::channel(8);
        let approvals = Arc::new(Approvals::new(tx));
        let db = havn_db::agent::connect_in_memory().await.expect("db");
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let search = |output: String| {
            tokio::spawn(in_turn(
                Arc::clone(&approvals),
                db.clone(),
                Arc::new(Refused::default()),
                Review::Result,
                "memory_search",
                serde_json::json!({ "query": "address" }),
                output,
                Arc::clone(&ran),
            ))
        };
        let call = search("Mira, 12 Rose Street".into());
        let request = next_request(&mut rx).await;
        assert_eq!(request.kind, "result");
        let shown: serde_json::Value = serde_json::from_str(&request.input).expect("json");
        assert_eq!(shown["found"], "Mira, 12 Rose Street");
        assert_eq!(shown["input"]["query"], "address");
        assert!(approvals.decide(&ApprovalDecision {
            note: Some("not the street".into()),
            ..decision("tg:1", &request.request_id, true, false)
        }));
        let given = call.await.expect("join");
        assert!(given.text().starts_with("Mira, 12 Rose Street"));
        assert!(
            given.text().contains("not the street"),
            "with the owner's note"
        );

        // Too long to show: the owner and the model get the same part.
        let call = search("x".repeat(10_000));
        let request = next_request(&mut rx).await;
        assert!(request.input.chars().count() <= MAX_SHOWN_INPUT);
        let shown: serde_json::Value = serde_json::from_str(&request.input).expect("json");
        let found = shown["found"].as_str().expect("found").to_string();
        assert!(found.ends_with("(The rest was left out: too long to show.)"));
        assert!(approvals.decide(&decision("tg:1", &request.request_id, true, true)));
        assert_eq!(call.await.expect("join").text(), found);

        // Always allowed there: given without asking.
        let later = search("Mira".into()).await.expect("join");
        assert_eq!(later.text(), "Mira");
        assert!(rx.try_recv().is_err());
    }
}
