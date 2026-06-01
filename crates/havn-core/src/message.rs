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
    /// Original platform payload, preserved verbatim for adapter-specific features.
    #[serde(default)]
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel_id: String,
    pub content: MessageContent,
    /// Optional message ID to thread / reply to.
    pub reply_to: Option<String>,
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
