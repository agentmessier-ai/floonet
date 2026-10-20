/**
 * floonet ↔ DeepSeek Harness (dsh)
 *
 * A Cordis plugin, and now ONLY for reach: registration, presence, and
 * receiving a wake. The push half that fed floonet's index is gone — floonet
 * reads dsh's transcripts off disk like every other runtime it supports (see
 * install/runtimes.d/dsh.toml).
 *
 * What that cost, stated rather than left for someone to discover: dsh's
 * `sessionQuery.readSurface()` applies a SURFACE FOLD, deciding which events
 * are current, which are shadowed, and how positional replacements resolve.
 * Reading the raw event log from outside does not reproduce it, so floonet's
 * descriptor declares no compaction marker for dsh and every dsh turn is
 * `unknown` surface rather than `current`. A scan is allowed to be wrong about
 * supersession; it is never allowed to claim it is right.
 *
 * What it bought: floonet sees every dsh session on disk, including the ones
 * that existed before this plugin was installed. The push half could only ever
 * see sessions live while it ran — the same structural limit as an API proxy —
 * and it kept a second copy of a conversation dsh had already written down.
 *
 * THE THREE EXPORTS BELOW ARE THE PLUGIN. Cordis loads a module by its `apply`;
 * a file that exports only `name` is rejected outright, and dsh then runs with
 * no floonet at all — no registration, so `fl live` cannot see the session, and
 * `fl ask` to it answers "STORED but nothing will drain it". That happened, from
 * a delete that took `apply` and `inject` with the push code they sat next to.
 *
 * Install: add to `~/.dsh/profiles/<name>/cordis.patch.yml`
 *
 *     - insert:
 *         - id: floonet
 *           name: '@floonet/dsh'
 */

import { spawn } from 'node:child_process'
import { randomBytes, timingSafeEqual } from 'node:crypto'
import { homedir } from 'node:os'
import { join } from 'node:path'
import type { IncomingMessage, ServerResponse } from 'node:http'
import type { Context } from '@deepseek-ai/cordis'
import { defineTool } from '@deepseek-ai/dsh-tools'

export const name = 'floonet'

/**
 * Only what this plugin cannot work without.
 *
 * `tools` is required, not optional, and for a reason specific to dsh: its
 * default `workspace-write` sandbox allows writes to exactly
 * `[workspaceRoot, /tmp, os.tmpdir()]`, so a session shelling out to `fl reply`
 * gets "attempt to write a readonly database" — `~/.teleport` is outside all
 * three. Verified live. These tools execute in the HOST process, outside that
 * sandbox, which is what makes replying possible at all. `webServer` is deliberately NOT
 * here — a CLI-only composition has none, and requiring it would stop this
 * plugin loading at all there.
 *
 * But omitting it from `inject` also makes it unreadable: Cordis throws
 * "cannot get property X without inject" on the very access, so a
 * `typeof ctx.webServer` guard is not a guard, it is the crash. (Verified the
 * hard way — that error took down a running dsh.) The correct shape for an
 * OPTIONAL service is a nested `ctx.inject([...], cb)` scope, which runs only
 * when the service is actually mounted; dsh uses it for `typert` in
 * `core/agent/src/index.ts`.
 */
export const inject = ['agents', 'tools', 'sessionQuery']

const RUNTIME = 'dsh'
const TP = process.env.TP_BIN ?? join(homedir(), '.local', 'bin', 'fl')

/**
 * floonet's PRESENCE_TTL is 90s and a row is marked stale — not evicted — when
 * it lapses. Beating at a third of that means two consecutive failures still
 * leave the session healthy.
 */
const HEARTBEAT_MS = 30_000

/**
 * Run `fl` and return stdout+stderr merged.
 *
 * Merging is not cosmetic. `fl` prints `[coverage]` and `[warn]` lines to
 * stderr — including "this scan was truncated" — and pi's extension originally
 * returned stdout only, which reported a partial scan to the model as a
 * complete one. That is the exact false negative floonet's coverage contract
 * exists to prevent, reintroduced at the integration layer.
 */
function fl(args: string[], signal?: AbortSignal): Promise<string> {
  return new Promise((resolve) => {
    const child = spawn(TP, args, { signal })
    let out = ''
    child.stdout?.on('data', (d) => (out += d))
    child.stderr?.on('data', (d) => (out += d))
    // Never reject: floonet being absent, unreadable, or slow must degrade to
    // "no floonet" and never break the dsh session hosting us.
    child.on('error', (e) => resolve(`floonet unavailable: ${e.message}`))
    child.on('close', () => resolve(out.trim()))
  })
}

/** Fire-and-forget: a failed register must not break session startup. */
function tpDetached(args: string[]): void {
  void fl(args)
}

export function apply(ctx: Context): void {
  // ── Presence ───────────────────────────────────────────────────────────────
  // `declared`, because dsh cannot be found by floonet's process scan: the web
  // profile's sessions live in a browser with no tty, and one host process
  // serves many of them. Without this the scan would prune these rows within
  // one interval — which is exactly what happened to pi before floonet learned
  // that the scan may only prune what it can see.
  // Resolved when the wake route is actually mounted; until then registration
  // declares no channel and floonet parks messages in the mailbox — correct,
  // and better than declaring a channel that would 404.
  let deliver: string | undefined = process.env.TELEPORT_DSH_WAKE
    ? `exec:${process.env.TELEPORT_DSH_WAKE}`
    : undefined

  ctx.on('session/created', (session) => {
    tpDetached([
      'register',
      '--session-id', session.id,
      '--runtime', RUNTIME,
      '--cwd', session.header.cwd ?? process.cwd(),
      '--presence', 'declared',
      // Our OWN pid, not the one floonet would infer. Its fallback walks up
      // from the spawned `fl` to the nearest registered ancestor, which finds
      // whatever launched dsh — observed live: a host started from a Claude
      // Code session got every dsh session registered under the CLAUDE pid,
      // which then made that session's own `fl ask` look like it came from dsh.
      '--pid', String(process.pid),
      ...(deliver ? ['--deliver', deliver] : []),
    ])
  })

  ctx.on('session/disposed', (session) => {
    tpDetached(['unregister', '--session-id', session.id, '--runtime', RUNTIME])
  })

  // Renew every live session. floonet marks a lapsed row stale rather than
  // deleting it, so a missed beat costs a wake, not the registration.
  const timer = setInterval(() => {
    for (const agent of ctx.agents.list()) {
      tpDetached(['heartbeat', '--session-id', agent.id, '--runtime', RUNTIME])
    }
  }, HEARTBEAT_MS)
  ctx.on('dispose', () => clearInterval(timer))

  // ── Tools ──────────────────────────────────────────────────────────────────
  // `floo_*` as native dsh tools, so the model does not shell out to `fl`.
  registerTools(ctx)

  // ── Delivery ───────────────────────────────────────────────────────────────
  // floonet POSTs a fixed control string here; the body never carries message
  // content. We drain our own inbox and put the result in front of
  // the model — the one step that cannot live in floonet, because only code
  // inside dsh can do it.
  //
  // `agent.send(msg, 'next-turn', true)` rather than a slash command: dsh
  // commands execute "without sending anything to the model", so a command
  // handler that only returned text would be silently half-wired. dsh's Agent
  // has a native inbox with a wakeup flag, which is `fl ask`'s semantics
  // expressed natively.
  // A secret, minted per plugin load, that floonet hands back on every wake.
  //
  // The carrier this route registers on has no middleware seat: dsh's own
  // Connection plugin writes its 403 as the first statement of its own
  // handler, and a route whose handler does not is simply unguarded. Loopback
  // is not the guard — any page the browser loads can POST to 127.0.0.1, and a
  // JSON body sent as text/plain is not preflighted.
  //
  // A shared secret rather than Host/Origin checks, because the caller here is
  // a local process and not a browser: it can hold a secret, and one
  // comparison closes both the cross-origin POST and a guess at a session id.
  // Never persisted — a restart re-registers every session with a fresh token
  // (the repair loop below), so a token cannot outlive the route it names.
  const wakeToken = randomBytes(32).toString('base64url')

  ctx.inject(['webServer'], (web: Context) => {
    deliver = `http://127.0.0.1:${web.webServer.port}/floonet/wake?token=${wakeToken}`
    const dispose = web.webServer.register({
      kind: 'exact',
      path: '/floonet/wake',
      handler: async (req: IncomingMessage, res: ServerResponse) => {
        try {
          // First, before the body is read: an unauthenticated caller must not
          // reach the drain, and must not learn whether a session id exists.
          if (!tokenMatches(req, wakeToken)) {
            res.statusCode = 403
            return res.end('forbidden')
          }
          const sessionId = await readSessionId(req)
          if (!sessionId) {
            res.statusCode = 400
            return res.end('missing session_id')
          }
          // floonet addresses sessions by its composite id
          // (`<machine>/<runtime>/<native>`); the key in `ctx.agents` is the
          // native segment alone.
          const nativeId = sessionId.split('/').pop() ?? sessionId
          const agent = ctx.agents.get(nativeId as never)
          if (!agent) {
            // Not an error: floonet may still hold a registration for a
            // session this host has since disposed. Answer 200 so the wake is
            // not retried against a session that no longer exists here.
            res.statusCode = 200
            return res.end('no such live session')
          }
          // `fl inbox` accepts either form, but pass the native id for symmetry
          // with registration, which also passes native and lets `fl` compose.
          //
          // The pending pass comes first and is not optional: a batch a
          // previous wake showed but never finished acking is invisible to a
          // plain drain, so this view is the only thing that ever recovers it.
          // Running it here rather than asking the model to means recovery does
          // not depend on the model electing to look.
          const pending = await fl(['inbox', '--pending', '--session-id', nativeId, '--runtime', RUNTIME])
          const drained = await fl(['inbox', '--session-id', nativeId, '--runtime', RUNTIME])
          const has = (s: string): boolean => Boolean(s) && !s.startsWith('inbox empty')
          if (has(pending) || has(drained)) {
            const body = [
              has(pending) ? `[unfinished from an earlier wake — finish these first]\n${pending}` : '',
              has(drained) ? drained : '',
            ].filter(Boolean).join('\n\n')
            agent.send(userMessage(`${FRAMING}\n\n${body}`), 'next-turn', true)
          }
          res.statusCode = 200
          res.end('ok')
        } catch (e) {
          res.statusCode = 500
          res.end(String(e))
        }
      },
    })
    web.on('dispose', dispose)

    // Repair the rows registered before this scope ran. Cordis mounts services
    // in whatever order the composition resolves, so a session created before
    // the web server came up registered with NO channel — and nothing else
    // ever wrote one afterwards: `heartbeat` touches last_seen_at only (the
    // Lease split, by design), and the `deliver` closure above only helps
    // sessions created LATER. Such a session was silently unreachable for its
    // whole life while every younger sibling worked.
    //
    // AFTER the route is live, deliberately: a repaired registration can wake
    // immediately (parked mail drains on the next attempt), and a channel that
    // 404s until the next tick is the thing registration waited to avoid.
    //
    // `--cwd` is not optional here even though the session already declared
    // it: the upsert overwrites every column it names (`cwd = excluded.cwd`),
    // so omitting it would erase what session/created wrote.
    for (const agent of ctx.agents.list()) {
      tpDetached([
        'register',
        '--session-id', agent.id,
        '--runtime', RUNTIME,
        '--cwd', agent.session?.header?.cwd ?? process.cwd(),
        '--presence', 'declared',
        '--pid', String(process.pid),
        '--deliver', deliver,
      ])
    }
  })
}





/** dsh content blocks → floonet's (text, thinking, tool_calls) triple. */
function collect(content: unknown): { text: string; thinking: string; tools: { name: string; input_digest: string | null }[] } {
  const texts: string[] = []
  const thinks: string[] = []
  const tools: { name: string; input_digest: string | null }[] = []
  if (typeof content === 'string') texts.push(content)
  else if (Array.isArray(content)) {
    for (const b of content as Record<string, unknown>[]) {
      if (b?.type === 'text' && typeof b.text === 'string') texts.push(b.text)
      // dsh calls it `reasoning`; floonet's field is `thinking`.
      else if (b?.type === 'reasoning' && typeof b.text === 'string') thinks.push(b.text)
      else if (b?.type === 'tool-call') {
        tools.push({
          name: String(b.name ?? '?'),
          // A DIGEST, never the raw input: tool payloads are large and
          // frequently carry secrets.
          input_digest: typeof b.arguments === 'string' ? b.arguments.slice(0, 80) : null,
        })
      }
    }
  }
  return { text: texts.join(''), thinking: thinks.join(''), tools }
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)
const num = (v: unknown): number | null => (typeof v === 'number' ? v : null)


/**
 * Set from inside the `sessionTitle` inject scope, so a composition without the
 */


/**
 * The outbound half: floonet as tools the model can call.
 *
 * Every one shells out to `fl`, so the logic stays in one shared binary and
 * this file remains a translation layer. Names and parameters match the MCP
 * tool set exactly — `crates/tp/tests/pi_tool_parity.rs` guards that for pi and
 * should be extended to cover dsh, because the same drift has happened twice.
 */
function registerTools(ctx: Context): void {
  const text = (value: unknown) => [{ type: 'text', text: String(value) }]
  // `required` must be `true` or ABSENT — dsh's schema compiler rejects
  // `required: false` outright ("must be true when present"), which is stricter
  // than JSON Schema and caught at plugin load rather than at call time.
  const str = (description: string, required = false) =>
    (required ? { type: 'string', required: true, description } : { type: 'string', description })

  ctx.tools.register(defineTool({
    name: 'floo_search',
    description:
      'Find WHERE something was discussed across agent sessions on this machine — Claude Code, pi, and dsh alike — and optionally on trusted peer machines. ' +
      'Returns coordinates and excerpts, not whole conversations; feed a hit into floo_turns when the excerpt is not enough. ' +
      'ALWAYS pass the [coverage] line back to the user when it reports a truncated or degraded scan: a partial scan is not proof that something was never discussed.',
    parameters: {
      query: str('Text to search for', true),
      folder: str('Restrict to sessions whose path matches this folder'),
      since: str('Start of the window: a duration ago (6h, 3d, 2w — default 6h) or an absolute local date (2026-08-04)'),
      until: str('End of the window, EXCLUSIVE — same spellings. Pair with an absolute `since` to ask about ONE day.'),
      include_thinking: { type: 'boolean', description: "Also search the model's extended reasoning (off by default)" },
      regex: { type: 'boolean', description: 'Treat `query` as a regular expression' },
      limit: { type: 'number', description: 'Max matches (default 20)' },
      all: { type: 'boolean', description: 'Fan out to EVERY trusted peer and merge results. Each peer answers by scanning its whole corpus, so this asks N machines to do real work at once — it is refused above a handful of peers, and `peers` is the way to search at any scale. Peers that fail to answer are always listed — pass that through, do not drop it.' },
      peers: { type: 'array', items: { type: 'string' }, description: 'Query only these peers, by id prefix or name (floo_peers lists them). Prefer this over `all` when you know where to look; it works at any number of paired machines.' },
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return fl([
        'search', String(a.query),
        ...optStr('--folder', a.folder), ...optStr('--since', a.since), ...optStr('--until', a.until),
        ...(a.include_thinking ? ['--include-thinking'] : []),
        ...(a.regex ? ['--regex'] : []),
        ...optStr('--limit', a.limit),
        ...(a.all ? ['--all'] : []),
        // Repeatable, one flag per peer: naming peers bounds the fan-out by
        // intent rather than by how many machines happen to be paired.
        ...(Array.isArray(a.peers) ? a.peers.flatMap((p) => ['--peer', String(p)]) : []),
      ])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_sessions',
    description: 'List agent sessions active in a time window, most recent first — use it to pick one before floo_turns.',
    parameters: {
      folder: str('Restrict to sessions whose path matches this folder'),
      since: str('Start of the window (default 7d), a duration or an absolute local date'),
      until: str('End of the window, EXCLUSIVE'),
      limit: { type: 'number', description: 'Max sessions (default 20)' },
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return fl(['sessions', ...optStr('--folder', a.folder), ...optStr('--since', a.since),
        ...optStr('--until', a.until), ...optStr('--limit', a.limit)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_turns',
    description:
      'Read the actual conversation of a session. This is the EXPENSIVE tool — it returns real transcript, orders of magnitude more context than floo_search. ' +
      'To answer "what happened recently" or "what happened on that day", OMIT session_id and pass `since` with `folder`: finding the id IS the question. ' +
      'Never use it to poll another session for progress — it cannot tell "still working" from "done"; wait for that session to reply.',
    parameters: {
      session_id: str('Composite session id from floo_search / floo_sessions. Omit it and pass `since` to read the most recent session instead.'),
      since: str('Start of a TIME WINDOW: a duration ago (4h, 2d) or an absolute local time (2026-08-04). Keeps the NEWEST turns if it overflows.'),
      until: str('End of the window, EXCLUSIVE. Pair with an absolute `since` to read ONE day.'),
      folder: str('With `since` and no `session_id`: which folder\'s most recent session to read'),
      include_thinking: { type: 'boolean', description: "Also return the model's extended reasoning" },
      limit: { type: 'number', description: 'Max turns (default 200)' },
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      if (!a.session_id && !a.since) return 'floo_turns needs either session_id, or since (e.g. "4h") to read the most recent session.'
      return fl(['turns', ...(a.session_id ? [String(a.session_id)] : []),
        ...optStr('--since', a.since), ...optStr('--until', a.until), ...optStr('--folder', a.folder),
        ...(a.include_thinking ? ['--include-thinking'] : []), ...optStr('--limit', a.limit)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_live',
    description: 'List agent sessions running RIGHT NOW on this machine, with their working directory. Use it to find a target before floo_ask.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute() {
      return fl(['live'])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_ask',
    description:
      'Send a message to another live agent session and wake it. THIS HAS A REAL SIDE EFFECT: it interrupts another session, which may be driven by someone else. ' +
      'Use it for questions, notifications, and to delegate real work — the receiving agent treats the message as a task, does it, and replies with what it did. ' +
      'IT DOES NOT WAIT. It returns as soon as the message is queued; the answer arrives later as a wake. Do NOT sit in a sleep loop polling for it.',
    parameters: {
      session_id: str('Composite session id of the target, from floo_live', true),
      message: str('What to say', true),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args, exec) {
      const a = args as Record<string, unknown>
      return fl(['ask', String(a.session_id), String(a.message), ...identity(exec)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_note',
    description:
      'Tell another live session something WITHOUT asking it for anything. Same delivery as floo_ask — it wakes the target, because a status update ' +
      'nobody sees for hours is not much of an update — but the message is marked so the receiver is told plainly that no reply is expected. ' +
      "Use it for 'I pushed the fix', 'your build is green', 'heads up, I changed X'. Use floo_ask only when you need something back: " +
      'a message that reads as a request costs the other agent a turn to answer.',
    parameters: {
      session_id: str('Composite session id of the target, from floo_live', true),
      message: str('What to tell them', true),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args, exec) {
      const a = args as Record<string, unknown>
      return fl(['note', String(a.session_id), String(a.message), ...identity(exec)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_peers',
    description:
      'List the other machines this one has paired with, and their trust state. Use it before searching another machine: floo_search with `peers` ' +
      'set queries only the ones you name, while `all` fans out to every trusted peer and each answers by scanning its whole corpus.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute() {
      return fl(['peers'])
    },
  }))

  // ── The four that were missing ──────────────────────────────────────────
  //
  // Not a dsh limitation — `ctx.tools.register` takes these exactly as it
  // takes the ones above. They were simply never written, and the gap was
  // found by an agent comparing the three integrations against each other
  // rather than by anything in this repository.
  //
  // Pairing APPROVE and REVOKE are deliberately absent and stay absent, here
  // and everywhere: granting trust is the one decision in floonet that must be
  // made by a person at a keyboard. A session running with permissions skipped,
  // or one acting on an injected instruction, could otherwise make a remote
  // machine permanently able to read this one. Requesting and rejecting grant
  // nothing, so those are safe to expose.

  ctx.tools.register(defineTool({
    name: 'floo_discover',
    description:
      'Probe a host to see whether it runs floonet. READ-ONLY: it grants nothing, trusts nobody, and changes no state on either ' +
      'side. Use it before floo_pair_request to check there is something there to pair with.',
    parameters: {
      host: str('`host` or `host:port` — the port is probed across the default range if omitted', true),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return fl(['discover', String(a.host)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_pair_request',
    description:
      'Introduce this machine to a peer. Records it as PENDING on both sides and trusts nobody — trust needs a person to compare ' +
      'the device id out of band and run `fl pair approve` on each machine. That approval is deliberately not a tool.',
    parameters: { addr: str('`host:port` of the peer', true) },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return fl(['pair', 'request', String(a.addr)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_pair_list',
    description:
      'Peers that are pending or trusted, with the fingerprint to compare out of band. Use it to read back what floo_pair_request ' +
      'recorded, and to get the device id a person needs in order to approve.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute() {
      return fl(['pair', 'list'])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_pair_reject',
    description:
      'Refuse a peer that has NOT been trusted yet, removing it. Safe as a tool because refusing grants nothing. For a peer that IS ' +
      'already trusted use `fl pair revoke` — same effect, different mistake, and a person has to do it.',
    parameters: { device_id: str("The peer's device id, from floo_pair_list", true) },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return fl(['pair', 'reject', String(a.device_id)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_reply',
    description:
      'Answer a message from your inbox, using the `id:` shown for it. ALWAYS prefer this over floo_ask when responding: the address comes from the original ' +
      'message so it cannot be misrouted, and a reply is the ONLY signal the sender ever gets — including when you have finished the work, not just when asked a question.',
    parameters: {
      message_id: str('The `id:` printed for that message', true),
      message: str('Your answer — report what you actually did, including anything you chose not to do', true),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args, exec) {
      const a = args as Record<string, unknown>
      return fl([
        'reply', String(a.message_id), String(a.message),
        // Identify ourselves explicitly. Without this `fl` infers the sender by
        // walking up from the spawned process, which lands on whatever launched
        // dsh — so a reply was stamped as coming from THAT session instead.
        // Contract R-capable: "a runtime that knows its own session id should
        // pass --from-session; never rely on inference."
        ...identity(exec),
      ])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_inbox',
    description:
      'Drain THIS session\'s own mailbox. Normally floonet wakes you automatically; call this if the user asks whether anyone messaged you. ' +
      'Draining marks a message READ, not ACKED — read means shown, ack means you confirm you finished acting on it (floo_ack). ' +
      'Set pending to see messages that were shown but never acked: the recovery view if a previous drain got interrupted before you finished ' +
      'acting on everything in it. Read-only — it does not drain or mark anything.',
    parameters: {
      session_id: str('This session\'s native id'),
      pending: { type: 'boolean', description: 'Show delivered-but-unacked messages instead of draining new ones. Read-only.' },
      history_since: str('Show ACKED messages from this window instead of draining new ones — a duration ("4h", "2d") or an absolute local time. Read-only.'),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return fl([
        'inbox', ...optStr('--session-id', a.session_id), '--runtime', RUNTIME,
        ...(a.pending ? ['--pending'] : []),
        ...(a.history_since ? ['--history', '--since', String(a.history_since)] : []),
      ])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'floo_ack',
    description:
      'Confirm you finished acting on a message from your inbox — NOT the same as floo_inbox showing it to you. Call this only after you ' +
      'have actually done what the message asked (or decided a note needs no action). An unacked message stays visible forever via ' +
      'floo_inbox with pending set, so if you get interrupted mid-batch, the next drain can recover exactly what you left undone.',
    parameters: { message_id: str('The message_id from your inbox (short prefix is fine)', true) },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return fl(['ack', String(a.message_id)])
    },
  }))
}

/**
 * `--from-session <native> --runtime dsh`, from the execution context.
 *
 * The tool runs inside the session that called it, so its id is known here and
 * must be stated: floonet's fallback is to infer the sender from the process
 * tree, which for a dsh host launched from another agent session resolves to
 * THAT session. Observed live — replies were stamped as coming from the Claude
 * Code session that started dsh.
 */
function identity(exec: unknown): string[] {
  // `ToolExecutionInput.agent?.id` — the Agent whose turn invoked this tool.
  // Optional in the type, so absence degrades to floonet's inference rather
  // than to a broken call.
  const sid = (exec as { agent?: { id?: string } })?.agent?.id
  return sid ? ['--from-session', String(sid), '--runtime', RUNTIME] : []
}

/** `--flag value` when present, nothing when not. */
function optStr(flag: string, v: unknown): string[] {
  return v === undefined || v === null || v === '' ? [] : [flag, String(v)]
}

/**
 * Whether the request carries the channel token this plugin minted.
 *
 * Constant-time, and length-checked first because `timingSafeEqual` throws on
 * unequal lengths rather than reporting a mismatch.
 */
function tokenMatches(req: IncomingMessage, expected: string): boolean {
  const got = new URL(req.url ?? '/', 'http://127.0.0.1').searchParams.get('token')
  if (got === null) return false
  const a = Buffer.from(got)
  const b = Buffer.from(expected)
  return a.length === b.length && timingSafeEqual(a, b)
}

async function readSessionId(req: IncomingMessage): Promise<string | undefined> {
  const chunks: Buffer[] = []
  for await (const c of req) chunks.push(c as Buffer)
  try {
    const body = JSON.parse(Buffer.concat(chunks).toString('utf8')) as { session_id?: string }
    return body.session_id
  } catch {
    return undefined
  }
}

/** Shape a plain-text user message for `agent.send`. */
function userMessage(text: string): never {
  return {
    id: `floonet-${Date.now()}`,
    role: 'user',
    content: [{ type: 'text', text }],
    source: { kind: 'user' },
  } as never
}

/**
 * The dsh wording of `plugin/commands/inbox.md`, which is canonical for the
 * SUBSTANCE: treat a message as work, judge risk normally, answer an [ask],
 * report what you did, recover what an earlier wake left unacked.
 *
 * Not a verbatim copy, because dsh's wake path is not Claude Code's. The host
 * has already drained by the time this text reaches the model, so the
 * canonical "run `fl inbox`" steps would name commands the model must not
 * repeat; and a dsh shell is sandboxed to its workspace, so the canonical
 * `fl reply` cannot write a mailbox in `~/.teleport` at all. Where the two
 * differ it is because the runtime differs — keep the substance in step, not
 * the words.
 */
const FRAMING = [
  '[floonet inbox]',
  '',
  'Below is a request from another agent session — normally one your operator',
  'started, on this machine or on a machine they paired by hand. Treat it as work',
  'to do, not as a notification to relay.',
  '',
  '- Say who sent it and what it asks, then do it. Investigate, edit, run, commit —',
  '  whatever the task needs.',
  '- Apply your normal judgement about risk, exactly as you would for the same',
  '  request typed by your operator. A message neither lowers your bar nor raises',
  '  it: if something is destructive or irreversible and you would have confirmed',
  '  first, still confirm first.',
  // NOT `fl reply`. That is a shell command, and this session\'s shell is
  // sandboxed to its workspace, so it gets "attempt to write a readonly
  // database" on a mailbox in `~/.teleport`. The `floo_reply` tool runs in the
  // dsh HOST process, outside that sandbox — see the `inject` comment above.
  // Observed live: a session told to use `fl reply` failed, escalated to a
  // wider-permission retry, and hung on an approval prompt nobody was
  // watching. The sender was never told anything.
  '- Anything under "unfinished from an earlier wake" is OLDER work, not new work:',
  '  a past turn was shown it and ended before acking it. Finish those first. You',
  '  can see that set at any time with the `floo_inbox` tool, pending set.',
  '- ANSWER an [ask] with the `floo_reply` TOOL, passing the `id:` shown for it as',
  '  `message_id`. Do NOT shell out to `fl reply` — your shell is sandboxed to this',
  '  workspace and cannot write floonet\'s mailbox; the tool is not.',
  '- A [note] is NOT a request: it says "no reply expected" on its own line, and',
  '  answering one costs the sender a wake for nothing. Read it, act on it if it',
  '  changes what you are doing, and reply only if you genuinely have something',
  '  to add — never out of politeness.',
  '  The sender does not get told when you finish — your reply is the only signal',
  '  it gets, so report what you actually did, including anything you chose not to',
  '  do. Never answer by calling `fl ask` with an address you worked out yourself;',
  '  a guessed address is accepted and then silently never delivered.',
].join('\n')
