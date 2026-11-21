//! Wire types for the channel-adapter HTTP+WS API (spec §3.1).
//!
//! Distinct from the agent-socket protocol in [`crate::AgentToGateway`] /
//! [`crate::GatewayToAgent`] — that's between gateway and the per-agent
//! runtime over Unix domain sockets. This module's types travel between
//! the gateway and **external adapter daemons** over a TCP WebSocket, in
//! a different trust zone (spec §3 intro: adapters never see agent
//! namespaces, the credential store, or policy state).
//!
//! A single WebSocket per (adapter daemon, channel account) carries
//! both directions multiplexed via a typed `kind` discriminator. The
//! gateway side is implemented in `havn-gateway::api::channel`; the
//! reference adapter implementation lives at `/home/havn-channel-telegram`
//! (sibling repo).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One bidirectional frame over the channel WebSocket.
///
/// Adapter and gateway both send / receive — direction is implied by
/// frame variant (e.g. `Inbound` only flows adapter → gateway, `Outbound`
/// only gateway → adapter). The router enforces directionality and
/// ignores frames flowing the wrong way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChannelFrame {
    /// Adapter → gateway. A user-typed message from the chat platform.
    Inbound(ChannelInbound),
    /// Gateway → adapter. An agent reply to deliver to the chat platform.
    Outbound(ChannelOutbound),
    /// Adapter → gateway. Sent on reconnect; gateway replays anything
    /// from `since_seq + 1` onward that's still in the per-account
    /// outbound buffer (spec §3.2; 5-minute window by default).
    Resume(ChannelResume),
    /// Either direction. Liveness check; receiver responds with `Pong`.
    Ping,
    /// Either direction. Reply to `Ping`.
    Pong,
    /// Gateway -> adapter, once the connection is open: the chat commands
    /// the gateway answers, for a platform's command menu (Telegram's
    /// `setMyCommands`). An adapter that predates it skips it.
    Commands(ChannelCommands),
}

/// Gateway -> adapter: see [`ChannelFrame::Commands`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelCommands {
    pub account_id: String,
    /// In the order a menu shows them.
    pub commands: Vec<ChannelCommand>,
    /// The gateway takes group messages (issue #263): an adapter passes
    /// them on only to one that says so, as an older gateway would take
    /// them for private ones.
    #[serde(default)]
    pub groups: bool,
}

/// One entry of [`ChannelCommands`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelCommand {
    /// Without the slash: lowercase letters, digits and `_`.
    pub name: String,
    pub description: String,
}

/// Adapter → gateway: one inbound user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInbound {
    /// Adapter-side monotonic counter. Audit-only; the gateway logs
    /// duplicates (same `(account_id, seq)`) but doesn't reject them —
    /// a malicious adapter replaying inbound is a same-blast-radius
    /// problem as any other adapter compromise (spec §3.2).
    pub seq: u64,
    /// Operator-declared account this frame belongs to. The WS upgrade
    /// already authenticated the connection against one account; the
    /// gateway rejects frames whose `account_id` doesn't match.
    pub account_id: String,
    /// Platform-specific opaque ID for this exact message. Used by
    /// outbound `in_reply_to` to thread replies back to the right
    /// chat thread (telegram message id, slack thread_ts, …).
    pub channel_message_id: String,
    /// Platform-prefixed sender ID. Convention: `<channel>:<id>`,
    /// e.g. `tg:123456789`, `slack:U07ABCD`. The prefix matches the
    /// channel; the suffix is the platform-native user id.
    pub sender_id: String,
    pub content: ChannelMessageContent,
    pub timestamp: DateTime<Utc>,
    /// Verbatim platform payload, preserved for audit / future
    /// platform-specific features. Optional — adapters can omit if the
    /// payload is large or sensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
    /// Files the message carries (issue #106): each one first sent to
    /// `POST /api/v1/channel/files` by the adapter, for this sender, and
    /// named here as that answered. A photo or document goes as text (its
    /// caption, or empty) with its file here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<havn_core::Attachment>,
    /// The group the message was sent in (issue #263); none in a private
    /// chat. Replies go to the group, which is one conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<ChannelGroup>,
}

/// The group a message was sent in (issue #263).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelGroup {
    /// Platform-prefixed group id, as a sender's (`tg:-1001234`): what
    /// replies are sent to.
    pub id: String,
    /// The group's name, as the platform shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The sender's name, as the platform shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    /// Whether the message is to the bot: it names the bot, replies to
    /// one of its messages, or is a command. Others are kept for context
    /// and not answered.
    #[serde(default)]
    pub addressed: bool,
}

/// Gateway → adapter: one assistant-turn reply to deliver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelOutbound {
    /// Gateway-side monotonic counter, per account. Adapters track the
    /// last seq they acked and use it in `Resume` after reconnect.
    pub seq: u64,
    pub account_id: String,
    /// Platform-prefixed recipient (mirrors `sender_id` shape from the
    /// inbound that triggered this).
    pub to: String,
    /// Optional platform message ID this is replying to. Some platforms
    /// use it to thread (Slack `thread_ts`, Telegram `reply_to_message_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    pub content: ChannelMessageContent,
    /// Rows of buttons under the message (issues #203, #204), for an
    /// adapter that shows them; one that does not shows the text alone,
    /// which says how to answer by typing. A press comes back as a
    /// `ChannelInbound` with the button's `data` as its text, from the
    /// sender who pressed it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<Vec<havn_core::Button>>,
    /// A question answered by choosing several options (issue #204), for
    /// an adapter that shows polls in place of the text; the answer comes
    /// back as the poll's `answer` and the chosen numbers, as a press does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<havn_core::Poll>,
    /// A part of a reply with more to come (issue #212): the agent is
    /// still working, so an adapter keeps showing that it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

/// Adapter → gateway: replay request after reconnect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelResume {
    pub account_id: String,
    /// Last seq the adapter successfully delivered. Gateway replays
    /// `since_seq + 1` through the current head from its in-memory
    /// buffer. Buffer overflow (adapter offline > 5 min) drops with a
    /// warn log — agents do not block on adapter availability.
    pub since_seq: u64,
}

/// Content type carried by `Inbound` and `Outbound` frames.
///
/// Adapters map their platform-specific message types into one of these
/// variants. Unknown platform features (Telegram polls, Slack blocks)
/// either flatten to text or get carried via `Inbound::raw` for the
/// agent's runtime to look at if it cares.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChannelMessageContent {
    Text {
        text: String,
    },
    Image {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    /// Outbound, a file an agent sent (issue #106): `url` is always
    /// `/api/v1/channel/files/<agent>/<file>` on the gateway. The adapter
    /// fetches it from the gateway it is connected to, adding
    /// `?channel=<channel>&account=<account>` and its `X-Adapter-Token`,
    /// and must not send its token anywhere else.
    File {
        url: String,
        filename: String,
        /// A line to go with it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    /// Voice / audio message, transcribed already. An adapter now sends a
    /// voice note as an audio file (`attachments`), which the gateway
    /// transcribes with `[transcription]` (issue #164); this stays for
    /// adapters that transcribe on their own.
    /// `url` is preserved so a future multimodal agent could fetch the
    /// audio directly; today nothing in havn reads it.
    Audio {
        /// Adapter-supplied transcript. The agent's LLM context
        /// renders this with a `[voice, Ns]:` marker so the model
        /// knows the original was speech (per spec §3, content-type
        /// universality criterion).
        transcript: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_seconds: Option<u32>,
        /// Platform-hosted audio URL (often time-limited — Telegram's
        /// is 24h). Optional; adapters that can't surface a URL leave
        /// it `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Geographic point. Adapter normalises the platform's "share
    /// location" payload (Telegram `Location`, WhatsApp location,
    /// Line location, …) into WGS84 lat/lng degrees.
    Location {
        latitude: f64,
        longitude: f64,
        /// Horizontal accuracy in meters, when the platform provides
        /// it (Telegram does; WhatsApp doesn't expose).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accuracy_meters: Option<f32>,
        /// Human-readable name if the platform attaches one
        /// ("Stanford University", "Apple Park"). Telegram "venue"
        /// vs plain "location" both flatten to this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
    /// Shared contact card (phone book entry). vCard 3.0 / 4.0 is
    /// the canonical format; adapters that produce structured
    /// objects (Telegram, WhatsApp) flatten to vCard for the wire so
    /// the agent doesn't see platform-specific layouts.
    Contact {
        display_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phone_number: Option<String>,
        /// Full vCard payload when the adapter has one; the
        /// individual fields above are convenience views over the
        /// most common bits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vcard: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn inbound_round_trips_with_optional_raw_omitted() {
        let frame = ChannelFrame::Inbound(ChannelInbound {
            seq: 42,
            account_id: "alice-tg-bot".into(),
            channel_message_id: "tg:chat:123:msg:7".into(),
            sender_id: "tg:123456789".into(),
            content: ChannelMessageContent::Text {
                text: "hello".into(),
            },
            timestamp: Utc::now(),
            raw: None,
            attachments: Vec::new(),
            group: None,
        });
        let s = serde_json::to_string(&frame).expect("serialize");
        // Sanity-check the discriminator and absence of raw.
        assert!(s.contains("\"kind\":\"inbound\""));
        assert!(s.contains("\"type\":\"text\""));
        assert!(!s.contains("\"raw\""), "raw=None should be skipped: {s}");
        let parsed: ChannelFrame = serde_json::from_str(&s).expect("parse");
        match parsed {
            ChannelFrame::Inbound(i) => {
                assert_eq!(i.seq, 42);
                assert_eq!(i.account_id, "alice-tg-bot");
            }
            other => panic!("expected Inbound, got {other:?}"),
        }
    }

    #[test]
    fn outbound_round_trips_with_in_reply_to() {
        let frame = ChannelFrame::Outbound(ChannelOutbound {
            seq: 100,
            account_id: "alice-tg-bot".into(),
            to: "tg:123456789".into(),
            in_reply_to: Some("tg:chat:123:msg:7".into()),
            content: ChannelMessageContent::Text { text: "hi".into() },
            buttons: Vec::new(),
            poll: None,
            partial: false,
        });
        let s = serde_json::to_string(&frame).expect("serialize");
        let parsed: ChannelFrame = serde_json::from_str(&s).expect("parse");
        match parsed {
            ChannelFrame::Outbound(o) => {
                assert_eq!(o.in_reply_to.as_deref(), Some("tg:chat:123:msg:7"));
            }
            other => panic!("expected Outbound, got {other:?}"),
        }
    }

    #[test]
    fn buttons_are_sent_only_when_there_are_some() {
        let outbound = |buttons: Vec<Vec<havn_core::Button>>| ChannelOutbound {
            seq: 1,
            account_id: "a".into(),
            to: "tg:1".into(),
            in_reply_to: None,
            content: ChannelMessageContent::Text { text: "t".into() },
            buttons,
            poll: None,
            partial: false,
        };
        let plain = serde_json::to_string(&outbound(Vec::new())).expect("json");
        assert!(!plain.contains("buttons"), "{plain}");
        let pressed = serde_json::to_string(&outbound(vec![vec![havn_core::Button {
            label: "Approve".into(),
            data: "/approve r1".into(),
        }]]))
        .expect("json");
        assert!(
            pressed.contains(r#""buttons":[[{"label":"Approve","data":"/approve r1"}]]"#),
            "{pressed}"
        );
        // A frame from before buttons still reads.
        let old: ChannelOutbound = serde_json::from_str(
            r#"{"seq":1,"account_id":"a","to":"tg:1","content":{"type":"text","text":"t"}}"#,
        )
        .expect("old frame");
        assert!(old.buttons.is_empty());
    }

    #[test]
    fn resume_round_trips() {
        let frame = ChannelFrame::Resume(ChannelResume {
            account_id: "alice-tg-bot".into(),
            since_seq: 99,
        });
        let s = serde_json::to_string(&frame).expect("serialize");
        assert!(s.contains("\"kind\":\"resume\""));
        let parsed: ChannelFrame = serde_json::from_str(&s).expect("parse");
        match parsed {
            ChannelFrame::Resume(r) => assert_eq!(r.since_seq, 99),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn ping_pong_serialize_compactly() {
        let s = serde_json::to_string(&ChannelFrame::Ping).expect("ping");
        assert_eq!(s, r#"{"kind":"ping"}"#);
        let s = serde_json::to_string(&ChannelFrame::Pong).expect("pong");
        assert_eq!(s, r#"{"kind":"pong"}"#);
    }

    #[test]
    fn audio_content_round_trip() {
        let with_url = ChannelMessageContent::Audio {
            transcript: "hello there".into(),
            duration_seconds: Some(12),
            url: Some("https://api.telegram.org/file/...".into()),
        };
        let s = serde_json::to_string(&with_url).expect("ok");
        assert!(s.contains("\"type\":\"audio\""));
        assert!(s.contains("\"transcript\":\"hello there\""));
        assert!(s.contains("\"duration_seconds\":12"));
        let parsed: ChannelMessageContent = serde_json::from_str(&s).expect("ok");
        assert_eq!(parsed, with_url);

        // No URL, no duration — minimum viable Audio payload.
        let minimal = ChannelMessageContent::Audio {
            transcript: "x".into(),
            duration_seconds: None,
            url: None,
        };
        let s = serde_json::to_string(&minimal).expect("ok");
        assert!(!s.contains("duration_seconds"));
        assert!(!s.contains("\"url\""));
    }

    #[test]
    fn location_content_round_trip() {
        let venue = ChannelMessageContent::Location {
            latitude: 37.427_5,
            longitude: -122.169_7,
            accuracy_meters: Some(20.0),
            name: Some("Stanford University".into()),
            address: Some("450 Jane Stanford Way".into()),
        };
        let s = serde_json::to_string(&venue).expect("ok");
        assert!(s.contains("\"type\":\"location\""));
        let parsed: ChannelMessageContent = serde_json::from_str(&s).expect("ok");
        if let ChannelMessageContent::Location { latitude, .. } = parsed {
            assert!((latitude - 37.427_5).abs() < 1e-6);
        } else {
            panic!("not location");
        }
    }

    #[test]
    fn contact_content_round_trip() {
        let c = ChannelMessageContent::Contact {
            display_name: "Alice Example".into(),
            phone_number: Some("+15551234567".into()),
            vcard: None,
        };
        let s = serde_json::to_string(&c).expect("ok");
        assert!(s.contains("\"type\":\"contact\""));
        assert!(!s.contains("vcard")); // None skipped
        let parsed: ChannelMessageContent = serde_json::from_str(&s).expect("ok");
        assert_eq!(parsed, c);
    }

    #[test]
    fn image_content_with_optional_caption() {
        let with = ChannelMessageContent::Image {
            url: "https://x/y.png".into(),
            caption: Some("hi".into()),
        };
        let s = serde_json::to_string(&with).expect("ok");
        assert!(s.contains("\"caption\":\"hi\""));
        let without = ChannelMessageContent::Image {
            url: "https://x/y.png".into(),
            caption: None,
        };
        let s = serde_json::to_string(&without).expect("ok");
        assert!(!s.contains("caption"), "None caption should skip: {s}");
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let s = r#"{"kind":"zzz","blah":1}"#;
        let r: Result<ChannelFrame, _> = serde_json::from_str(s);
        assert!(r.is_err(), "should reject unknown kind, got {r:?}");
    }
}
