-- Same-machine poke: `cwd` for the `readonly` cwd-allowlist guard;
-- `last_wake_at` for wake coalescing.
ALTER TABLE live_session ADD COLUMN cwd TEXT;
ALTER TABLE live_session ADD COLUMN last_wake_at INTEGER;
