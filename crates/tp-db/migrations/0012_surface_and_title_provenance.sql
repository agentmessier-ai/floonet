-- Surface, reasoning state, sidechains and title provenance. turn.uuid and
-- turn.parent_uuid are not here: 0005_provenance already added them.

-- ── 1. Supersession ─────────────────────────────────────────────────────────
--
-- Runtimes distinguish context that is still live from content that has been
-- compacted away (dsh `surfaceOp`, codex `replacement_history`, pi
-- `CompactionEntry`); without it a search hit cannot say whether the text it
-- matched is still part of the conversation.
--
-- Stored rather than derived at read time because the derivation needs the
-- whole session — a reverse scan to the newest compaction — which a windowed
-- query does not have. It is a cache of a computable fact, repairable by
-- re-ingest, which is why 'unknown' is the honest default rather than
-- 'current'.
--
--   'current'    still part of the model's context
--   'superseded' replaced by a compaction/rollback; kept for search, not context
--   'log_only'   never was context (dsh's third surface class)
--   'unknown'    ingested before this column existed, or by an adapter that
--                does not implement its runtime's fold. Not a synonym for
--                'current'.
ALTER TABLE turn ADD COLUMN surface TEXT NOT NULL DEFAULT 'unknown';

CREATE INDEX turn_surface_idx ON turn(session_id, surface);

-- ── 2. Reasoning that exists but cannot be read ─────────────────────────────
--
-- `thinking` is TEXT and an empty string cannot distinguish "this turn had no
-- reasoning" from "this turn reasoned and the payload is opaque" (codex
-- `encrypted_content`, pi `redacted: true`, Claude Code `redacted_thinking`).
--
--   'none'   no reasoning in the source
--   'text'   `thinking` holds readable text
--   'opaque' reasoning happened; the payload is encrypted/redacted and is not
--            in `thinking`
ALTER TABLE turn ADD COLUMN thinking_state TEXT NOT NULL DEFAULT 'none';

-- Backfill what is knowable: a non-empty `thinking` is readable text.
-- 'opaque' is not backfillable — nothing in the existing rows records that
-- reasoning was encrypted — so those turns stay 'none' until re-ingested,
-- which is what the old schema already claimed about them.
UPDATE turn SET thinking_state = 'text' WHERE thinking IS NOT NULL AND thinking != '';

-- ── 3. Sidechains ───────────────────────────────────────────────────────────
--
-- Claude Code marks `isSidechain` on transcript messages and writes subagent
-- transcripts to separate files, which are indexed as unrelated sessions.
-- This column claims the concept; joining the separate files to their parent
-- session is a distinct change.
ALTER TABLE turn ADD COLUMN sidechain INTEGER NOT NULL DEFAULT 0;

-- ── 4. Title provenance ─────────────────────────────────────────────────────
--
-- Every runtime has a native title, each in a different place. Storing a
-- derived title in the same column as a native one makes "this runtime has no
-- title" and "we did not look" indistinguishable, and lets a truncated first
-- message outrank a real title.
--
-- One column per source, resolved at read time, with the precedence codex
-- uses when importing Claude Code sessions:
--   COALESCE(title_user, title_ai, title_derived)
ALTER TABLE session ADD COLUMN title_user    TEXT;  -- /rename, /name, session_info
ALTER TABLE session ADD COLUMN title_ai      TEXT;  -- model-generated (Claude Code ai-title)
ALTER TABLE session ADD COLUMN title_derived TEXT;  -- teleport's fallback, marked as such

-- Everything in the existing `title` column was produced by teleport's own
-- derivation, so it moves wholesale into `title_derived`. `title` is left in
-- place, unread, rather than dropped: a rebuild repopulates the new columns
-- from source, and keeping the old value makes a bad rebuild recoverable.
UPDATE session SET title_derived = title WHERE title IS NOT NULL AND title != '';

-- ── Note on turn.seq ────────────────────────────────────────────────────────
--
-- `seq` is documented as "ordinal within session, gapless" and is populated
-- from file order. For dsh and codex that is conversation order. For pi and
-- Claude Code it is not: both are trees, and pi re-parents branch entries onto
-- older ids, so file order and conversation order differ on a branched session.
--
-- Renaming it (to `ingest_seq`) is the honest fix and is not done here: it is
-- referenced by UNIQUE(session_id, seq), every adapter, the resume checkpoint
-- and `tp turns` paging. Recorded so the next reader knows the column lies.
