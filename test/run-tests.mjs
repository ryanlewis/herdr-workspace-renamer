#!/usr/bin/env node
// Offline tests for sync.mjs: fake herdr CLI + fake ~/.claude/sessions
// registry, exercising each functional requirement from the spec.
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
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
function run({ sessions, world, state }) {
  const dir = mkdtempSync(join(tmpdir(), "wsren-test-"));
  const home = join(dir, "home");
  mkdirSync(join(home, ".claude", "sessions"), { recursive: true });
  sessions.forEach((s, i) =>
    writeFileSync(join(home, ".claude", "sessions", `${1000 + i}.json`), JSON.stringify(s)),
  );
  const worldPath = join(dir, "world.json");
  writeFileSync(worldPath, JSON.stringify(world));
  const callsPath = join(dir, "calls.log");
  writeFileSync(callsPath, "");
  const stateDir = join(dir, "state");
  mkdirSync(stateDir);
  if (state) writeFileSync(join(stateDir, "state.json"), JSON.stringify(state));

  const r = spawnSync(process.execPath, [syncScript], {
    encoding: "utf8",
    env: {
      ...process.env,
      HOME: home,
      HERDR_BIN_PATH: fakeHerdr,
      HERDR_PLUGIN_STATE_DIR: stateDir,
      FAKE_HERDR_WORLD: worldPath,
      FAKE_HERDR_CALLS: callsPath,
    },
  });

  const calls = readFileSync(callsPath, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
  let stateAfter = {};
  try {
    stateAfter = JSON.parse(readFileSync(join(stateDir, "state.json"), "utf8"));
  } catch {}
  rmSync(dir, { recursive: true, force: true });
  return { calls, stateAfter, stderr: r.stderr, status: r.status };
}

const ws = (id, label) => ({ workspace_id: id, label, number: 1, tab_count: 1, pane_count: 1 });
const rootPane = (wsId, cwd) => ({ [`${wsId}:p1`]: { pane_id: `${wsId}:p1`, cwd } });

// FR1: user-renamed session in default-labelled workspace → rename
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "my-cool-task" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("FR1 renames default-labelled workspace",
    r.calls.length === 1 && r.calls[0][0] === "w1" && r.calls[0][1] === "my-cool-task",
    JSON.stringify(r.calls) + r.stderr);
  check("FR1 records write in state", r.stateAfter.w1 === "my-cool-task");
}

// FR3: label == our last write → re-rename follows
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
  check("FR3 re-rename updates our own label",
    r.calls.length === 1 && r.calls[0][1] === "second-name",
    JSON.stringify(r.calls) + r.stderr);
}

// FR2/D2: manual label (≠ default, ≠ state) → untouched, state dropped
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
  check("FR2 manual label never touched", r.calls.length === 0, JSON.stringify(r.calls));
  check("FR2 stale state entry dropped", !("w1" in r.stateAfter), JSON.stringify(r.stateAfter));
}

// FR4: derived name → no-op
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes-27", nameSource: "derived" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("FR4 derived name never renames", r.calls.length === 0, JSON.stringify(r.calls));
}

// FR5: only the primary agent (lowest pane of first tab) drives the label
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
  check("FR5 secondary session ignored", r.calls.length === 0, JSON.stringify(r.calls));
}

// FR5 flip side: primary user-named wins even with a guest present
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
  check("FR5 primary drives the label (order-independent)",
    r.calls.length === 1 && r.calls[0][1] === "primary-name",
    JSON.stringify(r.calls) + r.stderr);
}

// FR6: want == label → no rename call at all
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "s1", name: "notes" }],
    world: {
      agents: [agent("s1", "w1:p1", "w1:t1", "w1")],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("FR6 no-op when label already matches", r.calls.length === 0, JSON.stringify(r.calls));
}

// D9: weird names get cleaned, not slugged
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
  check("D9 clean(): collapsed, control-stripped, capped at 32",
    r.calls.length === 1 && got.startsWith("Fix the thing") && got.length <= 32 && !/[\x00-\x1f\x7f]/.test(got),
    JSON.stringify(got));
}

// Provider seam: primary agent of a type with no provider → whole workspace
// skips, even when a user-named claude guest is present (type-blind FR5).
{
  const r = run({
    sessions: [{ pid: 1, sessionId: "guest", name: "guest-renamed" }],
    world: {
      agents: [
        agent("codex-sess", "w1:p1", "w1:t1", "w1", "codex"),
        agent("guest", "w1:p3", "w1:t1", "w1"),
      ],
      workspaces: [ws("w1", "notes")],
      panes: rootPane("w1", "/Users/ryan/dev/notes"),
    },
  });
  check("unmatched primary agent type → workspace skipped", r.calls.length === 0,
    JSON.stringify(r.calls) + r.stderr);
}

// D5/stale: session id not in registry → skip whole workspace
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

// R1 fail-safe: root pane get fails → skip, exit 0
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

console.log(failures ? `\n${failures} failure(s)` : "\nall tests passed");
process.exit(failures ? 1 : 0);
