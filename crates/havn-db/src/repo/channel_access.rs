//! Channel invite codes and the persistent sender allowlist (issue #25).
//!
//! An invite is a one-time code bound to one `(channel, account_id)`
//! pair. Redeeming it adds the sender to `channel_allowlist`, which the
//! gateway unions with the account's TOML `allow_from`. Codes are 16
//! Crockford base32 characters (80 bits from the OS RNG), shown as four
//! dash-separated groups; only their SHA-256 is stored.

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest as _, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

use havn_core::UserId;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

/// Crockford base32: no I, L, O or U, so a code read aloud or retyped
/// has no look-alike characters.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 16;
const CODE_BYTES: usize = CODE_LEN * 5 / 8;
const GROUP_LEN: usize = 4;
const _: () = assert!(CODE_BYTES * 8 >= 80);

/// Default lifetime of an unused invite.
pub const DEFAULT_TTL: Duration = Duration::hours(24);
/// Lifetime of a pairing code made on the web, which links its sender.
pub const LINK_TTL: Duration = Duration::minutes(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteStatus {
    Active,
    Used,
    Expired,
    Revoked,
}

impl InviteStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Used => "used",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Invite {
    pub id: Uuid,
    pub channel: String,
    pub account_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub used_by_sender: Option<String>,
    pub used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// The Havn user a code made on the web links its sender to.
    pub user_id: Option<UserId>,
}

impl Invite {
    #[must_use]
    pub fn status(&self, now: DateTime<Utc>) -> InviteStatus {
        if self.used_at.is_some() {
            InviteStatus::Used
        } else if self.revoked_at.is_some() {
            InviteStatus::Revoked
        } else if self.expires_at <= now {
            InviteStatus::Expired
        } else {
            InviteStatus::Active
        }
    }
}

/// A freshly created invite plus its code, which exists only here.
#[derive(Debug, Clone)]
pub struct CreatedInvite {
    pub invite: Invite,
    /// Display form, e.g. `7K3M-QX9P-2HWD-RT4B`.
    pub code: String,
}

#[derive(Debug, Clone)]
pub struct AllowedSender {
    pub channel: String,
    pub account_id: String,
    pub sender_id: String,
    pub registered_at: DateTime<Utc>,
    pub invite_id: Option<Uuid>,
}

/// Generate a canonical (undashed) code from the OS RNG.
pub fn generate_code() -> Result<String> {
    let mut bytes = [0u8; CODE_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| DbError::Random(e.to_string()))?;
    let mut acc: u128 = 0;
    for b in bytes {
        acc = (acc << 8) | u128::from(b);
    }
    let mut out = String::with_capacity(CODE_LEN);
    for i in (0..CODE_LEN).rev() {
        out.push(char::from(ALPHABET[((acc >> (i * 5)) & 0x1f) as usize]));
    }
    Ok(out)
}

/// Canonical form of something a user typed: dashes (including the
/// Unicode dashes, minus sign and fullwidth, small and soft hyphens that
/// editors and chat apps substitute), whitespace and zero-width
/// characters dropped, upper-cased, and the
/// Crockford look-alikes (O to 0, I and L to 1) folded. `None` unless
/// the result is exactly one code.
#[must_use]
pub fn normalize_code(input: &str) -> Option<String> {
    let mut out = String::with_capacity(CODE_LEN);
    for c in input.chars() {
        let separator = matches!(
            c,
            '-' | '\u{2010}'..='\u{2015}'
                | '\u{2212}'
                | '\u{ff0d}'
                | '\u{fe63}'
                | '\u{ad}'
                | '\u{200b}'..='\u{200d}'
                | '\u{2060}'
                | '\u{feff}'
        );
        if separator || c.is_whitespace() {
            continue;
        }
        if !c.is_ascii() || out.len() == CODE_LEN {
            return None;
        }
        let c = match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !ALPHABET.contains(&(c as u8)) {
            return None;
        }
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// Hex SHA-256 of a canonical code: the value stored in `code_hash`.
#[must_use]
pub fn hash_code(canonical: &str) -> String {
    Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn display_code(canonical: &str) -> String {
    canonical
        .as_bytes()
        .chunks(GROUP_LEN)
        .map(|g| String::from_utf8_lossy(g).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// Same width as the `strftime('%Y-%m-%dT%H:%M:%fZ')` column defaults,
/// so stored timestamps compare lexicographically.
fn timestamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

pub async fn create_invite(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    ttl: Duration,
) -> Result<CreatedInvite> {
    create_code(pool, channel, account_id, ttl, None).await
}

/// A pairing code that, redeemed, also links its sender to `user` (issue
/// #182); it lasts [`LINK_TTL`].
pub async fn create_link_code(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    user: UserId,
) -> Result<CreatedInvite> {
    create_code(pool, channel, account_id, LINK_TTL, Some(user)).await
}

async fn create_code(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    ttl: Duration,
    user: Option<UserId>,
) -> Result<CreatedInvite> {
    let code = generate_code()?;
    let id = Uuid::now_v7();
    let expires_at = Utc::now() + ttl;
    sqlx::query(
        "INSERT INTO channel_invites (id, code_hash, channel, account_id, expires_at, user_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(id.to_string())
    .bind(hash_code(&code))
    .bind(channel)
    .bind(account_id)
    .bind(timestamp(expires_at))
    .bind(user.map(|u| u.to_string()))
    .execute(pool)
    .await?;
    let invite = find_invite(pool, id).await?.ok_or(DbError::NotFound)?;
    Ok(CreatedInvite {
        invite,
        code: display_code(&code),
    })
}

const INVITE_COLUMNS: &str = "id, channel, account_id, created_at, expires_at, \
                              used_by_sender, used_at, revoked_at, user_id";

pub async fn find_invite(pool: &SqlitePool, id: Uuid) -> Result<Option<Invite>> {
    let row: Option<InviteRow> = sqlx::query_as(&format!(
        "SELECT {INVITE_COLUMNS} FROM channel_invites WHERE id = ?1"
    ))
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(Invite::try_from).transpose()
}

/// Invites, newest first, optionally for one channel and/or account.
/// A channel filter uses `idx_channel_invites_account`.
pub async fn list_invites(
    pool: &SqlitePool,
    channel: Option<&str>,
    account_id: Option<&str>,
) -> Result<Vec<Invite>> {
    let mut sql = format!("SELECT {INVITE_COLUMNS} FROM channel_invites WHERE 1 = 1");
    if channel.is_some() {
        sql.push_str(" AND channel = ?");
    }
    if account_id.is_some() {
        sql.push_str(" AND account_id = ?");
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");
    let mut q = sqlx::query_as::<_, InviteRow>(&sql);
    for v in [channel, account_id].into_iter().flatten() {
        q = q.bind(v);
    }
    let rows = q.fetch_all(pool).await?;
    rows.into_iter().map(Invite::try_from).collect()
}

/// Revoke an unused invite. Revoking twice is a no-op; a used invite is
/// `Conflict` (the sender it admitted is removed with [`revoke_sender`]).
pub async fn revoke_invite(pool: &SqlitePool, id: Uuid) -> Result<()> {
    let res = sqlx::query(
        "UPDATE channel_invites SET revoked_at = ?1 \
         WHERE id = ?2 AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(timestamp(Utc::now()))
    .bind(id.to_string())
    .execute(pool)
    .await?;
    if res.rows_affected() == 1 {
        return Ok(());
    }
    match find_invite(pool, id).await? {
        None => Err(DbError::NotFound),
        Some(invite) if invite.used_at.is_some() => {
            Err(DbError::Conflict("channel_invites.used_at"))
        }
        Some(_) => Ok(()),
    }
}

/// Consume the invite matching `code` for this `(channel, account_id)`
/// and admit `sender_id`; a code made on the web also links the sender to
/// its user, replacing the links either had on this channel. `None` when
/// the code is malformed, unknown, for another account, used, revoked or
/// expired; those cases are not told apart so a caller cannot leak which
/// one applied. The claim is a single conditional UPDATE, so two senders
/// racing for one code cannot both win. `None` too when `sender_id` is
/// already admitted and the code links no one: the claim is rolled back
/// and the code stays usable. With `links_only`, only a code made on the
/// web is claimed. A code made on the web is refused with `Conflict` when
/// the sender is linked to another user, who must unlink them first;
/// linking a user's new sender takes back the access the user's codes
/// gave their earlier one.
pub async fn redeem(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
    code: &str,
    now: DateTime<Utc>,
    links_only: bool,
) -> Result<Option<Invite>> {
    let Some(canonical) = normalize_code(code) else {
        return Ok(None);
    };
    let now_ts = timestamp(now);
    let mut tx = pool.begin().await?;
    let claimed: Option<InviteRow> = sqlx::query_as(&format!(
        "UPDATE channel_invites SET used_by_sender = ?1, used_at = ?2 \
         WHERE code_hash = ?3 AND channel = ?4 AND account_id = ?5 \
           AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?2 \
           AND (?6 = 0 OR user_id IS NOT NULL) \
         RETURNING {INVITE_COLUMNS}"
    ))
    .bind(sender_id)
    .bind(&now_ts)
    .bind(hash_code(&canonical))
    .bind(channel)
    .bind(account_id)
    .bind(links_only)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = claimed else {
        return Ok(None);
    };
    let inserted = sqlx::query(
        "INSERT INTO channel_allowlist (channel, account_id, sender_id, registered_at, invite_id) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (channel, account_id, sender_id) DO NOTHING",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .bind(&now_ts)
    .bind(&row.id)
    .execute(&mut *tx)
    .await?;
    let invite = Invite::try_from(row)?;
    match invite.user_id {
        Some(user) => {
            let linked: Vec<(String, String)> = sqlx::query_as(
                "SELECT sender_id, user_id FROM channel_links \
                 WHERE channel = ?1 AND (sender_id = ?2 OR user_id = ?3)",
            )
            .bind(channel)
            .bind(sender_id)
            .bind(user.to_string())
            .fetch_all(&mut *tx)
            .await?;
            if linked
                .iter()
                .any(|(s, u)| s == sender_id && *u != user.to_string())
            {
                tx.rollback().await?;
                return Err(DbError::Conflict("channel_links.sender_id"));
            }
            // The user's earlier sender on this channel loses what the
            // user's codes gave it.
            for (old, _) in linked.iter().filter(|(s, _)| s != sender_id) {
                revoke_link_access(&mut tx, channel, old, user).await?;
            }
            sqlx::query(
                "DELETE FROM channel_links WHERE channel = ?1 AND (sender_id = ?2 OR user_id = ?3)",
            )
            .bind(channel)
            .bind(sender_id)
            .bind(user.to_string())
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO channel_links (channel, sender_id, user_id, linked_at) VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(channel)
            .bind(sender_id)
            .bind(user.to_string())
            .bind(&now_ts)
            .execute(&mut *tx)
            .await?;
        }
        None if inserted.rows_affected() == 0 => {
            tx.rollback().await?;
            return Ok(None);
        }
        None => {}
    }
    tx.commit().await?;
    Ok(Some(invite))
}

/// A channel sender linked to a Havn user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelLink {
    pub channel: String,
    pub sender_id: String,
    pub user_id: UserId,
    /// The user's name, for listings.
    pub user_name: String,
    pub linked_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct LinkRow {
    channel: String,
    sender_id: String,
    user_id: String,
    user_name: String,
    linked_at: DateTime<Utc>,
}

impl TryFrom<LinkRow> for ChannelLink {
    type Error = DbError;
    fn try_from(r: LinkRow) -> Result<Self> {
        Ok(Self {
            user_id: UserId::from_uuid(parse_db_uuid(&r.user_id, "channel_links.user_id")?),
            channel: r.channel,
            sender_id: r.sender_id,
            user_name: r.user_name,
            linked_at: r.linked_at,
        })
    }
}

const LINK_SELECT: &str = "SELECT l.channel, l.sender_id, l.user_id, u.display_name AS user_name, \
                           l.linked_at FROM channel_links l JOIN users u ON u.id = l.user_id";

/// The user `sender_id` on `channel` is linked to.
pub async fn linked_user(
    pool: &SqlitePool,
    channel: &str,
    sender_id: &str,
) -> Result<Option<ChannelLink>> {
    let row: Option<LinkRow> = sqlx::query_as(&format!(
        "{LINK_SELECT} WHERE l.channel = ?1 AND l.sender_id = ?2"
    ))
    .bind(channel)
    .bind(sender_id)
    .fetch_optional(pool)
    .await?;
    row.map(ChannelLink::try_from).transpose()
}

/// Every link, or `user`'s, by channel and sender.
pub async fn list_links(pool: &SqlitePool, user: Option<UserId>) -> Result<Vec<ChannelLink>> {
    let rows: Vec<LinkRow> = match user {
        Some(user) => {
            sqlx::query_as(&format!(
                "{LINK_SELECT} WHERE l.user_id = ?1 ORDER BY l.channel, l.sender_id"
            ))
            .bind(user.to_string())
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(&format!("{LINK_SELECT} ORDER BY l.channel, l.sender_id"))
                .fetch_all(pool)
                .await?
        }
    };
    rows.into_iter().map(ChannelLink::try_from).collect()
}

/// Remove the link of `sender_id` on `channel`, whoever it is to, as
/// [`unlink`] does; the link removed, if there was one. For the person in
/// the chat app (`/disconnect`) and the operator: a sender linked to the
/// wrong user is not stuck with it.
pub async fn unlink_sender(
    pool: &SqlitePool,
    channel: &str,
    sender_id: &str,
) -> Result<Option<ChannelLink>> {
    let Some(link) = linked_user(pool, channel, sender_id).await? else {
        return Ok(None);
    };
    Ok(unlink(pool, link.user_id, channel, sender_id)
        .await?
        .then_some(link))
}

/// Take back from `sender_id` on `channel` the admissions that codes
/// `user` made on the web gave it; admissions it had otherwise stay.
async fn revoke_link_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    channel: &str,
    sender_id: &str,
    user: UserId,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM channel_allowlist WHERE channel = ?1 AND sender_id = ?2 \
         AND invite_id IN (SELECT id FROM channel_invites WHERE user_id = ?3)",
    )
    .bind(channel)
    .bind(sender_id)
    .bind(user.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Remove `user`'s link of `sender_id` on `channel`, with the access the
/// user's codes gave the sender; false when there was no such link. An
/// admission the sender had otherwise (`allow_from`, an invite) stays.
pub async fn unlink(
    pool: &SqlitePool,
    user: UserId,
    channel: &str,
    sender_id: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "DELETE FROM channel_links WHERE channel = ?1 AND sender_id = ?2 AND user_id = ?3",
    )
    .bind(channel)
    .bind(sender_id)
    .bind(user.to_string())
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    revoke_link_access(&mut tx, channel, sender_id, user).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn is_allowed(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
) -> Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM channel_allowlist \
         WHERE channel = ?1 AND account_id = ?2 AND sender_id = ?3",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn count_allowed(pool: &SqlitePool, channel: &str, account_id: &str) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM channel_allowlist WHERE channel = ?1 AND account_id = ?2",
    )
    .bind(channel)
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Every sender admitted by invite, ordered by account then registration.
pub async fn list_allowed(pool: &SqlitePool) -> Result<Vec<AllowedSender>> {
    let rows: Vec<AllowedRow> = sqlx::query_as(
        "SELECT channel, account_id, sender_id, registered_at, invite_id \
         FROM channel_allowlist ORDER BY channel, account_id, registered_at",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(AllowedSender::try_from).collect()
}

/// Remove every sender admitted to `account_id`, on `channel` or on any
/// channel. Returns how many were removed.
pub async fn revoke_account(
    pool: &SqlitePool,
    channel: Option<&str>,
    account_id: &str,
) -> Result<u64> {
    let res = sqlx::query(
        "DELETE FROM channel_allowlist WHERE account_id = ?1 AND (?2 IS NULL OR channel = ?2)",
    )
    .bind(account_id)
    .bind(channel)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Remove an admitted sender. The gateway reads the table on every
/// inbound message, so this takes effect on their next message.
pub async fn revoke_sender(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
) -> Result<()> {
    let res = sqlx::query(
        "DELETE FROM channel_allowlist WHERE channel = ?1 AND account_id = ?2 AND sender_id = ?3",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Senders turned away kept per account (issue #231): past it, those
/// who wrote longest ago are forgotten.
pub const REFUSALS_KEPT: i64 = 100;

/// Characters kept of what a turned-away sender last said.
pub const REFUSAL_SAID_CHARS: usize = 200;

/// How long after the owner was told about a sender they are not told
/// again.
pub const REFUSAL_NOTICE_EVERY: Duration = Duration::hours(24);

/// A sender an `allowlist` account turned away (issue #231).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub sender_id: String,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
    pub count: i64,
    /// The start of what they last said.
    pub said: String,
}

#[derive(Debug, sqlx::FromRow)]
struct RefusalRow {
    sender_id: String,
    first_at: DateTime<Utc>,
    last_at: DateTime<Utc>,
    count: i64,
    said: String,
}

/// Notices about senders turned away one account sends its owner in an
/// hour, at most: rotating ids cannot flood them.
pub const REFUSAL_NOTICES_PER_HOUR: i64 = 5;

/// Days a sender turned away is kept after they last wrote.
pub const REFUSAL_KEPT_DAYS: i64 = 30;

/// Note that `sender_id` was turned away by `(channel, account_id)` at
/// `now`, having said `said`. Whether the owner should be told: the first
/// time, and once a [`REFUSAL_NOTICE_EVERY`] after, while the account sent
/// fewer than [`REFUSAL_NOTICES_PER_HOUR`] in the last hour; a notice sent
/// is noted with [`refusal_told`]. The account keeps the
/// [`REFUSALS_KEPT`] who wrote last, and every one told about in the last
/// day, for [`REFUSAL_KEPT_DAYS`].
pub async fn record_refusal(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
    said: &str,
    now: DateTime<Utc>,
) -> Result<bool> {
    let said: String = said.trim().chars().take(REFUSAL_SAID_CHARS).collect();
    let at = timestamp(now);
    let day_ago = timestamp(now - REFUSAL_NOTICE_EVERY);
    let hour_ago = timestamp(now - Duration::hours(1));
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let before: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT notified_at FROM channel_refusals \
         WHERE channel = ?1 AND account_id = ?2 AND sender_id = ?3",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO channel_refusals (channel, account_id, sender_id, first_at, last_at, said) \
         VALUES (?1, ?2, ?3, ?4, ?4, ?5) \
         ON CONFLICT (channel, account_id, sender_id) DO UPDATE SET \
             first_at = CASE WHEN last_at < ?6 THEN excluded.first_at ELSE first_at END, \
             count = CASE WHEN last_at < ?6 THEN 1 ELSE count + 1 END, \
             last_at = excluded.last_at, said = excluded.said",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .bind(&at)
    .bind(&said)
    .bind(timestamp(now - Duration::days(REFUSAL_KEPT_DAYS)))
    .execute(&mut *tx)
    .await?;
    if before.is_none() {
        sqlx::query(
            "DELETE FROM channel_refusals WHERE channel = ?1 AND account_id = ?2 \
             AND (last_at < ?5 OR ((notified_at IS NULL OR notified_at <= ?4) AND sender_id NOT IN \
                 (SELECT sender_id FROM channel_refusals WHERE channel = ?1 AND account_id = ?2 \
                  ORDER BY last_at DESC LIMIT ?3)))",
        )
        .bind(channel)
        .bind(account_id)
        .bind(REFUSALS_KEPT)
        .bind(&day_ago)
        .bind(timestamp(now - Duration::days(REFUSAL_KEPT_DAYS)))
        .execute(&mut *tx)
        .await?;
    }
    let due = match &before {
        None | Some((None,)) => true,
        Some((Some(told),)) => told.as_str() <= day_ago.as_str(),
    };
    let (recent,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM channel_refusals \
         WHERE channel = ?1 AND account_id = ?2 AND notified_at > ?3",
    )
    .bind(channel)
    .bind(account_id)
    .bind(&hour_ago)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(due && recent < REFUSAL_NOTICES_PER_HOUR)
}

/// Note that the owner was told about `sender_id` at `now`.
pub async fn refusal_told(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE channel_refusals SET notified_at = ?4 \
         WHERE channel = ?1 AND account_id = ?2 AND sender_id = ?3",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .bind(timestamp(now))
    .execute(pool)
    .await?;
    Ok(())
}

/// The senders admitted by invite or by the owner to `(channel,
/// account_id)`, by registration.
pub async fn list_allowed_on(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
) -> Result<Vec<AllowedSender>> {
    let rows: Vec<AllowedRow> = sqlx::query_as(
        "SELECT channel, account_id, sender_id, registered_at, invite_id FROM channel_allowlist \
         WHERE channel = ?1 AND account_id = ?2 ORDER BY registered_at",
    )
    .bind(channel)
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(AllowedSender::try_from).collect()
}

/// The senders `(channel, account_id)` turned away in the last
/// [`REFUSAL_KEPT_DAYS`], who wrote last first.
pub async fn list_refusals(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
) -> Result<Vec<Refusal>> {
    let rows: Vec<RefusalRow> = sqlx::query_as(
        "SELECT sender_id, first_at, last_at, count, said FROM channel_refusals \
         WHERE channel = ?1 AND account_id = ?2 AND last_at >= ?3 ORDER BY last_at DESC",
    )
    .bind(channel)
    .bind(account_id)
    .bind(timestamp(Utc::now() - Duration::days(REFUSAL_KEPT_DAYS)))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Refusal {
            sender_id: r.sender_id,
            first_at: r.first_at,
            last_at: r.last_at,
            count: r.count,
            said: r.said,
        })
        .collect())
}

/// Forget that `sender_id` was turned away; whether they were.
pub async fn forget_refusal(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
) -> Result<bool> {
    let res = sqlx::query(
        "DELETE FROM channel_refusals WHERE channel = ?1 AND account_id = ?2 AND sender_id = ?3",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Admit `sender_id` to `(channel, account_id)` without a code, as its
/// agent's owner does from the dashboard or a notice (issue #231), and
/// forget that it was turned away. Whether they were not admitted yet.
pub async fn admit(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let res = sqlx::query(
        "INSERT OR IGNORE INTO channel_allowlist (channel, account_id, sender_id, registered_at) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .bind(timestamp(Utc::now()))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM channel_refusals WHERE channel = ?1 AND account_id = ?2 AND sender_id = ?3",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(res.rows_affected() > 0)
}

#[derive(Debug, sqlx::FromRow)]
struct InviteRow {
    id: String,
    channel: String,
    account_id: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    used_by_sender: Option<String>,
    used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    user_id: Option<String>,
}

impl TryFrom<InviteRow> for Invite {
    type Error = DbError;
    fn try_from(r: InviteRow) -> Result<Self> {
        Ok(Self {
            id: parse_db_uuid(&r.id, "channel_invites.id")?,
            channel: r.channel,
            account_id: r.account_id,
            created_at: r.created_at,
            expires_at: r.expires_at,
            used_by_sender: r.used_by_sender,
            used_at: r.used_at,
            revoked_at: r.revoked_at,
            user_id: r
                .user_id
                .as_deref()
                .map(|u| parse_db_uuid(u, "channel_invites.user_id").map(UserId::from_uuid))
                .transpose()?,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct AllowedRow {
    channel: String,
    account_id: String,
    sender_id: String,
    registered_at: DateTime<Utc>,
    invite_id: Option<String>,
}

impl TryFrom<AllowedRow> for AllowedSender {
    type Error = DbError;
    fn try_from(r: AllowedRow) -> Result<Self> {
        Ok(Self {
            channel: r.channel,
            account_id: r.account_id,
            sender_id: r.sender_id,
            registered_at: r.registered_at,
            invite_id: r
                .invite_id
                .as_deref()
                .map(|id| parse_db_uuid(id, "channel_allowlist.invite_id"))
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn senders_turned_away_are_kept_admitted_and_told_of_once_a_day() {
        let pool = crate::connect_in_memory().await.expect("db");
        let t0 = Utc::now();
        let record = |sender: &'static str, said: &'static str, at: DateTime<Utc>| {
            let pool = pool.clone();
            async move {
                record_refusal(&pool, "telegram", "bot", sender, said, at)
                    .await
                    .expect("record")
            }
        };
        assert!(record("tg:1", "hi, it is Bob", t0).await);
        assert!(record("tg:1", "hello?", t0).await, "due until told");
        refusal_told(&pool, "telegram", "bot", "tg:1", t0)
            .await
            .expect("told");
        assert!(
            !record("tg:1", "hello?", t0 + Duration::hours(1)).await,
            "told once"
        );
        assert!(
            record("tg:1", "anyone?", t0 + Duration::hours(25)).await,
            "and again a day later"
        );

        let long = "x".repeat(REFUSAL_SAID_CHARS * 2);
        record_refusal(&pool, "telegram", "other", "tg:2", &long, t0)
            .await
            .expect("other");
        let refused = list_refusals(&pool, "telegram", "bot").await.expect("list");
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].sender_id, "tg:1");
        assert_eq!(refused[0].count, 4);
        assert_eq!(refused[0].said, "anyone?");
        assert_eq!(
            list_refusals(&pool, "telegram", "other")
                .await
                .expect("list")[0]
                .said
                .chars()
                .count(),
            REFUSAL_SAID_CHARS
        );

        assert!(
            admit(&pool, "telegram", "bot", "tg:1")
                .await
                .expect("admit")
        );
        assert!(
            !admit(&pool, "telegram", "bot", "tg:1")
                .await
                .expect("again"),
            "once"
        );
        assert!(
            is_allowed(&pool, "telegram", "bot", "tg:1")
                .await
                .expect("allowed")
        );
        assert!(
            !is_allowed(&pool, "telegram", "other", "tg:1")
                .await
                .expect("only there")
        );
        assert!(
            list_refusals(&pool, "telegram", "bot")
                .await
                .expect("list")
                .is_empty()
        );
        assert_eq!(
            list_allowed_on(&pool, "telegram", "bot")
                .await
                .expect("on")
                .len(),
            1
        );
        assert!(
            list_allowed_on(&pool, "telegram", "other")
                .await
                .expect("on")
                .is_empty()
        );

        assert!(
            forget_refusal(&pool, "telegram", "other", "tg:2")
                .await
                .expect("forget")
        );
        assert!(
            !forget_refusal(&pool, "telegram", "other", "tg:2")
                .await
                .expect("gone")
        );

        // Someone back after the days kept starts over.
        for _ in 0..2 {
            record_refusal(&pool, "telegram", "back", "tg:3", "", Utc::now())
                .await
                .expect("record");
        }
        sqlx::query(
            "UPDATE channel_refusals SET last_at = ?1, first_at = ?1 WHERE sender_id = 'tg:3'",
        )
        .bind(timestamp(
            Utc::now() - Duration::days(REFUSAL_KEPT_DAYS + 5),
        ))
        .execute(&pool)
        .await
        .expect("age");
        assert!(
            list_refusals(&pool, "telegram", "back")
                .await
                .expect("list")
                .is_empty()
        );
        record_refusal(&pool, "telegram", "back", "tg:3", "hi", Utc::now())
            .await
            .expect("back");
        let refused = list_refusals(&pool, "telegram", "back")
            .await
            .expect("list");
        assert_eq!(refused[0].count, 1);
        assert!(refused[0].first_at > Utc::now() - Duration::hours(1));
    }

    #[tokio::test]
    async fn rotating_senders_cannot_flood_the_owner() {
        let pool = crate::connect_in_memory().await.expect("db");
        let t0 = Utc::now();
        let mut told = 0;
        for n in 0..(REFUSALS_KEPT + 30) {
            let sender = format!("tg:{n}");
            let at = t0 + Duration::seconds(n);
            if record_refusal(&pool, "telegram", "busy", &sender, "", at)
                .await
                .expect("record")
            {
                refusal_told(&pool, "telegram", "busy", &sender, at)
                    .await
                    .expect("told");
                told += 1;
            }
        }
        assert_eq!(told, REFUSAL_NOTICES_PER_HOUR, "a few an hour");
        let kept = list_refusals(&pool, "telegram", "busy")
            .await
            .expect("list");
        assert_eq!(
            kept.len() as i64,
            REFUSALS_KEPT + REFUSAL_NOTICES_PER_HOUR,
            "{}",
            kept.len()
        );
        assert_eq!(
            kept[0].sender_id,
            format!("tg:{}", REFUSALS_KEPT + 29),
            "the latest kept"
        );
        assert!(
            kept.iter().any(|r| r.sender_id == "tg:0"),
            "one told about is kept for the day, so it is not told again"
        );
        let later = t0 + Duration::hours(2);
        assert!(
            !record_refusal(&pool, "telegram", "busy", "tg:0", "", later)
                .await
                .expect("again")
        );
        assert!(
            record_refusal(&pool, "telegram", "busy", "tg:new", "", later)
                .await
                .expect("new"),
            "the hour's budget is back"
        );
        let month = t0 + Duration::days(REFUSAL_KEPT_DAYS + 1);
        record_refusal(&pool, "telegram", "busy", "tg:late", "", month)
            .await
            .expect("late");
        let kept = list_refusals(&pool, "telegram", "busy")
            .await
            .expect("list");
        assert_eq!(
            kept.iter()
                .map(|r| r.sender_id.as_str())
                .collect::<Vec<_>>(),
            vec!["tg:late"],
            "the old ones are forgotten"
        );
    }
    use crate::connect_in_memory;

    async fn user(pool: &SqlitePool, name: &str) -> UserId {
        crate::repo::users::create(pool, crate::repo::users::NewUser { display_name: name })
            .await
            .expect("user")
            .id
    }

    #[tokio::test]
    async fn a_web_code_admits_and_links_its_sender() {
        let pool = connect_in_memory().await.expect("db");
        let alice = user(&pool, "Alice").await;
        let code = create_link_code(&pool, "telegram", "bot", alice)
            .await
            .expect("code");
        assert_eq!(code.invite.user_id, Some(alice));
        assert!(code.invite.expires_at <= Utc::now() + LINK_TTL);
        let now = Utc::now();
        let used = redeem(&pool, "telegram", "bot", "tg:1", &code.code, now, false)
            .await
            .expect("redeem");
        assert_eq!(used.and_then(|i| i.user_id), Some(alice));
        assert!(
            is_allowed(&pool, "telegram", "bot", "tg:1")
                .await
                .expect("allowed")
        );
        let link = linked_user(&pool, "telegram", "tg:1")
            .await
            .expect("link")
            .expect("linked");
        assert_eq!((link.user_id, link.user_name.as_str()), (alice, "Alice"));
        assert!(
            redeem(&pool, "telegram", "bot", "tg:2", &code.code, now, false)
                .await
                .expect("redeem")
                .is_none(),
            "once only"
        );
    }

    #[tokio::test]
    async fn an_admitted_sender_still_links_and_linking_again_replaces() {
        let pool = connect_in_memory().await.expect("db");
        let alice = user(&pool, "Alice").await;
        let bob = user(&pool, "Bob").await;
        let now = Utc::now();
        // Admitted by a plain invite first: a second plain invite does nothing.
        let plain = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("invite");
        redeem(&pool, "telegram", "bot", "tg:1", &plain.code, now, false)
            .await
            .expect("redeem");
        let again = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("invite");
        assert!(
            redeem(&pool, "telegram", "bot", "tg:1", &again.code, now, false)
                .await
                .expect("redeem")
                .is_none()
        );
        assert_eq!(
            linked_user(&pool, "telegram", "tg:1").await.expect("link"),
            None
        );
        // A web code links them though.
        let a = create_link_code(&pool, "telegram", "bot", alice)
            .await
            .expect("code");
        assert!(
            redeem(&pool, "telegram", "bot", "tg:1", &a.code, now, false)
                .await
                .expect("redeem")
                .is_some()
        );
        assert_eq!(
            linked_user(&pool, "telegram", "tg:1")
                .await
                .expect("link")
                .map(|l| l.user_id),
            Some(alice)
        );
        // Alice links another Telegram account: it replaces the first.
        let a2 = create_link_code(&pool, "telegram", "bot", alice)
            .await
            .expect("code");
        redeem(&pool, "telegram", "bot", "tg:2", &a2.code, now, false)
            .await
            .expect("redeem");
        assert_eq!(
            linked_user(&pool, "telegram", "tg:1").await.expect("link"),
            None
        );
        // tg:1 lost what Alice's first code gave it; its invite stays.
        assert!(
            is_allowed(&pool, "telegram", "bot", "tg:1")
                .await
                .expect("allowed")
        );
        // Bob cannot take tg:2 over while it is Alice's; the code stays unused.
        let b = create_link_code(&pool, "telegram", "bot", bob)
            .await
            .expect("code");
        let taken = redeem(&pool, "telegram", "bot", "tg:2", &b.code, now, false).await;
        assert!(matches!(taken, Err(DbError::Conflict(_))), "{taken:?}");
        assert!(
            !unlink(&pool, bob, "telegram", "tg:2")
                .await
                .expect("unlink"),
            "not Bob's"
        );
        // Alice disconnects it: it loses what her code gave it.
        assert!(
            unlink(&pool, alice, "telegram", "tg:2")
                .await
                .expect("unlink")
        );
        assert!(
            !is_allowed(&pool, "telegram", "bot", "tg:2")
                .await
                .expect("allowed")
        );
        // Then Bob's code links it to Bob.
        redeem(&pool, "telegram", "bot", "tg:2", &b.code, now, false)
            .await
            .expect("redeem")
            .expect("redeemed");
        assert_eq!(
            linked_user(&pool, "telegram", "tg:2")
                .await
                .expect("link")
                .map(|l| l.user_id),
            Some(bob)
        );
        assert_eq!(list_links(&pool, None).await.expect("links").len(), 1);
    }

    #[tokio::test]
    async fn the_sender_can_drop_a_link_made_to_someone_else() {
        let pool = connect_in_memory().await.expect("db");
        let mallory = user(&pool, "Mallory").await;
        let alice = user(&pool, "Alice").await;
        let now = Utc::now();
        let theirs = create_link_code(&pool, "telegram", "bot", mallory)
            .await
            .expect("code");
        redeem(
            &pool,
            "telegram",
            "bot",
            "tg:alice",
            &theirs.code,
            now,
            false,
        )
        .await
        .expect("redeem");
        let dropped = unlink_sender(&pool, "telegram", "tg:alice")
            .await
            .expect("unlink")
            .expect("was linked");
        assert_eq!(dropped.user_id, mallory);
        assert!(
            !is_allowed(&pool, "telegram", "bot", "tg:alice")
                .await
                .expect("allowed")
        );
        let own = create_link_code(&pool, "telegram", "bot", alice)
            .await
            .expect("code");
        redeem(&pool, "telegram", "bot", "tg:alice", &own.code, now, false)
            .await
            .expect("redeem")
            .expect("linked to Alice now");
        assert!(
            unlink_sender(&pool, "telegram", "tg:nobody")
                .await
                .expect("unlink")
                .is_none()
        );
    }

    #[tokio::test]
    async fn links_only_leaves_plain_invites_alone() {
        let pool = connect_in_memory().await.expect("db");
        let plain = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("invite");
        let now = Utc::now();
        let skipped = redeem(&pool, "telegram", "bot", "tg:1", &plain.code, now, true)
            .await
            .expect("redeem");
        assert!(skipped.is_none());
        let used = redeem(&pool, "telegram", "bot", "tg:2", &plain.code, now, false)
            .await
            .expect("redeem");
        assert!(used.is_some());
    }

    #[tokio::test]
    async fn an_expired_web_code_links_no_one() {
        let pool = connect_in_memory().await.expect("db");
        let alice = user(&pool, "Alice").await;
        let code = create_link_code(&pool, "telegram", "bot", alice)
            .await
            .expect("code");
        let later = Utc::now() + LINK_TTL + Duration::seconds(1);
        let used = redeem(&pool, "telegram", "bot", "tg:1", &code.code, later, false)
            .await
            .expect("redeem");
        assert!(used.is_none());
    }

    #[tokio::test]
    async fn a_web_code_works_only_for_its_account() {
        let pool = connect_in_memory().await.expect("db");
        let alice = user(&pool, "Alice").await;
        let code = create_link_code(&pool, "telegram", "bot", alice)
            .await
            .expect("code");
        let other = redeem(
            &pool,
            "telegram",
            "other-bot",
            "tg:1",
            &code.code,
            Utc::now(),
            false,
        )
        .await
        .expect("redeem");
        assert!(other.is_none());
        assert_eq!(
            linked_user(&pool, "telegram", "tg:1").await.expect("link"),
            None
        );
    }

    #[test]
    fn generated_codes_are_canonical_and_distinct() {
        let a = generate_code().expect("code");
        let b = generate_code().expect("code");
        assert_eq!(a.len(), CODE_LEN);
        assert_eq!(normalize_code(&a).as_deref(), Some(a.as_str()));
        assert_ne!(a, b);
    }

    #[test]
    fn normalize_accepts_typing_variants() {
        let canonical = "7K3MQX9P2HWDRT4B";
        for typed in [
            "7K3M-QX9P-2HWD-RT4B",
            " 7k3m qx9p 2hwd rt4b\n",
            "7k3mqx9p2hwdrt4b",
        ] {
            assert_eq!(normalize_code(typed).as_deref(), Some(canonical), "{typed}");
        }
        assert_eq!(
            normalize_code("OIL0-0000-0000-0000").as_deref(),
            Some("0110000000000000")
        );
        for pasted in [
            "7K3M\u{2010}QX9P\u{2013}2HWD\u{2014}RT4B",
            "7K3M\u{2212}QX9P\u{2011}2HWD\u{2015}RT4B",
            "\u{feff}7K3M-\u{200b}QX9P-2HWD\u{200d}-RT4B\u{2060}",
            "\u{a0}7K3M QX9P\u{a0}2HWD RT4B\u{a0}",
            "7K3M\u{ff0d}QX9P\u{fe63}2HWD\u{ad}RT4B",
            "7K3M\u{ad}QX9P\u{ad}2HWD\u{ad}RT4B\u{ad}",
        ] {
            assert_eq!(
                normalize_code(pasted).as_deref(),
                Some(canonical),
                "{pasted:?}"
            );
        }
        assert_eq!(normalize_code("7K3M-QX9P-2HWD-RT4"), None);
        assert_eq!(normalize_code("7K3M-QX9P-2HWD-RT4BX"), None);
        assert_eq!(normalize_code("7K3M-QX9P-2HWD-RT4U"), None);
        assert_eq!(normalize_code("hello, can you help me"), None);
        assert_eq!(normalize_code("7K3M-QX9P-2HWD-RT4\u{e9}"), None);
    }

    #[tokio::test]
    async fn create_stores_only_the_hash() {
        let pool = connect_in_memory().await.expect("db");
        let created = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("create");
        assert_eq!(created.code.len(), CODE_LEN + 3);
        assert_eq!(created.code.matches('-').count(), 3);
        let canonical = normalize_code(&created.code).expect("canonical");
        let (stored,): (String,) = sqlx::query_as("SELECT code_hash FROM channel_invites")
            .fetch_one(&pool)
            .await
            .expect("row");
        assert_eq!(stored, hash_code(&canonical));
        assert_eq!(stored.len(), 64);
        let dump: Vec<(String,)> = sqlx::query_as(
            "SELECT id || code_hash || channel || account_id || expires_at FROM channel_invites",
        )
        .fetch_all(&pool)
        .await
        .expect("dump");
        assert!(!dump[0].0.contains(&canonical));
        let invite = &created.invite;
        assert_eq!(invite.status(Utc::now()), InviteStatus::Active);
        let ttl = invite.expires_at - invite.created_at;
        assert!((ttl - DEFAULT_TTL).num_seconds().abs() < 5, "{ttl}");
    }

    #[tokio::test]
    async fn redeem_admits_once() {
        let pool = connect_in_memory().await.expect("db");
        let created = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("create");
        assert!(!is_allowed(&pool, "telegram", "bot", "tg:1").await.unwrap());
        let typed = created.code.to_lowercase();
        let used = redeem(&pool, "telegram", "bot", "tg:1", &typed, Utc::now(), false)
            .await
            .expect("redeem")
            .expect("valid code");
        assert_eq!(used.id, created.invite.id);
        assert_eq!(used.used_by_sender.as_deref(), Some("tg:1"));
        assert_eq!(used.status(Utc::now()), InviteStatus::Used);
        assert!(is_allowed(&pool, "telegram", "bot", "tg:1").await.unwrap());

        let again = redeem(
            &pool,
            "telegram",
            "bot",
            "tg:2",
            &created.code,
            Utc::now(),
            false,
        )
        .await
        .expect("redeem");
        assert!(
            again.is_none(),
            "a used code must not admit a second sender"
        );
        assert!(!is_allowed(&pool, "telegram", "bot", "tg:2").await.unwrap());

        let listed = list_allowed(&pool).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].invite_id, Some(created.invite.id));
    }

    #[tokio::test]
    async fn redeem_rejects_expired_wrong_account_and_revoked() {
        let pool = connect_in_memory().await.expect("db");
        let created = create_invite(&pool, "telegram", "bot", Duration::hours(1))
            .await
            .expect("create");
        let later = Utc::now() + Duration::hours(2);
        assert!(
            redeem(
                &pool,
                "telegram",
                "bot",
                "tg:1",
                &created.code,
                later,
                false
            )
            .await
            .unwrap()
            .is_none()
        );
        for (channel, account) in [("telegram", "other-bot"), ("slack", "bot")] {
            assert!(
                redeem(
                    &pool,
                    channel,
                    account,
                    "tg:1",
                    &created.code,
                    Utc::now(),
                    false
                )
                .await
                .unwrap()
                .is_none(),
                "{channel}/{account}"
            );
        }
        revoke_invite(&pool, created.invite.id)
            .await
            .expect("revoke");
        revoke_invite(&pool, created.invite.id)
            .await
            .expect("second revoke is a no-op");
        assert!(
            redeem(
                &pool,
                "telegram",
                "bot",
                "tg:1",
                &created.code,
                Utc::now(),
                false
            )
            .await
            .unwrap()
            .is_none()
        );
        let invite = find_invite(&pool, created.invite.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(invite.status(Utc::now()), InviteStatus::Revoked);
        assert!(invite.used_at.is_none());
        assert_eq!(count_allowed(&pool, "telegram", "bot").await.unwrap(), 0);
    }

    async fn file_pool(dir: &tempfile::TempDir) -> SqlitePool {
        crate::connect(dir.path().join("havn.db"))
            .await
            .expect("db")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_redeems_of_one_code_admit_one_sender() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = file_pool(&dir).await;
        for _ in 0..10 {
            let created = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
                .await
                .expect("create");
            let tasks: Vec<_> = (0..4)
                .map(|i| {
                    let pool = pool.clone();
                    let code = created.code.clone();
                    tokio::spawn(async move {
                        redeem(
                            &pool,
                            "telegram",
                            "bot",
                            &format!("tg:{i}"),
                            &code,
                            Utc::now(),
                            false,
                        )
                        .await
                        .expect("redeem")
                    })
                })
                .collect();
            let mut winners = 0;
            for t in tasks {
                winners += usize::from(t.await.expect("join").is_some());
            }
            assert_eq!(winners, 1);
            assert_eq!(count_allowed(&pool, "telegram", "bot").await.unwrap(), 1);
            revoke_account(&pool, None, "bot").await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_sender_racing_two_codes_keeps_the_second_usable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = file_pool(&dir).await;
        let a = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .unwrap();
        let b = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .unwrap();
        let (ra, rb) = tokio::join!(
            redeem(&pool, "telegram", "bot", "tg:1", &a.code, Utc::now(), false),
            redeem(&pool, "telegram", "bot", "tg:1", &b.code, Utc::now(), false),
        );
        let (ra, rb) = (ra.expect("redeem"), rb.expect("redeem"));
        assert_eq!(usize::from(ra.is_some()) + usize::from(rb.is_some()), 1);
        let unused = if ra.is_some() { &b } else { &a };
        let row = find_invite(&pool, unused.invite.id).await.unwrap().unwrap();
        assert_eq!(row.status(Utc::now()), InviteStatus::Active);
        assert!(
            redeem(
                &pool,
                "telegram",
                "bot",
                "tg:2",
                &unused.code,
                Utc::now(),
                false
            )
            .await
            .unwrap()
            .is_some()
        );
    }

    #[tokio::test]
    async fn revoke_account_clears_one_account() {
        let pool = connect_in_memory().await.expect("db");
        for (channel, account, sender) in [
            ("telegram", "bot", "tg:1"),
            ("telegram", "bot", "tg:2"),
            ("slack", "bot", "slack:1"),
            ("telegram", "other-bot", "tg:3"),
        ] {
            let created = create_invite(&pool, channel, account, DEFAULT_TTL)
                .await
                .unwrap();
            redeem(
                &pool,
                channel,
                account,
                sender,
                &created.code,
                Utc::now(),
                false,
            )
            .await
            .unwrap()
            .expect("valid");
        }
        assert_eq!(
            revoke_account(&pool, Some("telegram"), "bot")
                .await
                .unwrap(),
            2
        );
        assert_eq!(count_allowed(&pool, "slack", "bot").await.unwrap(), 1);
        assert_eq!(revoke_account(&pool, None, "bot").await.unwrap(), 1);
        assert_eq!(
            count_allowed(&pool, "telegram", "other-bot").await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn revoking_a_used_invite_is_a_conflict() {
        let pool = connect_in_memory().await.expect("db");
        let created = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("create");
        redeem(
            &pool,
            "telegram",
            "bot",
            "tg:1",
            &created.code,
            Utc::now(),
            false,
        )
        .await
        .unwrap()
        .expect("valid");
        assert!(matches!(
            revoke_invite(&pool, created.invite.id).await,
            Err(DbError::Conflict(_))
        ));
        assert!(matches!(
            revoke_invite(&pool, Uuid::now_v7()).await,
            Err(DbError::NotFound)
        ));
    }

    #[tokio::test]
    async fn revoke_sender_removes_access() {
        let pool = connect_in_memory().await.expect("db");
        let created = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("create");
        redeem(
            &pool,
            "telegram",
            "bot",
            "tg:1",
            &created.code,
            Utc::now(),
            false,
        )
        .await
        .unwrap()
        .expect("valid");
        assert_eq!(count_allowed(&pool, "telegram", "bot").await.unwrap(), 1);
        revoke_sender(&pool, "telegram", "bot", "tg:1")
            .await
            .expect("revoke");
        assert!(!is_allowed(&pool, "telegram", "bot", "tg:1").await.unwrap());
        assert!(matches!(
            revoke_sender(&pool, "telegram", "bot", "tg:1").await,
            Err(DbError::NotFound)
        ));
    }

    #[tokio::test]
    async fn list_invites_reports_status() {
        let pool = connect_in_memory().await.expect("db");
        let a = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .unwrap();
        let b = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .unwrap();
        redeem(&pool, "telegram", "bot", "tg:1", &a.code, Utc::now(), false)
            .await
            .unwrap()
            .expect("valid");
        let c = create_invite(&pool, "slack", "bot", DEFAULT_TTL)
            .await
            .unwrap();
        let listed = list_invites(&pool, Some("telegram"), None).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, b.invite.id);
        assert_eq!(listed[0].status(Utc::now()), InviteStatus::Active);
        assert_eq!(listed[1].status(Utc::now()), InviteStatus::Used);
        assert_eq!(list_invites(&pool, None, None).await.unwrap().len(), 3);
        let by_account = list_invites(&pool, Some("slack"), Some("bot"))
            .await
            .unwrap();
        assert_eq!(by_account.len(), 1);
        assert_eq!(by_account[0].id, c.invite.id);
        assert!(
            list_invites(&pool, None, Some("other-bot"))
                .await
                .unwrap()
                .is_empty()
        );
    }
}
