-- Migration 0008 — users gain an optional external identity (issue #49).
--
-- In trust_header mode the reverse proxy forwards whatever subject its
-- SSO produced: a username or an email (oauth2-proxy X-Forwarded-User,
-- Authelia Remote-User, Cloudflare Access email), not a havn UUID. The
-- gateway maps that value to the internal user id through this column.
--
-- Matching is exact and case-sensitive: `Alice@example.com` and
-- `alice@example.com` are different identities. Operators provision the
-- value exactly as their proxy emits it. NULL means the user can only be
-- addressed by internal id; the partial unique index keeps every
-- non-NULL value pointing at one user.
ALTER TABLE users ADD COLUMN external_id TEXT;

CREATE UNIQUE INDEX idx_users_external_id
    ON users(external_id)
    WHERE external_id IS NOT NULL;
