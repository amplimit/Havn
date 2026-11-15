//! Tool calls that wait for the user (issue #127). A hook's `ask` rule
//! that matches pauses the call: the runtime sends an
//! [`ApprovalRequest`] for the conversation, the gateway asks the user, and
//! their [`ApprovalDecision`] lets the call run or refuses it. With no one
//! to ask (a scheduled run) or no answer in time, the call is refused.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use havn_proto::{AgentToGateway, ApprovalDecision, ApprovalRequest};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// How long a call waits for the user before it is refused.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Longest input shown to the user.
const MAX_SHOWN_INPUT: usize = 500;

#[derive(Debug)]
struct Waiting {
    channel_id: String,
    tool: String,
    input: String,
    reply: oneshot::Sender<bool>,
}

#[derive(Debug)]
pub struct Approvals {
    writer_tx: mpsc::Sender<AgentToGateway>,
    /// Requests waiting, oldest first.
    waiting: Mutex<Vec<(String, Waiting)>>,
    /// (conversation, tool, input) approved for the rest of a conversation.
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

    /// Wait for the user of `conversation` to approve calling `tool` with
    /// `input`. `Err` says why the call may not run.
    pub async fn ask(
        &self,
        conversation: Option<&str>,
        tool: &str,
        input: &str,
        reason: &str,
    ) -> Result<(), String> {
        let Some(channel_id) = conversation else {
            return Err(
                "it needs the user's approval, and no one can be asked in a \
                        scheduled run"
                    .into(),
            );
        };
        let key = (channel_id.to_string(), tool.to_string(), input.to_string());
        if self.lock_remembered().contains(&key) {
            return Ok(());
        }
        let request_id = Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        self.lock_waiting().push((
            request_id.clone(),
            Waiting {
                channel_id: channel_id.to_string(),
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
        let shown: String = input.chars().take(MAX_SHOWN_INPUT).collect();
        let request = AgentToGateway::ApprovalRequest(ApprovalRequest {
            request_id: request_id.clone(),
            channel_id: channel_id.to_string(),
            tool: tool.to_string(),
            input: shown,
            reason: reason.to_string(),
        });
        if self.writer_tx.send(request).await.is_err() {
            return Err("the gateway connection is closed".into());
        }
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err("the user denied it".into()),
            Ok(Err(_)) => Err("the request was dropped".into()),
            Err(_) => Err(format!(
                "the user did not approve it within {} minutes",
                APPROVAL_TIMEOUT.as_secs() / 60
            )),
        }
    }

    /// Apply the user's decision: to the named request, or to the latest
    /// one waiting in the conversation. A decision for another
    /// conversation's request is ignored.
    pub fn decide(&self, decision: &ApprovalDecision) -> bool {
        let found = {
            let mut waiting = self.lock_waiting();
            let at = match &decision.request_id {
                Some(id) => waiting
                    .iter()
                    .position(|(rid, w)| rid == id && w.channel_id == decision.channel_id),
                None => waiting
                    .iter()
                    .rposition(|(_, w)| w.channel_id == decision.channel_id),
            };
            at.map(|i| waiting.remove(i).1)
        };
        let Some(waiting) = found else {
            return false;
        };
        if decision.approve && decision.remember {
            self.lock_remembered()
                .insert((waiting.channel_id, waiting.tool, waiting.input));
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
        self.approvals
            .lock_waiting()
            .retain(|(id, _)| id != self.request_id);
    }
}

/// Requests waiting, by conversation, for tests.
#[cfg(test)]
fn waiting_by_conversation(approvals: &Approvals) -> std::collections::HashMap<String, usize> {
    let mut out = std::collections::HashMap::new();
    for (_, w) in approvals.lock_waiting().iter() {
        *out.entry(w.channel_id.clone()).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::Arc;

    use super::*;

    fn decision(
        channel: &str,
        id: Option<&str>,
        approve: bool,
        remember: bool,
    ) -> ApprovalDecision {
        ApprovalDecision {
            channel_id: channel.into(),
            request_id: id.map(str::to_string),
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
    async fn a_call_waits_for_the_user_of_its_conversation() {
        let (tx, mut rx) = mpsc::channel(8);
        let approvals = Arc::new(Approvals::new(tx));
        let call = tokio::spawn({
            let a = Arc::clone(&approvals);
            async move { a.ask(Some("c1"), "shell", "deploy prod", "rule").await }
        });
        let request = next_request(&mut rx).await;
        assert_eq!(
            (request.channel_id.as_str(), request.tool.as_str()),
            ("c1", "shell")
        );
        assert!(
            !approvals.decide(&decision("c2", Some(&request.request_id), true, false)),
            "other conversation"
        );
        assert!(approvals.decide(&decision("c1", Some(&request.request_id), true, true)));
        assert_eq!(call.await.unwrap(), Ok(()));

        // Remembered for this conversation: no second question.
        assert_eq!(
            approvals
                .ask(Some("c1"), "shell", "deploy prod", "rule")
                .await,
            Ok(())
        );
        assert!(rx.try_recv().is_err());

        let denied = tokio::spawn({
            let a = Arc::clone(&approvals);
            async move { a.ask(Some("c1"), "shell", "rm -rf build", "rule").await }
        });
        let _ = next_request(&mut rx).await;
        assert!(
            approvals.decide(&decision("c1", None, false, false)),
            "the latest waiting"
        );
        assert!(denied.await.unwrap().unwrap_err().contains("denied"));
        assert!(waiting_by_conversation(&approvals).is_empty());
    }

    #[tokio::test]
    async fn without_a_conversation_or_an_answer_calls_are_refused() {
        let (tx, mut rx) = mpsc::channel(8);
        let approvals = Approvals::new(tx);
        assert!(
            approvals
                .ask(None, "shell", "x", "rule")
                .await
                .unwrap_err()
                .contains("scheduled run")
        );
        tokio::time::pause();
        let wait = approvals.ask(Some("c1"), "shell", "x", "rule");
        tokio::pin!(wait);
        tokio::select! {
            _ = &mut wait => panic!("answered without a decision"),
            r = rx.recv() => assert!(r.is_some()),
        }
        tokio::time::advance(APPROVAL_TIMEOUT + Duration::from_secs(1)).await;
        assert!(wait.await.unwrap_err().contains("did not approve"));
        assert!(
            waiting_by_conversation(&approvals).is_empty(),
            "the timed-out request is gone"
        );
    }
}
