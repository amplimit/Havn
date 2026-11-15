//! Tool calls that wait for approval (issue #127). A hook's `ask` rule
//! that matches pauses the call: the runtime sends an [`ApprovalRequest`]
//! with the call's whole input, the gateway asks the agent's owner, and
//! their [`ApprovalDecision`] lets the call run or refuses it. With no one
//! to ask (a scheduled run, a heartbeat, another agent's query), an input
//! too long to show, or no answer in time, the call is refused.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use havn_proto::{AgentToGateway, ApprovalDecision, ApprovalRequest};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// How long a call waits for approval before it is refused.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Longest input that can be approved; the owner sees all of it.
const MAX_SHOWN_INPUT: usize = 4000;

#[derive(Debug)]
struct Waiting {
    channel_id: String,
    /// The chat a remembered approval holds for.
    chat: String,
    tool: String,
    input: String,
    reply: oneshot::Sender<bool>,
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
        let request_id = Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        self.lock_waiting().push((
            request_id.clone(),
            Waiting {
                channel_id: channel_id.to_string(),
                chat: chat.to_string(),
                tool: tool.to_string(),
                input: input.to_string(),
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
        });
        if self.writer_tx.send(request).await.is_err() {
            return Err("the gateway connection is closed".into());
        }
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err("it was denied".into()),
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
        if decision.approve && decision.remember {
            self.lock_remembered()
                .insert((waiting.chat, waiting.tool, waiting.input));
        }
        let _ = waiting.reply.send(decision.approve);
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

    fn decision(channel: &str, id: &str, approve: bool, remember: bool) -> ApprovalDecision {
        ApprovalDecision {
            channel_id: channel.into(),
            request_id: id.into(),
            approve,
            remember,
        }
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
}
