#!/usr/bin/env node
// fleet-watch (bundled with stonewave, copy 1): tells a running coding-agent session what was addressed to its agent in Wave.
//
// It asks the MCP tool `inbox` every 20 seconds and prints ONE line for each new thing: a mention or an address in a room the agent reads, a task assigned to
// it, a task it created that got stuck. A session that watches this program as a Monitor wakes up on each line. No dependencies, Node 18 or newer.
//
//   WAVE_MCP_URL=https://<wave-api host>/mcp  WAVE_AGENT_KEY=wak_...  node fleet-watch.mjs [--once] [--interval 20] [--session name]
//
// What the lines are: information from Wave, not orders from a person. Every line starts with "fleet:". Text of outside origin is never printed,
// only a marker. The key is read from the environment (or a key file), sent only as the Authorization header, and never printed (also not in errors).
// What it remembers (next number, tasks already told) goes into a small file, so a restart prints nothing twice.

import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { pathToFileURL } from 'node:url';

export const PREFIX = 'fleet:';
const PAGE = 50, TEXT_MAX = 120, TITLE_MAX = 100, ROOMS_REFRESH_MAX = 1;
const OUTSIDE_MARK = '[outside text, not shown; read it with the entry tool]';

// Control, format (direction marks, zero-width, the tag block U+E0000-E007F that can hide text, U+061C, U+2060-2064 ...), private use, unassigned, surrogate
// and line/paragraph separators. All of them become a space, so a text can neither hide words nor add a line of its own.
const INVISIBLE = /[\p{Cc}\p{Cf}\p{Co}\p{Cn}\p{Cs}\p{Zl}\p{Zp}]/gu;

/** One printable line: invisible characters out, white space folded, cut at `max` code points (not UTF-16 units). */
export function oneLine(x, max = TEXT_MAX) {
  const cps = [...String(x ?? '').replace(INVISIBLE, ' ').replace(/\s+/g, ' ').trim()];
  return cps.length > max ? cps.slice(0, max - 1).join('').trimEnd() + '…' : cps.join('');
}

/** The id part of a key `wak_<agent id>_<secret>`: safe to put in a file name. Never the secret. */
export function agentIdOf(key) {
  const m = /^wak_([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})_/i.exec(key ?? '');
  return m ? m[1].toLowerCase() : null;
}

export function defaultStatePath(url, key) {
  const id = agentIdOf(key) ?? 'agent';
  return join(homedir(), '.fleet-watch', `${createHash('sha256').update(`${url}|${id}`).digest('hex').slice(0, 16)}.json`);
}

const freshState = () => ({ next_seq: 0, tasks: new Set(), stuck: new Set(), fresh: true, warn: null });

/** No file is a normal first start. A file that is there but unreadable or broken is NOT silent: it is kept as `<file>.corrupt` and the user is told. */
function loadState(path) {
  let raw;
  try { raw = readFileSync(path, 'utf8'); }
  catch (e) { return e?.code === 'ENOENT' ? freshState() : { ...freshState(), warn: `the state file cannot be read (${e?.code ?? 'error'}); starting from now, older items are skipped` }; }
  try {
    const s = JSON.parse(raw);
    if (Number.isSafeInteger(s.next_seq) && s.next_seq >= 0) return { next_seq: s.next_seq, tasks: new Set(Array.isArray(s.tasks) ? s.tasks : []), stuck: new Set(Array.isArray(s.stuck) ? s.stuck : []), fresh: false, warn: null };
  } catch { /* broken: below */ }
  let kept = '';
  try { renameSync(path, `${path}.corrupt`); kept = `, kept as ${path}.corrupt`; } catch { /* leave it */ }
  return { ...freshState(), warn: `the state file is broken${kept}; starting from now, older items are skipped (--from-start shows them)` };
}

function saveState(path, st) {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify({ next_seq: st.next_seq, tasks: [...st.tasks], stuck: [...st.stuck] }), { mode: 0o600 });
  renameSync(tmp, path);
}

/** Seconds (or an HTTP date) of a Retry-After header as milliseconds, at most an hour; null when there is none. */
function retryAfterMs(res) {
  const v = res.headers?.get?.('retry-after');
  if (!v) return null;
  const secs = /^\d+$/.test(v.trim()) ? Number(v) : (Date.parse(v) - Date.now()) / 1000;
  return Number.isFinite(secs) && secs > 0 ? Math.min(secs, 3600) * 1000 : null;
}

/** A call to the MCP tool. Errors carry a short reason (and `code` for a refusal by the tool), never the key and never the answer's text. */
async function callTool(o, name, args) {
  let res;
  try {
    res = await o.fetch(o.url, {
      method: 'POST', headers: { 'content-type': 'application/json', accept: 'application/json', authorization: `Bearer ${o.key}` },
      body: JSON.stringify({ jsonrpc: '2.0', id: ++o.rpc, method: 'tools/call', params: { name, arguments: { ...args, session: o.session } } }), signal: AbortSignal.timeout(o.timeoutMs ?? 15_000),
      redirect: 'error',                       // the Authorization header must never follow a redirect to another address
    });
  } catch (e) { throw new Error(e?.name === 'TimeoutError' ? 'no answer in time' : /redirect/i.test(String(e?.cause?.message ?? e?.message)) ? 'Wave answered with a redirect (not followed)' : 'cannot reach Wave'); }
  const fail = (msg, extra = {}) => Object.assign(new Error(msg), extra);
  if (res.status === 401) throw fail('key refused (401)');
  if (res.status === 403) throw fail('key not allowed (403)');
  if (res.status === 423) throw fail('agent or company paused (423)');
  if (res.status === 429) throw fail('too many calls (429)', { retryAfterMs: retryAfterMs(res) });
  if (res.status === 503) throw fail('Wave is busy (503)', { retryAfterMs: retryAfterMs(res) });
  if (res.status !== 200) throw fail(`Wave answered ${res.status}`);
  let j;
  try { j = await res.json(); } catch { throw fail('answer is not JSON'); }
  if (j?.error) throw fail(`protocol error ${Number(j.error.code) || ''}`.trim());
  const texts = (j?.result?.content ?? []).map((c) => c?.text).filter((t) => typeof t === 'string');
  let data;
  try { data = JSON.parse(texts[texts.length - 1]); } catch { throw fail('answer is not JSON'); }
  if (j.result.isError) { const code = oneLine(data?.error?.code ?? 'error', 40); throw fail(`refused: ${code}`, { code }); }
  return data;
}

async function roomNames(o) {
  const data = await callTool(o, 'rooms', {});
  for (const r of data?.items ?? []) if (r?.id) o.rooms.set(r.id, oneLine(r.name ?? r.key ?? r.id, 40));
}
const roomLabel = (o, id) => o.rooms.get(id) ?? String(id ?? '?').slice(0, 8);

function itemLine(o, it) {
  const a = it.author ?? {};
  const who = a.kind === 'agent' ? `${oneLine(a.name ?? 'agent', 40)} (${a.engine === 'external' ? 'external agent' : 'Wave agent'})` : a.kind === 'member' ? `${oneLine(a.name ?? 'person', 40)} (person)` : oneLine(a.kind ?? '?', 20);
  // text of outside origin (mail, pasted text) is never shown: the session reads it with `entry` if it must, and finds it wrapped there
  const text = it.untrusted === true ? OUTSIDE_MARK : oneLine([it.title, it.body].filter(Boolean).join(' - '));
  return `${PREFIX} ${roomLabel(o, it.room_id)} | ${who} | seq ${it.seq} | entry ${it.id} | ${text}`;
}
// a task of outside origin shows no title either: only the marker
const taskLine = (kind, t) => `${PREFIX} ${kind} | ${t.untrusted === true ? OUTSIDE_MARK : oneLine(t.title, TITLE_MAX)} | task ${t.id} | due ${String(t.due_at ?? '?').slice(0, 16).replace('T', ' ')}`;

/**
 * The first entry after `since` is too large to send. Which number is it? Ask with limit 1 from further and further ahead until the call works, then halve
 * the distance: the largest number from which the call still fails is the one before it. Costs about 2 log2(distance) calls.
 */
async function findTooLarge(o, since) {
  const fails = async (s) => {
    try { await callTool(o, 'inbox', { since_seq: s, limit: 1 }); return false; }
    catch (e) { if (e?.code === 'result_too_large') return true; throw e; }
  };
  let lo = since, step = 1, hi = null;
  while (hi === null && step < 2 ** 50) { const probe = lo + step; if (await fails(probe)) { lo = probe; step *= 2; } else hi = probe; }
  if (hi === null) throw new Error('cannot find the entry that is too large');
  while (hi - lo > 1) { const mid = lo + Math.floor((hi - lo) / 2); if (await fails(mid)) lo = mid; else hi = mid; }
  return hi;
}

/**
 * One page of the inbox. When Wave refuses the answer as too large (`result_too_large`), ask for fewer items, down to one; if one single entry is too large,
 * skip exactly that number with a warning line instead of asking the same thing for ever.
 */
async function inboxPage(o, st, say) {
  let limit = PAGE;
  for (;;) {
    try { return { data: await callTool(o, 'inbox', { since_seq: st.next_seq, limit }), limit }; }
    catch (e) {
      if (e?.code !== 'result_too_large') throw e;
      if (limit > 1) { limit = Math.max(1, Math.floor(limit / 2)); continue; }
      const seq = await findTooLarge(o, st.next_seq);
      say(`${PREFIX} skipped one inbox entry (seq ${seq}) that is too large to read; it stays in Wave, read it with the entry tool`);
      st.next_seq = seq; limit = PAGE;
    }
  }
}

/**
 * One ask: pages through the inbox from the remembered number and prints what is new. Returns the number of lines printed.
 * The state is saved AFTER printing: a crash in between repeats a line (harmless) instead of losing one.
 */
export async function cycle(o, st) {
  let printed = 0, pendingNotice = null;
  // a single ask (--once) prints nothing when there is no news; its fixed line goes first only when there is something to read
  const say = (line) => { if (pendingNotice) { o.out(pendingNotice); pendingNotice = null; } o.out(line); printed++; };
  let rooms = 0, last = null;
  const baseline = st.fresh && !o.fromStart;                    // the first run without a state file: older items are skipped, not shouted
  let skipped = 0;
  for (;;) {
    const { data, limit } = await inboxPage(o, st, say);
    last = data;
    if (!o.noticeShown && typeof data.notice === 'string') {
      const line = `${PREFIX} ${oneLine(data.notice, 200)}`;
      if (o.once) pendingNotice = line; else say(line);
      o.noticeShown = true;
    }
    const items = Array.isArray(data.items) ? data.items : [];
    if (items.some((it) => !o.rooms.has(it.room_id)) && rooms++ < ROOMS_REFRESH_MAX) await roomNames(o).catch(() => {});
    for (const it of items) { if (baseline) skipped++; else say(itemLine(o, it)); }
    if (Number.isSafeInteger(data.next_seq) && data.next_seq > st.next_seq) st.next_seq = data.next_seq;
    if (items.length < limit) break;
  }
  if (baseline && skipped) say(`${PREFIX} ${skipped} older item(s) in the inbox skipped on the first start (use --from-start to see them)`);
  // tasks are a state, not a stream: tell each one once, and again only if it left the list and came back
  const now = { tasks: new Set((last.tasks ?? []).map((t) => t.id)), stuck: new Set((last.stuck ?? []).map((s) => s.task?.id)) };
  for (const t of last.tasks ?? []) if (!st.tasks.has(t.id)) say(taskLine('task assigned to you', t));
  for (const s of last.stuck ?? []) if (s.task && !st.stuck.has(s.task.id)) say(taskLine('task you created is stuck', s.task));
  st.tasks = now.tasks; st.stuck = now.stuck; st.fresh = false;
  // a state file that cannot be written is not an inbox error: say so once, and go on (after a restart some lines may come again)
  try { saveState(o.statePath, st); }
  catch (e) { if (!o.saveFailed) { o.saveFailed = true; say(`${PREFIX} cannot save the state file (${oneLine(e?.code ?? 'error', 20)}); after a restart you may see lines again`); } }
  return printed;
}

/** `<mcp address>` -> `<mcp address>/stream`: the doorbell of Wave (GET, the same key as the tools). */
export function streamUrlOf(url) {
  const u = new URL(url);
  u.pathname = u.pathname.replace(/\/+$/, '') + '/stream'; u.search = ''; u.hash = '';
  return u.toString();
}

/** What the loop waits on: a wake from the stream, or the timeout. A wake that came while the loop was busy is not lost. */
function makeWaker() {
  let release = null, pending = false;
  const w = {
    up: false,
    wake() { pending = true; release?.(); },
    async wait(ms) {
      if (!pending) {
        let timer;
        await new Promise((resolve) => { release = resolve; timer = setTimeout(resolve, ms); });
        clearTimeout(timer); release = null;
      }
      pending = false;
    },
  };
  return w;
}

const WAKE_EVENTS = new Set(['inbox.entry', 'inbox.task', 'inbox.stuck', 'reset']);

/**
 * Holds one SSE connection to Wave's doorbell (GET <mcp address>/stream). The frames hold ids only, never text, so nothing is read from them: any of them just means
 * "look at the inbox now", and the normal cycle does that (state file, notice and outside-text markers as always). While the stream is down the loop polls as before.
 *  - 404 or 405: this Wave has no doorbell: stay on polling, look again every ten minutes;
 *  - a closed or broken stream, a refused key, a silent line (no byte for 70 s: Wave sends a comment every 25 s): try again with a growing wait (1 s up to 5 minutes);
 *  - never follows a redirect (the key travels in a header).
 * Nothing about the stream's state is printed: each line wakes a session.
 */
async function runStream(o, w) {
  const url = streamUrlOf(o.url);
  const pause = o.streamSleep ?? ((ms) => new Promise((r) => { const t = setTimeout(r, ms); t.unref?.(); }));
  let backoff = o.streamBackoffMs ?? 1000;
  while (!o.stopped?.()) {
    const ac = new AbortController();
    let idle;
    const arm = () => { clearTimeout(idle); idle = setTimeout(() => ac.abort(), o.streamIdleMs ?? 70_000); idle.unref?.(); };
    let long = false;
    try {
      const res = await o.fetch(url, { headers: { authorization: `Bearer ${o.key}`, accept: 'text/event-stream' }, redirect: 'error', signal: ac.signal });
      if (res.status === 404 || res.status === 405) { await res.body?.cancel?.(); await pause(o.streamMissingMs ?? 600_000); continue; }
      if (res.status !== 200 || !res.body) { await res.body?.cancel?.(); throw new Error('refused'); }
      w.up = true; w.wake(); arm();                                  // one look at the inbox now: whatever came while the stream was away
      const started = Date.now();
      const reader = res.body.getReader(), dec = new TextDecoder();
      let buf = '', bye = false;
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        arm(); buf += dec.decode(value, { stream: true });
        if (buf.length > 65_536) buf = buf.slice(-65_536);          // a hostile line without an end must not grow without limit
        let i;
        while ((i = buf.indexOf('\n\n')) >= 0) {
          const block = buf.slice(0, i); buf = buf.slice(i + 2);
          const m = /^event: ([a-z0-9_.-]{1,64})$/m.exec(block);
          if (!m) continue;
          if (m[1] === 'bye') bye = true; else if (WAKE_EVENTS.has(m[1])) w.wake();
        }
        if (bye) break;
      }
      long = Date.now() - started > 30_000;
    } catch { /* the loop below decides */ }
    finally { clearTimeout(idle); ac.abort(); }
    if (w.up) { w.up = false; w.wake(); }                           // the loop goes back to polling at once and looks once
    if (o.stopped?.()) return;
    if (long) backoff = o.streamBackoffMs ?? 1000;
    await pause(backoff); backoff = Math.min(backoff * 2, 300_000);
  }
}

/** Runs until stopped (or once). Errors are said once per outage, because each line wakes the session. */
export async function watch(o) {
  o = { rpc: 0, rooms: new Map(), fetch: globalThis.fetch, out: (l) => console.log(l), sleep: (ms) => new Promise((r) => setTimeout(r, ms)), intervalMs: 20_000, ...o };
  const st = loadState(o.statePath);
  if (st.warn) o.out(`${PREFIX} ${st.warn}`);
  let down = null, fails = 0;
  const w = o.stream && !o.once ? makeWaker() : null;      // the doorbell (opt-in: main turns it on); without it the loop polls as it always did
  if (w) void runStream(o, w);
  for (;;) {
    let wait = null;
    try {
      await cycle(o, st);
      if (down) { o.out(`${PREFIX} Wave answers again`); down = null; }
      fails = 0;
    } catch (e) {
      const why = oneLine(e?.message ?? 'error', 80);
      if (down !== why) { o.out(`${PREFIX} cannot read the inbox: ${why}.${o.once ? '' : ' Trying again; no news until it works.'}`); down = why; }
      fails++;
      wait = e?.retryAfterMs ?? null;                            // Wave said how long to keep away: do not ask sooner
      if (o.once) return 1;
    }
    if (o.once) return 0;
    if (o.stopped?.()) return 0;
    if (w) {
      // the stream up: a safety look every few minutes (the doorbell can miss, the inbox cannot); errors and a stream that is down: the polling rhythm as before
      const poll = Math.min(o.intervalMs * 2 ** Math.min(fails, 4), 300_000);
      await w.wait(Math.max(w.up && fails === 0 ? (o.safetyMs ?? 300_000) : poll, wait ?? 0));
      if (!o.stopped?.()) await new Promise((r) => setTimeout(r, o.debounceMs ?? 30));      // several frames at once are one look
    } else await o.sleep(Math.max(Math.min(o.intervalMs * 2 ** Math.min(fails, 4), 300_000), wait ?? 0));
    if (o.stopped?.()) return 0;
  }
}

function parseArgs(argv, env) {
  const a = { once: false, fromStart: false, intervalMs: 20_000, session: env.FLEET_SESSION || 'fleet-watch', statePath: env.FLEET_STATE || null, stream: !['off', 'no', '0'].includes((env.FLEET_STREAM ?? '').toLowerCase()) };
  for (let i = 0; i < argv.length; i++) {
    const x = argv[i];
    if (x === '--once') a.once = true;
    else if (x === '--from-start') a.fromStart = true;
    else if (x === '--no-stream') a.stream = false;
    else if (x === '--interval') { const n = Number(argv[++i]); if (!(n >= 1 && n <= 3600)) throw new Error('--interval is seconds between 1 and 3600'); a.intervalMs = n * 1000; }
    else if (x === '--session') a.session = argv[++i] ?? '';
    else if (x === '--state') a.statePath = argv[++i] ?? '';
    else if (x === '--help' || x === '-h') a.help = true;
    else throw new Error(`unknown option ${oneLine(x, 30)}`);
  }
  return a;
}

const HELP = `fleet-watch: prints one line ("fleet: ...") for each new thing addressed to your agent in Wave.
  WAVE_MCP_URL=https://<wave-api host>/mcp  WAVE_AGENT_KEY=wak_...  node fleet-watch.mjs [options]
  --once             ask once and exit (0 = ok, 1 = could not read, 2 = setup error)
  --interval <s>     seconds between asks while the live line is down (default 20)
  --no-stream        do not hold the live line to Wave (also FLEET_STREAM=off); ask every --interval seconds as before
  --session <name>   name of your session in Wave's log (default fleet-watch; or FLEET_SESSION)
  --state <file>     where it remembers what it told (default ~/.fleet-watch/<id>.json; or FLEET_STATE)
  --from-start       on the first start also print the older items (default: skip them)
The key comes from WAVE_AGENT_KEY, or from the file named by WAVE_AGENT_KEY_FILE, and is never printed.
http:// is accepted for localhost only.`;

/** The address and the key from the environment, or a reason why not. The reason never contains the key. */
export function setupFrom(env) {
  const url = env.WAVE_MCP_URL ?? '';
  let u;
  try { u = new URL(url); } catch { return { error: 'set WAVE_MCP_URL to the /mcp address of Wave' }; }
  if (u.protocol !== 'https:' && u.protocol !== 'http:') return { error: 'WAVE_MCP_URL must start with https://' };
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(u.hostname) || u.hostname.endsWith('.localhost');
  if (u.protocol === 'http:' && !local) return { error: 'WAVE_MCP_URL must use https:// (http:// is only for localhost): the key would travel in the clear' };
  let key = env.WAVE_AGENT_KEY ?? '';
  if (!key && env.WAVE_AGENT_KEY_FILE) {
    try { key = readFileSync(env.WAVE_AGENT_KEY_FILE, 'utf8').trim(); } catch (e) { return { error: `the key file cannot be read (${e?.code ?? 'error'})` }; }
  }
  if (!key) return { error: 'set WAVE_AGENT_KEY (the key of your agent) or WAVE_AGENT_KEY_FILE (a file that holds it)' };
  return { url, key };
}

async function main() {
  // setup errors go to STDOUT as a "fleet:" line: a Monitor or a loop that only reads stdout must hear about them (and the exit code is 2)
  const setupError = (msg) => { console.log(`${PREFIX} setup error: ${oneLine(msg, 200)}`); return 2; };
  let a;
  try { a = parseArgs(process.argv.slice(2), process.env); } catch (e) { return setupError(e.message); }
  if (a.help) { console.log(HELP); return 0; }
  const s = setupFrom(process.env);
  if (s.error) return setupError(s.error);
  let stop = false;
  for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => { stop = true; process.exit(0); });
  return watch({ url: s.url, key: s.key, session: a.session, once: a.once, stream: a.stream, fromStart: a.fromStart, intervalMs: a.intervalMs, statePath: a.statePath || defaultStatePath(s.url, s.key), stopped: () => stop });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().then((c) => { process.exitCode = c; });
