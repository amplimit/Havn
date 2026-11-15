-- The chat a scheduled task was created in (issue #126): its results are
-- delivered to that chat and kept in its history.
ALTER TABLE cron_jobs ADD COLUMN chat TEXT;
