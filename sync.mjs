#!/usr/bin/env node
// Workspace Renamer — sync Claude Code session names onto herdr workspace labels.
// Global idempotent reconcile: every invocation sweeps all workspaces (event
// payload ignored). A workspace whose label isn't the default (or our own last
// write) is never touched — manual names win, permanently.
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
} from "node:fs";
import { spawnSync } from "node:child_process";
import { join, basename } from "node:path";
import { homedir } from "node:os";

const DRY = process.argv.includes("--dry-run");
const HERDR = process.env.HERDR_BIN_PATH || "herdr";
const STATE_DIR = process.env.HERDR_PLUGIN_STATE_DIR || null;
const REGISTRY_DIR = join(homedir(), ".claude", "sessions");
const DEBOUNCE_MS = 250;
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
  return JSON.parse(r.stdout);
}

// ~/.claude/sessions/<pid>.json — undocumented Claude Code internal (spec R1).
// Validate per file; anything surprising is treated as "no session info".
function readRegistry() {
  const bySessionId = new Map();
  let files;
  try {
    files = readdirSync(REGISTRY_DIR).filter((f) => f.endsWith(".json"));
  } catch {
    return bySessionId; // no registry → nothing to sync
  }
  for (const f of files) {
    try {
      const j = JSON.parse(readFileSync(join(REGISTRY_DIR, f), "utf8"));
      if (typeof j?.sessionId === "string" && typeof j?.name === "string") {
        bySessionId.set(j.sessionId, j);
      }
    } catch {
      // unparseable (mid-rewrite, stale junk) — skip silently
    }
  }
  return bySessionId;
}

// Trim, collapse whitespace, strip control chars, cap length. No re-slugging —
// the user typed what they want.
function clean(name) {
  return name
    .replace(/[\x00-\x1f\x7f]/g, "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, MAX_LABEL)
    .trim();
}

const paneNum = (id) => {
  const m = /:p(\d+)$/.exec(id ?? "");
  return m ? Number(m[1]) : Infinity;
};
const tabNum = (id) => {
  const m = /:t(\d+)$/.exec(id ?? "");
  return m ? Number(m[1]) : Infinity;
};

function readState() {
  if (!STATE_DIR) return {};
  try {
    const j = JSON.parse(readFileSync(join(STATE_DIR, "state.json"), "utf8"));
    return j && typeof j === "object" && !Array.isArray(j) ? j : {};
  } catch {
    return {};
  }
}

function writeState(state) {
  if (!STATE_DIR || DRY) return;
  try {
    mkdirSync(STATE_DIR, { recursive: true });
    writeFileSync(
      join(STATE_DIR, "state.json"),
      JSON.stringify(state, null, 2) + "\n",
    );
  } catch (e) {
    warn(`state write failed: ${e.message}`);
  }
}

// Events can burst (three subscriptions can fire off one user action) — skip if
// a full sweep ran within the last DEBOUNCE_MS.
function debounced() {
  if (!STATE_DIR || DRY) return false;
  const stamp = join(STATE_DIR, ".last-sweep");
  try {
    if (Date.now() - statSync(stamp).mtimeMs < DEBOUNCE_MS) return true;
  } catch {
    // no stamp yet
  }
  try {
    mkdirSync(STATE_DIR, { recursive: true });
    closeSync(openSync(stamp, "w"));
  } catch {
    // stamp write failing just means no debounce — harmless
  }
  return false;
}

function main() {
  if (debounced()) return;

  const registry = readRegistry();
  if (registry.size === 0) return;

  const agents = herdr("agent", "list")?.result?.agents;
  const workspaces = herdr("workspace", "list")?.result?.workspaces;
  if (!Array.isArray(agents) || !Array.isArray(workspaces)) {
    warn("unexpected herdr list output shape; no-op");
    return;
  }

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

  for (const ws of workspaces) {
    const wsId = ws?.workspace_id;
    const label = ws?.label;
    if (typeof wsId !== "string" || typeof label !== "string") continue;

    const wsAgents = agents.filter(
      (a) => a?.workspace_id === wsId && a?.agent_session?.value,
    );
    if (wsAgents.length === 0) continue;

    // FR5: primary = lowest-numbered agent pane of the first tab. Secondary
    // sessions never drive the label.
    const minTab = Math.min(...wsAgents.map((a) => tabNum(a.tab_id)));
    const primary = wsAgents
      .filter((a) => tabNum(a.tab_id) === minTab)
      .reduce((best, a) => (paneNum(a.pane_id) < paneNum(best.pane_id) ? a : best));

    const sess = registry.get(primary.agent_session.value);
    if (!sess) continue; // stale/foreign/non-claude — drops out of the join
    if (sess.nameSource === "derived") continue; // FR4: auto-names never rename
    const want = clean(sess.name);
    if (!want) continue;

    // FR2 guard: only touch a label that is the default (root-pane cwd
    // basename) or our own last write. Anything else → user owns it, forever.
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
        delete state[wsId]; // user overrode our write — locked from now on
        stateDirty = true;
      }
      continue;
    }

    if (want === label) continue; // FR6: no-op when equal, no rename loops

    if (DRY) {
      warn(`[dry-run] would rename ${wsId} "${label}" -> "${want}"`);
      continue;
    }
    try {
      herdr("workspace", "rename", wsId, want);
      state[wsId] = want;
      stateDirty = true;
      warn(`renamed ${wsId} "${label}" -> "${want}"`);
    } catch (e) {
      warn(`rename ${wsId} failed: ${e.message}`);
    }
  }

  if (stateDirty) writeState(state);
}

try {
  main();
} catch (e) {
  warn(e.message); // fail safe: any surprise is a no-op
}
