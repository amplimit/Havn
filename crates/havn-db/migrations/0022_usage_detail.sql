-- What a model call cost, in money (issue #235): the input tokens read
-- from and written to a provider's prompt cache, priced apart (both still
-- counted in `tokens_in`), and the model the call asked for, as
-- `[[llm.models]]` names it after the provider: `model` keeps the name the
-- provider answered with. Empty for calls made before.
ALTER TABLE credential_usages ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE credential_usages ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE credential_usages ADD COLUMN asked_model TEXT NOT NULL DEFAULT '';

-- Whether the provider's usage replaced the reservation: a call still
-- running, or whose provider reported none, stays at its reserved most.
-- Calls from before count as settled.
ALTER TABLE credential_usages ADD COLUMN settled INTEGER NOT NULL DEFAULT 1;

-- The usage page reads everyone's calls since a day.
CREATE INDEX idx_credusage_time ON credential_usages(created_at);
