-- Active-scan discovery: tpd reconciles live_session against a periodic
-- tmux/iTerm2 sweep as well as hook registrations. `source` distinguishes a
-- row with a real session_id (from the runtime's hook) from one the scan
-- created with an inferred id (matched by cwd, or a synthetic placeholder).
ALTER TABLE live_session ADD COLUMN source TEXT NOT NULL DEFAULT 'hook';
