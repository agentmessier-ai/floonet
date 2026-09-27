-- A stable address for a conversation, distinct from the id of one transcript
-- segment.
--
-- Claude Code mints a new session id at every compaction, so a session id is
-- not a durable address: mail sent to a rotated id sits in a mailbox nothing
-- drains. Additive: `message`, `session` and turns are untouched, and a
-- session id remains the key of a transcript segment. What is added is a
-- second name that survives compaction, and a record of which segments
-- answered to it. Addressing resolves through it at the edge.

-- One correspondent. `pid` + `cwd` + `runtime_id` is how a rotation is
-- recognized, not what identifies the row — a compaction re-registers a new
-- session id from the same process, in the same directory, immediately.
CREATE TABLE conversation (
  id           TEXT PRIMARY KEY,   -- <machine>/<runtime>/conv-<uuid>
  machine_id   TEXT NOT NULL,
  runtime_id   TEXT NOT NULL,
  -- Host process of the most recent member. Updated on every join, so a
  -- conversation that outlives one pid (it cannot today, but a runtime that
  -- reconnects could) still points at where it currently lives.
  pid          INTEGER,
  cwd          TEXT,
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
) STRICT;

CREATE INDEX conversation_host ON conversation(runtime_id, pid, last_seen_at);

-- Which transcript segments have answered to this conversation.
--
-- Its own table rather than a column on `live_session` because `live_session`
-- rows are pruned and the membership must outlive them: draining a
-- conversation's inbox means reading the mailboxes of every id it ever had,
-- which is what rescues mail addressed before a rotation.
CREATE TABLE conversation_member (
  session_id      TEXT PRIMARY KEY,
  conversation_id TEXT NOT NULL REFERENCES conversation(id) ON DELETE CASCADE,
  joined_at       INTEGER NOT NULL
) STRICT;

CREATE INDEX conversation_member_conv ON conversation_member(conversation_id, joined_at);
