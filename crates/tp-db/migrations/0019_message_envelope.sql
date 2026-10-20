-- An envelope that can carry something floonet did not invent.
--
-- `kind` (ask, note, reply) is the vocabulary for one agent addressing
-- another, and it stays. It cannot express an event from elsewhere — a build
-- that failed, a review submitted — and squeezing those into `note` throws the
-- type away at the one place it was still known. `kind TEXT NOT NULL` already
-- accepts any string and 0015 put no CHECK on it; opening the set is done in
-- code. What the schema was missing is the other two thirds of an envelope.
--
-- Modelled on CloudEvents:
--
--   id        -> message.id
--   source    -> from_session/machine
--   time      -> created_at
--   subject   -> to_session
--   type      -> kind                  opened in code, not here
--   datacontenttype ->                 added below
--   extensions ->                      added below
--
-- The headers are open from the start, as RFC 822's were, so a later content
-- type or attachment fits without a flag day.

-- What the body is, so a reader never has to guess. Every message until now
-- was prose for a model, and defaulting to that keeps every existing row true.
-- Without it the first non-text payload (a patch, a structured task) arrives
-- base64-encoded inside a field documented as prose.
ALTER TABLE message ADD COLUMN content_type TEXT NOT NULL DEFAULT 'text/plain';

-- Room for fields this version does not know about.
--
-- JSON rather than more columns, because the next sender is not in this
-- repository: a CI build id, a webhook delivery id, a priority — none should
-- cost a migration, and a schema that makes them cost one gets worked around
-- instead of extended.
--
-- NULL rather than '{}' for the common case: absent means "nobody attached
-- anything", which differs from "someone attached an empty object", and the
-- distinction is free here and unrecoverable later.
ALTER TABLE message ADD COLUMN extensions TEXT;
