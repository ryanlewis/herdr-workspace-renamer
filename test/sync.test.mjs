// Offline tests for sync.mjs: fake herdr CLI + fake session registries,
// exercising every rename, guard, and fail-safe behaviour. Each scenario runs
// in its own sandbox (fresh HOME, world state, plugin state dir).
import { test } from "node:test";
import assert from "node:assert/strict";
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

const agent = (sessionId, paneId, tabId, wsId, type = "claude") => ({
  agent: type,
  agent_session: { agent: type, kind: "id", source: `herdr:${type}`, value: sessionId },
  agent_status: "idle",
  pane_id: paneId,
  tab_id: tabId,
  workspace_id: wsId,
});

const ws = (id, label) => ({ workspace_id: id, label, number: 1, tab_count: 1, pane_count: 1 });
const rootPane = (wsId, cwd) => ({ [`${wsId}:p1`]: { pane_id: `${wsId}:p1`, cwd } });

// One sandbox per scenario: fresh HOME, world file, calls/meta logs, state dir.
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

test("user-named session renames default-labelled workspace", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "my-cool-task" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  assert.deepEqual(r.calls, [["w1", "my-cool-task"]], r.stderr);
  assert.equal(r.stateAfter.w1, "my-cool-task", "rename recorded in plugin state");
});

test("re-rename updates a label the plugin set", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "second-name" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "first-name")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
    state: { w1: "first-name" },
  });
  assert.deepEqual(r.calls, [["w1", "second-name"]], r.stderr);
});

test("manually named workspace never touched; state entry dropped", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "plugin-wants-this" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "user-chose-this")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
    state: { w1: "old-plugin-name" },
  });
  assert.deepEqual(r.calls, []);
  assert.ok(!("w1" in r.stateAfter), "state entry dropped when user overrides label");
});

test("derived session name never renames", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes-27", nameSource: "derived" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  assert.deepEqual(r.calls, []);
});

test("guest session rename ignored — only the primary agent drives the label", () => {
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
  assert.deepEqual(r.calls, []);
});

test("primary drives the label regardless of agent list order", () => {
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
  assert.deepEqual(r.calls, [["w1", "primary-name"]], r.stderr);
});

test("no-op when label already matches", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  assert.deepEqual(r.calls, []);
});

test("clean(): whitespace collapsed, control chars stripped, capped at 32", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "  Fix\tthe   thing\x07 " + "x".repeat(60) }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  assert.equal(r.calls.length, 1, r.stderr);
  const got = r.calls[0][1];
  assert.ok(got.startsWith("Fix the thing"), got);
  assert.ok(got.length <= 32, got);
  assert.ok(!/[\x00-\x1f\x7f]/.test(got), got);
});

test("primary agent of a type with no provider skips the workspace", () => {
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
  assert.deepEqual(r.calls, [], r.stderr);
});

test("unjoined primary blocks its tab — same-tab guest never drives", () => {
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
  assert.deepEqual(r.calls, [], r.stderr);
});

test("unjoinable first tab falls through to the next tab's agent", () => {
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
  assert.deepEqual(r.calls, [["w1", "tab-two-name"]], r.stderr);
});

test("empty-string session id never joins", () => {
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
  assert.deepEqual(r.calls, [], r.stderr);
});

test("codex named thread renames default-labelled workspace", () => {
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
  assert.deepEqual(r.calls, [["w1", "codex-task-name"]], r.stderr);
});

test("codex unnamed session (absent from index) never renames", () => {
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
  assert.deepEqual(r.calls, [], r.stderr);
});

test("codex duplicate ids: newest entry wins (re-rename)", () => {
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
  assert.deepEqual(r.calls, [["w1", "new-name"]], r.stderr);
});

test("codex duplicate ids: updated_at beats line order", () => {
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
  assert.deepEqual(r.calls, [["w1", "new-name"]], r.stderr);
});

test("codex tombstone entry clears a stale name", () => {
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
  assert.deepEqual(r.calls, [], r.stderr);
});

test("codex corrupt index line skipped, valid lines still apply", () => {
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
  assert.deepEqual(r.calls, [["w1", "good-name"]], r.stderr);
});

test("mixed sweep: claude and codex workspaces both sync", () => {
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
  assert.deepEqual(Object.fromEntries(r.calls), { w1: "claude-name", w2: "codex-name" }, r.stderr);
});

const lockScenario = {
  sessions: [{ pid: 1, sessionId: "s1", name: "wants-this" }],
  world: {
    agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
    workspaces: [ws("w1", "notes")],
    panes: rootPane("w1", "/Users/ryan/dev/notes"),
  },
};

test("fresh lock held by another sweep: skipped, lock preserved", () => {
  const r = run({ ...lockScenario, lockAgeMs: 0 });
  assert.deepEqual(r.calls, []);
  assert.ok(r.lockLeft, "foreign lock must not be removed");
  assert.equal(r.status, 0);
});

test("stale lock stolen: sweep proceeds, lock released", () => {
  const r = run({ ...lockScenario, lockAgeMs: 60_000 });
  assert.deepEqual(r.calls, [["w1", "wants-this"]], r.stderr);
  assert.ok(!r.lockLeft);
});

test("normal run releases the lock", () => {
  const r = run(lockScenario);
  assert.deepEqual(r.calls, [["w1", "wants-this"]], r.stderr);
  assert.ok(!r.lockLeft);
});

test("session id missing from registry: workspace skipped", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "someone-else", name: "irrelevant" }],
    world: {
      agents: [agent("not-in-registry", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  assert.deepEqual(r.calls, []);
});

test("missing root pane: safe skip, exit 0", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "wants-this" }],
    world: {
      agents: [agent("s1", "w1:p2", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: {}, // no p1 → pane get exits 1
    },
  });
  assert.deepEqual(r.calls, []);
  assert.equal(r.status, 0);
});

test("global sweep renames only eligible workspaces", () => {
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
  assert.deepEqual(r.calls, [["w1", "eligible-name"]], r.stderr);
});

test("rename reports ~-shortened dir metadata", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "my-cool-task" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
  });
  assert.deepEqual(r.calls, [["w1", "my-cool-task"]], r.stderr);
  assert.deepEqual(r.meta, [
    ["w1", "--source", "io.rlew.workspace-renamer", "--token", "dir=~/dev/notes"],
  ]);
});

test("in-sync owned workspace refreshes dir metadata without renaming", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "our-name" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "our-name")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
    state: { w1: "our-name" },
  });
  assert.deepEqual(r.calls, []);
  assert.deepEqual(r.meta, [
    ["w1", "--source", "io.rlew.workspace-renamer", "--token", "dir=~/dev/notes"],
  ]);
});

test("user override clears dir metadata", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "plugin-wants-this" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "user-chose-this")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
    state: { w1: "old-plugin-name" },
  });
  assert.deepEqual(r.calls, []);
  assert.deepEqual(r.meta, [
    ["w1", "--source", "io.rlew.workspace-renamer", "--clear-token", "dir"],
  ]);
});

test("untouched workspace gets no dir metadata", () => {
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes-27", nameSource: "derived" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "$HOME/dev/notes"),
    },
  });
  assert.deepEqual(r.meta, []);
});
