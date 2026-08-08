#!/usr/bin/env node
// Offline tests for sync.mjs: fake herdr CLI + fake session registries,
// exercising every rename, guard, and fail-safe behaviour.
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  utimesSync,
  existsSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";

const here = dirname(fileURLToPath(import.meta.url));
const syncScript = join(here, "..", "sync.mjs");
const fakeHerdr = join(here, "fake-herdr.mjs");

let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${ok || !detail ? "" : ` — ${detail}`}`);
  if (!ok) failures++;
};

const agent = (sessionId, paneId, tabId, wsId, type = "claude") => ({
  agent: type,
  agent_session: { agent: type, kind: "id", source: `herdr:${type}`, value: sessionId },
  agent_status: "idle",
  pane_id: paneId,
  tab_id: tabId,
  workspace_id: wsId,
});

// One sandbox per scenario: fresh HOME, world file, calls log, state dir.
function run({ sessions = [], codexIndex, world, state, lockAgeMs }) {
  const dir = mkdtempSync(join(tmpdir(), "wsren-test-"));
  const home = join(dir, "home");
  mkdirSync(join(home, ".claude", "sessions"), { recursive: true });
  sessions.forEach((s, i) =>
    writeFileSync(join(home, ".claude", "sessions", `${1000 + i}.json`), JSON.stringify(s)),
  );
  if (codexIndex) {
    mkdirSync(join(home, ".codex"), { recursive: true });
    // Entries may be raw strings (passed through verbatim) so tests can
    // express corrupt lines alongside valid ones.
    writeFileSync(
      join(home, ".codex", "session_index.jsonl"),
      codexIndex.map((e) => (typeof e === "string" ? e : JSON.stringify(e))).join("\n") + "\n",
    );
  }
  const worldPath = join(dir, "world.json");
  // "$HOME" in fixture paths becomes the sandbox home, so tests can exercise
  // home-relative behaviour (e.g. ~-shortening) against the real HOME env.
  writeFileSync(worldPath, JSON.stringify(world).replaceAll("$HOME", home));
  const callsPath = join(dir, "calls.log");
  writeFileSync(callsPath, "");
  const metaPath = join(dir, "meta.log");
  writeFileSync(metaPath, "");
  const stateDir = join(dir, "state");
  mkdirSync(stateDir);
  if (state) writeFileSync(join(stateDir, "state.json"), JSON.stringify(state));
  if (lockAgeMs !== undefined) {
    const lock = join(stateDir, ".lock");
    writeFileSync(lock, "99999");
    const t = (Date.now() - lockAgeMs) / 1000;
    utimesSync(lock, t, t);
  }

  const r = spawnSync(process.execPath, [syncScript], {
    encoding: "utf8",
    env: {
      ...process.env,
      HOME: home,
      HERDR_BIN_PATH: fakeHerdr,
      HERDR_PLUGIN_STATE_DIR: stateDir,
      FAKE_HERDR_WORLD: worldPath,
      FAKE_HERDR_CALLS: callsPath,
      FAKE_HERDR_META: metaPath,
    },
  });

  const calls = readFileSync(callsPath, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
  const meta = readFileSync(metaPath, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
  let stateAfter = {};
  try {
    stateAfter = JSON.parse(readFileSync(join(stateDir, "state.json"), "utf8"));
  } catch {}
  const lockLeft = existsSync(join(stateDir, ".lock"));
  rmSync(dir, { recursive: true, force: true });
  return { calls, meta, stateAfter, lockLeft, stderr: r.stderr, status: r.status };
}

const ws = (id, label) => ({ workspace_id: id, label, number: 1, tab_count: 1, pane_count: 1 });
const rootPane = (wsId, cwd) => ({ [`${wsId}:p1`]: { pane_id: `${wsId}:p1`, cwd } });

// A user-renamed session in a default-labelled workspace renames it
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "my-cool-task" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("user-named session renames default-labelled workspace",
    r.calls.length === 1 && r.calls[0][0] === "w1" && r.calls[0][1] === "my-cool-task",
    JSON.stringify(r.calls) + r.stderr);
  check("rename recorded in plugin state", r.stateAfter.w1 === "my-cool-task");
}

// Label equals our last write → a re-rename follows
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "second-name" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "first-name")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
    state: { w1: "first-name" },
  });
  check("re-rename updates a label the plugin set",
    r.calls.length === 1 && r.calls[0][1] === "second-name",
    JSON.stringify(r.calls) + r.stderr);
}

// Manual label (≠ default, ≠ state) → untouched, state dropped
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "plugin-wants-this" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "user-chose-this")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
    state: { w1: "old-plugin-name" },
  });
  check("manually named workspace never touched", r.calls.length === 0, JSON.stringify(r.calls));
  check("state entry dropped when user overrides label", !("w1" in r.stateAfter), JSON.stringify(r.stateAfter));
}

// Auto-derived name → no-op
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes-27", nameSource: "derived" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("derived session name never renames", r.calls.length === 0, JSON.stringify(r.calls));
}

// Only the primary agent (lowest pane of first tab) drives the label
{
  const r = run({
    sessions: [
      { pid: 1, sessionId: "primary", name: "prim-3f", nameSource: "derived" },
      { pid: 2, sessionId: "guest", name: "guest-renamed" },
    ],
    world: {
      agents: [
        agent("primary", "w1:p1", "w1:t1", "w1"),
        agent("guest", "w1:p3", "w1:t1", "w1"),
      ],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("guest session rename ignored", r.calls.length === 0, JSON.stringify(r.calls));
}

// Flip side: a user-named primary wins even with a guest present
{
  const r = run({
    sessions: [
      { pid: 1, sessionId: "primary", name: "primary-name" },
      { pid: 2, sessionId: "guest", name: "guest-name" },
    ],
    world: {
      agents: [
        agent("guest", "w1:p3", "w1:t1", "w1"),
        agent("primary", "w1:p1", "w1:t1", "w1"),
      ],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("primary drives the label (order-independent)",
    r.calls.length === 1 && r.calls[0][1] === "primary-name",
    JSON.stringify(r.calls) + r.stderr);
}

// Name already matches the label → no rename call at all
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("no-op when label already matches", r.calls.length === 0, JSON.stringify(r.calls));
}

// Weird names get cleaned, not slugged
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "  Fix\tthe   thing\x07 " + "x".repeat(60) }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  const got = r.calls[0]?.[1] ?? "";
  check("clean(): collapsed, control-stripped, capped at 32",
    r.calls.length === 1 && got.startsWith("Fix the thing") && got.length <= 32 && !/[\x00-\x1f\x7f]/.test(got),
    JSON.stringify(got));
}

// Provider seam: primary agent of a type with no provider → whole workspace
// skips, even when a user-named claude guest is present.
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "guest", name: "guest-renamed" }],
    world: {
      agents: [
        agent("gemini-sess", "w1:p1", "w1:t1", "w1", "gemini"),
        agent("guest", "w1:p3", "w1:t1", "w1"),
      ],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("unmatched primary agent type → workspace skipped", r.calls.length === 0,
    JSON.stringify(r.calls) + r.stderr);
}

// A primary pane herdr hasn't joined yet (no agent_session) blocks its tab —
// primacy never falls through to a same-tab guest.
{
  const noSession = agent("x", "w1:p1", "w1:t1", "w1", "codex");
  delete noSession.agent_session;
  const r = run({
    sessions: [{ pid: 1, sessionId: "guest", name: "guest-renamed" }],
    world: {
      agents: [noSession, agent("guest", "w1:p3", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("unjoined primary blocks workspace (guest never drives)",
    r.calls.length === 0, JSON.stringify(r.calls) + r.stderr);
}

// …but a tab herdr couldn't join at all doesn't block the workspace: the
// first joinable tab's agent is primary.
{
  const noSession = agent("x", "w1:p1", "w1:t1", "w1", "codex");
  delete noSession.agent_session;
  const r = run({
    sessions: [{ pid: 1, sessionId: "cl-1", name: "tab-two-name" }],
    world: {
      agents: [noSession, agent("cl-1", "w1:p2", "w1:t2", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("unjoinable first tab falls through to next tab's agent",
    r.calls.length === 1 && r.calls[0][1] === "tab-two-name",
    JSON.stringify(r.calls) + r.stderr);
}

// An empty-string session id is never a join key — neither as an agent's
// session value nor as a registry/index entry id.
{
  const r = run({
    codexIndex: [
      { id: "", thread_name: "stray-name", updated_at: "2026-08-08T21:40:59Z" },
    ],
    world: {
      agents: [agent("", "w1:p1", "w1:t1", "w1", "codex")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("empty-string session id never joins", r.calls.length === 0,
    JSON.stringify(r.calls) + r.stderr);
}

// Codex: a later tombstone entry (thread_name no longer a string) clears the
// name instead of leaving the stale one in force
{
  const r = run({
    codexIndex: [
      { id: "cdx-1", thread_name: "old-name", updated_at: "2026-08-08T21:00:00Z" },
      { id: "cdx-1", thread_name: null, updated_at: "2026-08-08T22:00:00Z" },
    ],
    world: {
      agents: [agent("cdx-1", "w1:p1", "w1:t1", "w1", "codex")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("codex tombstone entry clears a stale name", r.calls.length === 0,
    JSON.stringify(r.calls) + r.stderr);
}

// Codex: duplicate ids resolve by updated_at, not line order
{
  const r = run({
    codexIndex: [
      { id: "cdx-1", thread_name: "new-name", updated_at: "2026-08-08T22:00:00Z" },
      { id: "cdx-1", thread_name: "old-name", updated_at: "2026-08-08T21:00:00Z" },
    ],
    world: {
      agents: [agent("cdx-1", "w1:p1", "w1:t1", "w1", "codex")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("codex duplicate ids: newest updated_at wins over line order",
    r.calls.length === 1 && r.calls[0][1] === "new-name",
    JSON.stringify(r.calls) + r.stderr);
}

// Codex: one corrupt line doesn't poison the rest of the index
{
  const r = run({
    codexIndex: [
      '{"id":"cdx-1","thread_na GARBAGE',
      { id: "cdx-2", thread_name: "good-name", updated_at: "2026-08-08T22:00:00Z" },
    ],
    world: {
      agents: [agent("cdx-2", "w1:p1", "w1:t1", "w1", "codex")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("codex corrupt index line skipped, valid lines still apply",
    r.calls.length === 1 && r.calls[0][1] === "good-name",
    JSON.stringify(r.calls) + r.stderr);
}

// Codex: named thread (present in session_index.jsonl) → rename
{
  const r = run({
    codexIndex: [
      { id: "cdx-1", thread_name: "codex-task-name", updated_at: "2026-08-08T21:40:59Z" },
    ],
    world: {
      agents: [agent("cdx-1", "w1:p1", "w1:t1", "w1", "codex")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("codex named thread renames default-labelled workspace",
    r.calls.length === 1 && r.calls[0][1] === "codex-task-name",
    JSON.stringify(r.calls) + r.stderr);
}

// Codex: session absent from the index (unnamed/auto) → no opinion, no-op
{
  const r = run({
    codexIndex: [
      { id: "other", thread_name: "someone-else", updated_at: "2026-08-08T21:40:59Z" },
    ],
    world: {
      agents: [agent("cdx-unnamed", "w1:p1", "w1:t1", "w1", "codex")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("codex unnamed session never renames", r.calls.length === 0,
    JSON.stringify(r.calls) + r.stderr);
}

// Codex: duplicate ids — the newest entry wins (re-rename)
{
  const r = run({
    codexIndex: [
      { id: "cdx-1", thread_name: "old-name", updated_at: "2026-08-08T21:00:00Z" },
      { id: "cdx-1", thread_name: "new-name", updated_at: "2026-08-08T22:00:00Z" },
    ],
    world: {
      agents: [agent("cdx-1", "w1:p1", "w1:t1", "w1", "codex")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("codex duplicate index lines: latest wins",
    r.calls.length === 1 && r.calls[0][1] === "new-name",
    JSON.stringify(r.calls) + r.stderr);
}

// Mixed providers in one sweep: claude ws and codex ws both rename
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "cl-1", name: "claude-name" }],
    codexIndex: [
      { id: "cdx-1", thread_name: "codex-name", updated_at: "2026-08-08T21:40:59Z" },
    ],
    world: {
      agents: [
        agent("cl-1", "w1:p1", "w1:t1", "w1"),
        agent("cdx-1", "w2:p1", "w2:t1", "w2", "codex"),
      ],
      workspaces: [ws("w1", "notes"), ws("w2", "notes")],
      panes: { ...rootPane("w1", "/Users/ryan/dev/notes"), ...rootPane("w2", "/Users/ryan/dev/notes") },
    },
  });
  const byWs = Object.fromEntries(r.calls.map((c) => [c[0], c[1]]));
  check("mixed sweep: claude and codex workspaces both sync",
    r.calls.length === 2 && byWs.w1 === "claude-name" && byWs.w2 === "codex-name",
    JSON.stringify(r.calls) + r.stderr);
}

// Concurrency: a fresh lock (another sweep in flight) → skip entirely
{
  const world = {
    agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
    workspaces: [ws("w1", "notes")],
    panes: rootPane("w1", "/Users/ryan/dev/notes"),
  };
  const sessions = [{ pid: 1, sessionId: "s1", name: "wants-this" }];
  const fresh = run({ sessions, world, lockAgeMs: 0 });
  check("fresh lock held → sweep skipped, lock preserved",
    fresh.calls.length === 0 && fresh.lockLeft && fresh.status === 0,
    `calls=${JSON.stringify(fresh.calls)} lockLeft=${fresh.lockLeft}`);

  // …but a stale lock (crashed sweep) is stolen and the sweep proceeds
  const stale = run({ sessions, world, lockAgeMs: 60_000 });
  check("stale lock stolen → sweep proceeds, lock released",
    stale.calls.length === 1 && stale.calls[0][1] === "wants-this" && !stale.lockLeft,
    `calls=${JSON.stringify(stale.calls)} lockLeft=${stale.lockLeft}${stale.stderr}`);

  // …and a normal run leaves no lock behind
  const normal = run({ sessions, world });
  check("normal run releases the lock", normal.calls.length === 1 && !normal.lockLeft,
    `calls=${JSON.stringify(normal.calls)} lockLeft=${normal.lockLeft}`);
}

// Stale: session id not in registry → skip whole workspace
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "someone-else", name: "irrelevant" }],
    world: {
      agents: [agent("not-in-registry", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("join drops sessions missing from registry", r.calls.length === 0, JSON.stringify(r.calls));
}

// Fail-safe: root pane get fails → skip, exit 0
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "wants-this" }],
    world: {
      agents: [agent("s1", "w1:p2", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: {}, // no p1 → pane get exits 1
    },
  });
  check("missing root pane → safe skip, exit 0",
    r.calls.length === 0 && r.status === 0, `status=${r.status} ${JSON.stringify(r.calls)}`);
}

// Multi-workspace sweep: only the eligible one is renamed
{
  const r = run({
    sessions: [
      { pid: 1, sessionId: "s1", name: "eligible-name" },
      { pid: 2, sessionId: "s2", name: "blocked-name" },
    ],
    world: {
      agents: [
        agent("s1", "w1:p1", "w1:t1", "w1"),
        agent("s2", "w2:p1", "w2:t1", "w2"),
      ],
      workspaces: [ws("w1", "notes"), ws("w2", "manual-label")],
      panes: { ...rootPane("w1", "/Users/ryan/dev/notes"), ...rootPane("w2", "/Users/ryan/dev/notes") },
    },
  });
  check("global sweep renames only eligible workspaces",
    r.calls.length === 1 && r.calls[0][0] === "w1",
    JSON.stringify(r.calls) + r.stderr);
}

// A rename also reports the workspace dir as a $dir sidebar token, ~-shortened
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "my-cool-task" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
  });
  check("rename reports ~-shortened dir metadata",
    r.calls.length === 1 &&
      r.meta.length === 1 &&
      r.meta[0].join(" ") === "w1 --source io.rlew.workspace-renamer --token dir=~/dev/notes",
    JSON.stringify(r.meta) + r.stderr);
}

// An owned, already-in-sync workspace refreshes the dir token without renaming
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "our-name" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "our-name")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
    state: { w1: "our-name" },
  });
  check("in-sync owned workspace refreshes dir metadata, no rename",
    r.calls.length === 0 && r.meta.length === 1 && r.meta[0].includes("dir=~/dev/notes"),
    JSON.stringify({ calls: r.calls, meta: r.meta }) + r.stderr);
}

// A user override clears the dir token along with the state entry
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "plugin-wants-this" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "user-chose-this")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
    state: { w1: "old-plugin-name" },
  });
  check("user override clears dir metadata",
    r.calls.length === 0 &&
      r.meta.length === 1 &&
      r.meta[0].join(" ") === "w1 --source io.rlew.workspace-renamer --clear-token dir",
    JSON.stringify(r.meta) + r.stderr);
}

// Workspaces the plugin never renamed get no metadata at all
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes-27", nameSource: "derived" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
  });
  check("untouched workspace gets no dir metadata", r.meta.length === 0,
    JSON.stringify(r.meta) + r.stderr);
}

console.log(failures ? `\n${failures} failure(s)` : "\nall tests passed");
process.exit(failures ? 1 : 0);
