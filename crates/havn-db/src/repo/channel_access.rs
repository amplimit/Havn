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

/// Canonical form of something a user typed: dashes and whitespace
/// dropped, upper-cased, and the Crockford look-alikes (O to 0, I and L
/// to 1) folded. `None` unless the result is exactly one code.
#[must_use]
pub fn normalize_code(input: &str) -> Option<String> {
    let mut out = String::with_capacity(CODE_LEN);
    for c in input.chars() {
        if c == '-' || c.is_whitespace() {
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
    let code = generate_code()?;
    let id = Uuid::now_v7();
    let expires_at = Utc::now() + ttl;
    sqlx::query(
        "INSERT INTO channel_invites (id, code_hash, channel, account_id, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id.to_string())
    .bind(hash_code(&code))
    .bind(channel)
    .bind(account_id)
    .bind(timestamp(expires_at))
    .execute(pool)
    .await?;
    let invite = find_invite(pool, id).await?.ok_or(DbError::NotFound)?;
    Ok(CreatedInvite {
        invite,
        code: display_code(&code),
    })
}

const INVITE_COLUMNS: &str = "id, channel, account_id, created_at, expires_at, \
                              used_by_sender, used_at, revoked_at";

pub async fn find_invite(pool: &SqlitePool, id: Uuid) -> Result<Option<Invite>> {
    let row: Option<InviteRow> = sqlx::query_as(&format!(
        "SELECT {INVITE_COLUMNS} FROM channel_invites WHERE id = ?1"
    ))
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(Invite::try_from).transpose()
}

/// Every invite, newest first.
pub async fn list_invites(pool: &SqlitePool) -> Result<Vec<Invite>> {
    let rows: Vec<InviteRow> = sqlx::query_as(&format!(
        "SELECT {INVITE_COLUMNS} FROM channel_invites ORDER BY created_at DESC, id DESC"
    ))
    .fetch_all(pool)
    .await?;
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
/// and admit `sender_id`. `None` when the code is malformed, unknown,
/// for another account, used, revoked or expired; those cases are not
/// told apart so a caller cannot leak which one applied. The claim is a
/// single conditional UPDATE, so two senders racing for one code cannot
/// both win.
pub async fn redeem(
    pool: &SqlitePool,
    channel: &str,
    account_id: &str,
    sender_id: &str,
    code: &str,
    now: DateTime<Utc>,
) -> Result<Option<Invite>> {
    let Some(canonical) = normalize_code(code) else {
        return Ok(None);
    };
    let now_ts = timestamp(now);
    let mut tx = pool.begin().await?;
    let claimed: Option<(String,)> = sqlx::query_as(
        "UPDATE channel_invites SET used_by_sender = ?1, used_at = ?2 \
         WHERE code_hash = ?3 AND channel = ?4 AND account_id = ?5 \
           AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?2 \
         RETURNING id",
    )
    .bind(sender_id)
    .bind(&now_ts)
    .bind(hash_code(&canonical))
    .bind(channel)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id,)) = claimed else {
        return Ok(None);
    };
    sqlx::query(
        "INSERT INTO channel_allowlist (channel, account_id, sender_id, registered_at, invite_id) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (channel, account_id, sender_id) \
         DO UPDATE SET registered_at = excluded.registered_at, invite_id = excluded.invite_id",
    )
    .bind(channel)
    .bind(account_id)
    .bind(sender_id)
    .bind(&now_ts)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    find_invite(pool, parse_db_uuid(&id, "channel_invites.id")?).await
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
    use crate::connect_in_memory;

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
        let used = redeem(&pool, "telegram", "bot", "tg:1", &typed, Utc::now())
            .await
            .expect("redeem")
            .expect("valid code");
        assert_eq!(used.id, created.invite.id);
        assert_eq!(used.used_by_sender.as_deref(), Some("tg:1"));
        assert_eq!(used.status(Utc::now()), InviteStatus::Used);
        assert!(is_allowed(&pool, "telegram", "bot", "tg:1").await.unwrap());

        let again = redeem(&pool, "telegram", "bot", "tg:2", &created.code, Utc::now())
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
            redeem(&pool, "telegram", "bot", "tg:1", &created.code, later)
                .await
                .unwrap()
                .is_none()
        );
        for (channel, account) in [("telegram", "other-bot"), ("slack", "bot")] {
            assert!(
                redeem(&pool, channel, account, "tg:1", &created.code, Utc::now())
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
            redeem(&pool, "telegram", "bot", "tg:1", &created.code, Utc::now())
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

    #[tokio::test]
    async fn revoking_a_used_invite_is_a_conflict() {
        let pool = connect_in_memory().await.expect("db");
        let created = create_invite(&pool, "telegram", "bot", DEFAULT_TTL)
            .await
            .expect("create");
        redeem(&pool, "telegram", "bot", "tg:1", &created.code, Utc::now())
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
        redeem(&pool, "telegram", "bot", "tg:1", &created.code, Utc::now())
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
        redeem(&pool, "telegram", "bot", "tg:1", &a.code, Utc::now())
            .await
            .unwrap()
            .expect("valid");
        let listed = list_invites(&pool).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, b.invite.id);
        assert_eq!(listed[0].status(Utc::now()), InviteStatus::Active);
        assert_eq!(listed[1].status(Utc::now()), InviteStatus::Used);
    }
}
