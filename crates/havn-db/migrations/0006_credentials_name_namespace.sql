-- Migration 0006 — credentials gain a `name` handle and the v0.2
-- provider namespace (spec §7.3, §3).
--
-- Background. v0.1 `Credential` was keyed implicitly by (scope, scope_id,
-- provider, priority): multiple LLM credentials per provider in priority
-- order, picked by the resolver. v0.2 adds two new credential consumers:
-- channel adapters (one row per adapter `account_id`) and OAuth2 SaaS
-- packs (one row per SaaS account). Both want a stable `(provider, name)`
-- handle so config blocks can reference credentials by string —
-- `secret:channel:telegram:alice-tg-bot`, `secret:saas:microsoft-graph:
-- alice-m365`. The LLM resolver's priority chain still works without
-- names (multiple unnamed rows per provider, ordered by priority).

-- 1. Add the `name` column. NULL = "no handle, only addressable via the
--    fallback chain" (v0.1 pattern). NOT NULL = "addressable as a
--    specific (provider, name) pair" (v0.2 pattern).
ALTER TABLE credentials ADD COLUMN name TEXT;

-- 2. Auto-prefix bare LLM provider strings to the `llm:<provider>`
--    namespace. Operators don't need to do anything; existing rows just
--    start matching the v0.2 namespace convention.
--
--    Idempotent: rows already in the new namespace (provider LIKE
--    'llm:%' / 'channel:%' / 'saas:%') aren't touched, so re-running
--    this migration is a no-op. Operators with non-LLM provider strings
--    outside this short list (custom integrations) are also untouched —
--    their rows keep working under whatever provider value they had.
UPDATE credentials
SET provider = 'llm:' || provider
WHERE provider IN ('anthropic', 'openai', 'gemini', 'openrouter', 'azure-openai');

-- 3. Partial unique index on (scope, scope_id, provider, name) for
--    NAMED rows only. This enforces the v0.2 handle-uniqueness invariant
--    without breaking v0.1 priority-chain rows (which all have name IS
--    NULL and skip the index). New code that requires a handle must
--    pass a non-NULL name; the old fallback-chain code paths keep
--    working unchanged.
CREATE UNIQUE INDEX idx_credentials_handle
    ON credentials(scope, scope_id, provider, name)
    WHERE name IS NOT NULL;
