#!/usr/bin/env node
// Workspace Renamer — sync agent session names onto herdr workspace labels.
// Global idempotent reconcile: every invocation sweeps all workspaces (event
// payload ignored), so a missed event self-heals on the next one. A workspace
// whose label isn't the default (or our own last write) is never touched —
// manual names win, permanently.
//
// Fail-safe posture: any parse/shape surprise → skip + one line to stderr.
// Doing nothing is always acceptable; a wrong rename is the only real failure.

import {
  readFileSync,
  readdirSync,
  writeFileSync,
  mkdirSync,
  statSync,
  openSync,
  closeSync,
  renameSync,
  unlinkSync,
  utimesSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { join, basename } from "node:path";
import { homedir } from "node:os";

const DRY = process.argv.includes("--dry-run");
const HERDR = process.env.HERDR_BIN_PATH || "herdr";
const HOME = homedir();
const METADATA_SOURCE = "io.rlew.workspace-renamer";
// --dry-run from a plain shell must read the real ownership state or the
// preview diverges from event-driven behaviour, so herdr's default plugin
// state path fills in when the env var is absent. Real runs still require the
// env var — its presence is the proof we're running under herdr.
const STATE_DIR =
  process.env.HERDR_PLUGIN_STATE_DIR ||
  (DRY
    ? join(homedir(), ".local", "state", "herdr", "plugins", METADATA_SOURCE)
    : null);
const REGISTRY_DIR = join(homedir(), ".claude", "sessions");
const CODEX_INDEX = join(homedir(), ".codex", "session_index.jsonl");
const LOCK_STALE_MS = 30_000;
const RETRY_WINDOW_MS = 3_000;
const MAX_LABEL = 32;

const warn = (msg) => process.stderr.write(`workspace-renamer: ${msg}\n`);

function herdr(...args) {
  const r = spawnSync(HERDR, args, { encoding: "utf8", timeout: 10_000 });
  if (r.error) throw new Error(`herdr ${args.join(" ")}: ${r.error.message}`);
  if (r.status !== 0) {
    throw new Error(
      `herdr ${args.join(" ")}: exit ${r.status} ${(r.stderr || "").trim()}`,
    );
  }
  // Some verbs (e.g. report-metadata) succeed silently with no output.
  return r.stdout.trim() === "" ? null : JSON.parse(r.stdout);
}

// ---- sidebar dir metadata ---------------------------------------------
// Once we rename a workspace, its label stops saying where you are. So for
// workspaces whose label we own, report the root pane's directory as
// display-only workspace metadata: a custom `$dir` sidebar token, rendered
// only if the user's sidebar layout includes "$dir" (see README). Re-reported
// every sweep so it heals after a herdr restart; cleared the moment the user
// takes the label back.

const tildify = (p) =>
  p === HOME ? "~" : p.startsWith(HOME + "/") ? "~" + p.slice(HOME.length) : p;

function reportDirToken(wsId, cwd) {
  if (DRY) return;
  try {
    // Workspace id first: herdr's parser rejects options-then-positional here.
    herdr(
      "workspace",
      "report-metadata",
      wsId,
      "--source",
      METADATA_SOURCE,
      "--token",
      `dir=${tildify(cwd)}`,
    );
  } catch (e) {
    warn(`report dir metadata for ${wsId} failed: ${e.message}`);
  }
}

function clearDirToken(wsId) {
  if (DRY) return;
  try {
    herdr(
      "workspace",
      "report-metadata",
      wsId,
      "--source",
      METADATA_SOURCE,
      "--clear-token",
      "dir",
    );
  } catch (e) {
    warn(`clear dir metadata for ${wsId} failed: ${e.message}`);
  }
}

// ---- name providers ---------------------------------------------------
// Each provider owns one agent type and answers a single question: "what
// name does the user intend for this session?" The contract:
//   matches(agent) — claims a herdr agent record by type.
//   load()         — reads whatever registry the agent keeps and returns a
//                    resolver (agent → name | null), or null when there is
//                    no data at all. Loaded lazily, at most once per sweep,
//                    and only when a matching agent is actually primary in
//                    some workspace.
// A null name means "no opinion" and always collapses to the no-op path, so
// adding a provider can never weaken the fail-safe posture. Everything else
// (workspace join, primary selection, ownership guard, state) is agent-
// agnostic and lives in main().

// Parse a list of JSON texts leniently, folding each parsed record into a
// Map via `apply(record, map)`. Malformed entries are skipped silently — bad
// data means "no session info here", never a hard failure.
function foldJsonRecords(texts, apply) {
  const map = new Map();
  for (const text of texts) {
    try {
      apply(JSON.parse(text), map);
    } catch {
      // unparseable (mid-write, stale junk) — skip
    }
  }
  return map;
}

// ~/.claude/sessions/<pid>.json — one file per running Claude Code session,
// an undocumented Claude Code internal (shape observed on 2.1.226): `name`
// is the session name; `nameSource: "derived"` marks auto-generated names
// and is absent on user-renamed sessions. Anything surprising is treated as
// "no session info".
function readClaudeRegistry() {
  let files;
  try {
    files = readdirSync(REGISTRY_DIR).filter((f) => f.endsWith(".json"));
  } catch {
    return new Map(); // no registry → nothing to sync
  }
  const texts = [];
  for (const f of files) {
    try {
      texts.push(readFileSync(join(REGISTRY_DIR, f), "utf8"));
    } catch {
      // file vanished mid-sweep — skip
    }
  }
  return foldJsonRecords(texts, (j, map) => {
    if (
      typeof j?.sessionId === "string" &&
      j.sessionId !== "" &&
      typeof j?.name === "string"
    ) {
      map.set(j.sessionId, j);
    }
  });
}

const claudeProvider = {
  id: "claude",
  matches: (agent) => agent?.agent === "claude",
  load() {
    const registry = readClaudeRegistry();
    if (registry.size === 0) return null;
    return (agent) => {
      const sess = registry.get(agent.agent_session.value);
      if (!sess) return null; // stale/foreign — not a live local session
      if (sess.nameSource === "derived") return null; // auto-name: never rename
      return sess.name;
    };
  },
};

// ~/.codex/session_index.jsonl — append-only, one line per *named* codex
// thread ({id, thread_name, updated_at}); auto/unnamed sessions never appear,
// so index presence doubles as the "explicitly named" signal (codex has no
// equivalent of Claude Code's nameSource field). For duplicate ids the entry
// with the newest updated_at wins, falling back to line order when the
// timestamps don't decide; an entry whose thread_name is no longer a string
// is a tombstone that clears the name. Join key is the same
// agent_session.value herdr reports for claude panes — codex panes get theirs
// from herdr's codex SessionStart hook integration.
const codexProvider = {
  id: "codex",
  matches: (agent) => agent?.agent === "codex",
  load() {
    let raw;
    try {
      raw = readFileSync(CODEX_INDEX, "utf8");
    } catch {
      return null; // no index → no named threads → no opinion
    }
    const entries = foldJsonRecords(
      raw.split("\n").filter((l) => l.trim()),
      (j, map) => {
        if (typeof j?.id !== "string" || j.id === "") return;
        const name = typeof j.thread_name === "string" ? j.thread_name : null;
        const at = Date.parse(j.updated_at);
        const prev = map.get(j.id);
        // Keep the previous entry only when both timestamps are usable and
        // the previous one is strictly newer; otherwise the later line wins.
        if (prev && Number.isFinite(at) && Number.isFinite(prev.at) && at < prev.at) {
          return;
        }
        map.set(j.id, { name, at });
      },
    );
    if (entries.size === 0) return null;
    return (agent) => entries.get(agent.agent_session.value)?.name ?? null;
  },
};

const PROVIDERS = [claudeProvider, codexProvider];

// Trim, collapse whitespace, strip control chars, cap length. No re-slugging —
// the user typed what they want. The cap counts code points, not UTF-16
// units, so it can't split an emoji's surrogate pair.
function clean(name) {
  const scrubbed = name
    .replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, "") // controls; \t \n \r survive to become spaces
    .replace(/\s+/g, " ")
    .trim();
  return [...scrubbed].slice(0, MAX_LABEL).join("").trim();
}

const paneNum = (id) => {
  const m = /:p(\d+)$/.exec(id ?? "");
  return m ? Number(m[1]) : Infinity;
};
const tabNum = (id) => {
  const m = /:t(\d+)$/.exec(id ?? "");
  return m ? Number(m[1]) : Infinity;
};

const hasSession = (agent) =>
  typeof agent?.agent_session?.value === "string" &&
  agent.agent_session.value !== "";

function readState() {
  if (!STATE_DIR) return {};
  try {
    const j = JSON.parse(readFileSync(join(STATE_DIR, "state.json"), "utf8"));
    return j && typeof j === "object" && !Array.isArray(j) ? j : {};
  } catch {
    return {};
  }
}

// Returns false when the state could not be persisted — callers must then
// abandon any renames that depend on it. (This machinery is vendored by
// design and mirrors github.com/ryanlewis/herdr-tab-renamer's rename.mjs —
// when fixing a bug here, port it there, and vice versa.)
function writeState(state) {
  if (!STATE_DIR || DRY) return true;
  try {
    mkdirSync(STATE_DIR, { recursive: true });
    // Atomic replace: a torn state.json would make readState() return {} in a
    // concurrent sweep, which the ownership guard would misread as "the user
    // named these workspaces" — permanently. Write-then-rename makes that
    // unobservable.
    const tmp = join(STATE_DIR, `state.json.tmp-${process.pid}`);
    writeFileSync(tmp, JSON.stringify(state, null, 2) + "\n");
    renameSync(tmp, join(STATE_DIR, "state.json"));
    return true;
  } catch (e) {
    warn(`state write failed: ${e.message}`);
    return false;
  }
}

// Serialize whole sweeps: overlapping event-triggered processes would race on
// read-modify-write of state.json (last writer drops the other's entries,
// with the same permanent-lockout consequence as a torn read). A lock whose
// mtime is older than LOCK_STALE_MS is from a crashed sweep — live sweeps
// refresh it via touchLock() between herdr calls.
function acquireLock() {
  if (!STATE_DIR || DRY) return true; // nothing to serialize against
  const lock = join(STATE_DIR, ".lock");
  try {
    mkdirSync(STATE_DIR, { recursive: true });
    writeFileSync(lock, String(process.pid), { flag: "wx" });
    return true;
  } catch {
    try {
      if (Date.now() - statSync(lock).mtimeMs > LOCK_STALE_MS) {
        // Steal atomically: rename is exclusive, so exactly one of any
        // concurrent stealers evicts the stale lock (the losers throw
        // ENOENT), then the vacated slot is contended for with the same
        // exclusive create as above.
        const tomb = join(STATE_DIR, `.lock.stale-${process.pid}`);
        renameSync(lock, tomb);
        unlinkSync(tomb);
        writeFileSync(lock, String(process.pid), { flag: "wx" });
        return true;
      }
    } catch {
      // lock vanished, unreadable, or another stealer won — skip this sweep
    }
    return false;
  }
}

// The lock's mtime doubles as its liveness signal — refresh it between herdr
// calls so a legitimately slow sweep isn't mistaken for a crashed one.
function touchLock() {
  if (!STATE_DIR || DRY) return;
  try {
    const now = new Date();
    utimesSync(join(STATE_DIR, ".lock"), now, now);
  } catch {
    // lock gone (stolen after a stall) — nothing to refresh
  }
}

function releaseLock() {
  if (!STATE_DIR || DRY) return;
  const lock = join(STATE_DIR, ".lock");
  try {
    // Only remove a lock we still own — after a stall past LOCK_STALE_MS ours
    // may have been stolen, and the file now serializes someone else's sweep.
    if (readFileSync(lock, "utf8") === String(process.pid)) unlinkSync(lock);
  } catch {
    // already gone — fine
  }
}

const sleep = (ms) =>
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

// .last-sweep's mtime records when the most recent sweep STARTED (only a
// sweep that started after an event arrived can have seen that event's
// effects — see the entrypoint).
function lastSweepStart() {
  try {
    return statSync(join(STATE_DIR, ".last-sweep")).mtimeMs;
  } catch {
    return 0;
  }
}

function stampSweepStart() {
  try {
    mkdirSync(STATE_DIR, { recursive: true });
    closeSync(openSync(join(STATE_DIR, ".last-sweep"), "w"));
  } catch {
    // stamp write failing just means extra sweeps — harmless
  }
}

function main() {
  const agents = herdr("agent", "list")?.result?.agents;
  const workspaces = herdr("workspace", "list")?.result?.workspaces;
  if (!Array.isArray(agents) || !Array.isArray(workspaces)) {
    warn("unexpected herdr list output shape; no-op");
    return;
  }

  // Lazy per-sweep provider cache: a provider's registry is only read when an
  // agent of its type is actually primary somewhere (no point parsing the
  // codex index on a claude-only machine, and vice versa).
  const loadedResolvers = new Map();
  const resolverFor = (agent) => {
    const provider = PROVIDERS.find((p) => p.matches(agent));
    if (!provider) return null;
    if (!loadedResolvers.has(provider.id)) {
      let resolve = null;
      try {
        resolve = provider.load();
      } catch (e) {
        warn(`provider ${provider.id} load failed: ${e.message}`);
      }
      loadedResolvers.set(provider.id, resolve);
    }
    return loadedResolvers.get(provider.id);
  };

  const state = readState();
  let stateDirty = false;

  // Drop state for workspaces that no longer exist.
  const liveIds = new Set(workspaces.map((w) => w?.workspace_id));
  for (const id of Object.keys(state)) {
    if (!liveIds.has(id)) {
      delete state[id];
      stateDirty = true;
    }
  }

  const plan = [];
  for (const ws of workspaces) {
    const wsId = ws?.workspace_id;
    const label = ws?.label;
    if (typeof wsId !== "string" || typeof label !== "string") continue;
    touchLock(); // each iteration can spend a herdr call on `pane get`

    const wsAgents = agents.filter((a) => a?.workspace_id === wsId);
    const joined = wsAgents.filter(hasSession);
    if (joined.length === 0) continue;

    // Only the primary agent drives the label; guest sessions are ignored.
    // The primary tab is the lowest tab holding a joinable agent (a tab herdr
    // couldn't join at all doesn't block the workspace), but within that tab
    // the lowest agent pane wins even if unjoined — a sibling pane must never
    // speak for the tab's real owner. On a pane-number tie, prefer the joined
    // record.
    const minTab = Math.min(...joined.map((a) => tabNum(a.tab_id)));
    const primary = wsAgents
      .filter((a) => tabNum(a.tab_id) === minTab)
      .reduce((best, a) => {
        const pa = paneNum(a.pane_id);
        const pb = paneNum(best.pane_id);
        if (pa !== pb) return pa < pb ? a : best;
        return hasSession(best) || !hasSession(a) ? best : a;
      });
    if (!hasSession(primary)) continue;

    const resolve = resolverFor(primary);
    const rawName = resolve ? resolve(primary) : null;
    if (typeof rawName !== "string") continue;
    const want = clean(rawName);
    if (!want) continue;

    // Ownership guard: only touch a label that is the default (root-pane cwd
    // basename) or our own last write. Anything else → the user named this
    // workspace, and their choice is permanent.
    let rootCwd;
    try {
      rootCwd = herdr("pane", "get", `${wsId}:p1`)?.result?.pane?.cwd;
    } catch (e) {
      warn(`pane get ${wsId}:p1 failed (${e.message}); skipping ${wsId}`);
      continue;
    }
    if (typeof rootCwd !== "string" || rootCwd === "") continue;

    if (label !== basename(rootCwd) && label !== state[wsId]) {
      if (wsId in state) {
        // user overrode our write — locked from now on; retire our dir row too
        delete state[wsId];
        stateDirty = true;
        clearDirToken(wsId);
      }
      continue;
    }

    if (want === label) {
      // Already in sync — no rename (also breaks any rename feedback loop).
      // If the label is ours, keep the sidebar dir token alive.
      if (label === state[wsId]) reportDirToken(wsId, rootCwd);
      continue;
    }

    if (DRY) {
      warn(`[dry-run] would rename ${wsId} "${label}" -> "${want}"`);
      continue;
    }
    plan.push({ wsId, label, want, rootCwd });
  }

  // Persist intent BEFORE renaming: if a rename landed but the state write
  // didn't, the next sweep would misread our own label as user-named and lock
  // the workspace out permanently. The reverse failure (intent recorded,
  // rename lost) is harmless — the label stays default-eligible and
  // self-heals on the next sweep.
  for (const p of plan) state[p.wsId] = p.want;
  if ((stateDirty || plan.length > 0) && !writeState(state)) return;

  for (const p of plan) {
    touchLock();
    try {
      // Re-read the label at the last moment: the list snapshot is stale by
      // now, and a manual rename landing mid-sweep must win.
      const live = herdr("workspace", "get", p.wsId)?.result?.workspace?.label;
      if (live !== p.label) continue;
      herdr("workspace", "rename", p.wsId, p.want);
      warn(`renamed ${p.wsId} "${p.label}" -> "${p.want}"`);
      reportDirToken(p.wsId, p.rootCwd);
    } catch (e) {
      warn(`rename ${p.wsId} failed: ${e.message}`);
    }
  }
}

// Outside herdr (no state dir) a real run would rename workspaces without
// recording ownership, permanently orphaning them from future syncs — refuse
// rather than half-work. --dry-run stays available for previewing.
if (!STATE_DIR && !DRY) {
  warn(
    "HERDR_PLUGIN_STATE_DIR is not set (not running under herdr?); refusing to rename without state tracking — use --dry-run to preview",
  );
} else if (DRY) {
  try {
    main();
  } catch (e) {
    warn(e.message); // fail safe: any surprise is a no-op
  }
} else {
  // An in-flight sweep may have read herdr's state BEFORE the change that
  // fired this event, so "a sweep is already running" is not coverage — this
  // event is covered only by a sweep that STARTED after it arrived. On
  // contention, wait briefly and re-check rather than fire-and-forget
  // skipping (which would leave a label stale until some unrelated future
  // event). Give up after RETRY_WINDOW_MS; herdr events are frequent enough
  // that a later sweep self-heals a rare miss.
  const arrival = Date.now();
  for (;;) {
    if (lastSweepStart() > arrival) break; // a newer sweep covered this event
    if (acquireLock()) {
      try {
        if (lastSweepStart() <= arrival) {
          stampSweepStart();
          main();
        }
      } catch (e) {
        warn(e.message); // fail safe: any surprise is a no-op
      } finally {
        releaseLock();
      }
      break;
    }
    if (Date.now() - arrival > RETRY_WINDOW_MS) break;
    sleep(50);
  }
}
