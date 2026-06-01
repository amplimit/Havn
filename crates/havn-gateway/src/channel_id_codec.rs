//! Round-trip encoding for `InboundMessage.channel_id` over external
//! channel adapters (spec §3, decision A from v0.2 design discussion).
//!
//! When a channel-adapter inbound arrives, the gateway must remember
//! enough to route the agent's eventual reply back through the right
//! WebSocket to the right user. v0.6 webchat solved this with a UUID
//! `channel_id` and an in-memory router map; that's fine for loopback
//! (browser auto-reconnects) but loses every in-flight conversation
//! on a gateway restart, which is unacceptable for external adapters.
//!
//! This module's encoding stuffs the routing tuple `(channel,
//! account_id, sender_id)` directly into the `channel_id` string the
//! agent round-trips. `|` separates the three fields. Why `|`:
//! telegram (`tg:<numeric>`), slack (`slack:<U…>`), whatsapp
//! (`wa:<phone>`), discord (`discord:<snowflake>`) sender IDs use `:`
//! as their internal separator; `|` doesn't appear in any platform's
//! native id format. account_id is operator-supplied and config
//! validation rejects pipes (see [`validate_channel_id_part`]).
//!
//! Encoding format: `<channel>|<account_id>|<sender_id>`.
//!
//! Why this beats the alternatives:
//! - **In-memory map** (`UUID -> RoutingContext`): gateway restart =
//!   silent black hole for in-flight conversations. Adapter sends
//!   reply, gateway has no idea where it should go.
//! - **DB side table** (`channel_routings(channel_id, channel,
//!   account, sender, ts)`): violates "bindings are config-only — no
//!   DB table" lineage from spec §3.5. Adds a write path on every
//!   inbound + cleanup logic for old rows.
//!
//! In-band encoding fits because:
//! - The channel_id already persists in `agent.db.conversations` —
//!   gateway restart preserves routing for free.
//! - It's pure data; no state to keep coherent.
//! - Routing parse is 3 string splits, O(1).

/// Marker prefix that distinguishes external-channel `channel_id`s
/// from webchat session UUIDs in agent.db conversations history. Not
/// strictly required for parsing (UUIDs don't contain `|` either),
/// but useful for human reading + log filtering.
pub const CHANNEL_ID_PREFIX: &str = "ch|";

/// Build the round-trip channel_id for an external-channel inbound.
/// Caller must have validated `channel`, `account_id`, and `sender_id`
/// don't contain the `|` separator (call [`validate_channel_id_part`]
/// at config-load / inbound-receive time; this function does NOT
/// re-validate to keep the hot path branchless).
#[must_use]
pub fn encode(channel: &str, account_id: &str, sender_id: &str) -> String {
    format!("{CHANNEL_ID_PREFIX}{channel}|{account_id}|{sender_id}")
}

/// Decode a channel_id produced by [`encode`]. Returns `None` for
/// channel_ids that don't carry the marker prefix (webchat UUIDs,
/// bare strings, malformed inputs). The caller distinguishes "this is
/// a channel-adapter id" from "this is something else" by which
/// branch returns Some.
///
/// Note `splitn(3, '|')` after the prefix-strip means the THIRD field
/// (`sender_id`) may contain pipes if a future platform's id format
/// includes one. We don't expect this today (none of telegram /
/// slack / whatsapp / discord do), but the `splitn(3, _)` choice is
/// the more permissive of the two reasonable parses.
#[must_use]
pub fn decode(channel_id: &str) -> Option<(&str, &str, &str)> {
    let body = channel_id.strip_prefix(CHANNEL_ID_PREFIX)?;
    let mut parts = body.splitn(3, '|');
    let channel = parts.next()?;
    let account_id = parts.next()?;
    let sender_id = parts.next()?;
    if channel.is_empty() || account_id.is_empty() || sender_id.is_empty() {
        return None;
    }
    Some((channel, account_id, sender_id))
}

/// Validate that a config-supplied or platform-supplied string can be
/// safely embedded in the encoded channel_id. Currently just rejects
/// the `|` separator. Called by:
/// - Gateway config parser when validating `[[channels.<channel>.accounts]]
///   id = ...` (operator could otherwise paste a pipe).
/// - Channel WS handler when accepting an inbound's `sender_id`
///   (defence-in-depth — adapter shouldn't send pipes either).
///
/// Returns `Err(reason)` describing what went wrong; caller logs +
/// rejects.
pub fn validate_channel_id_part(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.contains('|') {
        return Err(format!(
            "{label} must not contain `|` (pipe is the channel_id separator); got {value:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_telegram() {
        let s = encode("telegram", "alice-tg-bot", "tg:123456789");
        assert_eq!(s, "ch|telegram|alice-tg-bot|tg:123456789");
        let (c, a, sender) = decode(&s).expect("decode");
        assert_eq!(c, "telegram");
        assert_eq!(a, "alice-tg-bot");
        assert_eq!(sender, "tg:123456789");
    }

    #[test]
    fn round_trip_slack() {
        let s = encode("slack", "team-help-bot", "slack:U07ABCDEF");
        let (c, a, sender) = decode(&s).expect("decode");
        assert_eq!(c, "slack");
        assert_eq!(a, "team-help-bot");
        assert_eq!(sender, "slack:U07ABCDEF");
    }

    #[test]
    fn decode_rejects_webchat_uuid() {
        // Webchat session_ids are bare UUIDs, no prefix. decode() must
        // return None so the gateway routes them through the webchat
        // path, not the channel-adapter path.
        let uuid = "019dfb59-3f45-7b60-9a1a-5a358262b607";
        assert!(decode(uuid).is_none());
    }

    #[test]
    fn decode_rejects_missing_fields() {
        assert!(decode("ch|telegram").is_none());
        assert!(decode("ch|telegram|alice").is_none());
        assert!(decode("ch||account|sender").is_none());
        assert!(decode("ch|telegram||sender").is_none());
        assert!(decode("ch|telegram|account|").is_none());
    }

    #[test]
    fn decode_handles_sender_id_with_internal_colons() {
        // Telegram's actual id format is `tg:<numeric>` — colons inside
        // the sender field must round-trip cleanly.
        let s = encode("telegram", "bot", "tg:123:extra");
        let (_, _, sender) = decode(&s).expect("decode");
        assert_eq!(sender, "tg:123:extra");
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_channel_id_part("account_id", "").is_err());
    }

    #[test]
    fn validate_rejects_pipe() {
        assert!(validate_channel_id_part("account_id", "alice|bot").is_err());
        assert!(validate_channel_id_part("channel", "tele|gram").is_err());
    }

    #[test]
    fn validate_accepts_typical_ids() {
        assert!(validate_channel_id_part("account_id", "alice-tg-bot").is_ok());
        assert!(validate_channel_id_part("channel", "telegram").is_ok());
        // Sender IDs commonly use colons; pipe is the only forbidden char.
        assert!(validate_channel_id_part("sender_id", "tg:123456789").is_ok());
        assert!(validate_channel_id_part("sender_id", "slack:U07ABCDEF").is_ok());
    }
}
