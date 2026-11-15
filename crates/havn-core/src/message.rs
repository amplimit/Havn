//! Unified message types used across all channel adapters.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Unique ID for this chat / conversation within the channel.
    pub channel_id: String,
    /// Channel-specific user identifier of the sender.
    pub sender_id: String,
    /// Opaque channel name as configured by the operator
    /// (`[[channels.<channel>.accounts]]` in TOML). `"webchat"` for
    /// havn's built-in console; whatever the operator chose otherwise.
    /// Core deliberately does not enumerate channels — that would
    /// re-couple havn to specific platforms (§1.6 / Phase 5).
    pub channel: String,
    pub content: MessageContent,
    pub timestamp: DateTime<Utc>,
    /// The chat of `channel_id` it continues (issue #126): one the
    /// dashboard chose, or, when absent, the active one. The runtime sets
    /// it to the chat it resolved before running the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// Where it branches off its chat instead of continuing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<Branch>,
    /// Files sent with it, saved in the agent's workspace (issue #106).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Original platform payload, preserved verbatim for adapter-specific features.
    #[serde(default)]
    pub raw: serde_json::Value,
}

/// A file sent with a message (issue #106), saved in the agent's
/// workspace before the message reaches the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Where it is, relative to the workspace: `inbox/<name>`.
    pub path: String,
    /// The name it was sent with.
    pub name: String,
    pub media_type: String,
    pub size: u64,
    /// Where its text is, when it was extracted (a PDF, say).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_path: Option<String>,
}

/// Where the files people send are saved, in an agent's workspace.
pub const INBOX_DIR: &str = "inbox";

impl Attachment {
    /// Whether `path` names a file directly in the inbox: `inbox/<name>`,
    /// the name without separators, dot segments or control characters.
    #[must_use]
    pub fn in_inbox(path: &str) -> bool {
        let Some(name) = path
            .strip_prefix(INBOX_DIR)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return false;
        };
        !name.is_empty()
            && name.len() <= 255
            && !name.starts_with('.')
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control)
    }
}

/// Where a message branches off its chat (issue #129).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Branch {
    /// A new version of user turn `turn`, with the message's content.
    Edit { turn: String },
    /// Another reply in place of assistant turn `turn`, or of the last
    /// reply when absent; the message's content is not a turn.
    Regenerate {
        #[serde(default)]
        turn: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel_id: String,
    pub content: MessageContent,
    /// Optional message ID to thread / reply to.
    pub reply_to: Option<String>,
    /// The chat a reply belongs to, when it answers one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageContent {
    Text {
        text: String,
    },
    Image {
        url: String,
        caption: Option<String>,
    },
    File {
        url: String,
        filename: String,
    },
    /// Voice / audio. The transcript is what the agent's LLM reads;
    /// runtime renders it with a `[voice, Ns]:` marker so the model
    /// knows the original was speech (spec §3 content-type universality
    /// criterion — 4+ platforms support voice with a common shape). The
    /// adapter does STT before the message ever reaches havn; raw audio
    /// bytes never touch the gateway or agent process.
    Audio {
        transcript: String,
        duration_seconds: Option<u32>,
        /// Platform-hosted URL (Telegram's is 24h-valid). Optional;
        /// today nothing in havn fetches it. Multimodal agents are v0.3+.
        url: Option<String>,
    },
    /// Geographic point in WGS84. Universal across telegram / whatsapp /
    /// line / wechat / signal.
    Location {
        latitude: f64,
        longitude: f64,
        accuracy_meters: Option<f32>,
        name: Option<String>,
        address: Option<String>,
    },
    /// Shared contact card. vCard 3.0/4.0 is canonical when present;
    /// `display_name` + `phone_number` are convenience views.
    Contact {
        display_name: String,
        phone_number: Option<String>,
        vcard: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_files_directly_in_the_inbox_are_attachments() {
        assert!(Attachment::in_inbox("inbox/20261115-report.pdf"));
        assert!(Attachment::in_inbox("inbox/报告 2026.pdf"));
        for bad in [
            "inbox/",
            "inbox",
            "inbox/../agent.db",
            "inbox/a/b.txt",
            "inbox/.hidden",
            "inbox/a\\b",
            "other/x.pdf",
            "/inbox/x.pdf",
            "inbox/a\nb",
        ] {
            assert!(!Attachment::in_inbox(bad), "{bad}");
        }
    }
}
