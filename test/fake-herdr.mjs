#!/usr/bin/env node
// Fake herdr CLI for offline tests. Reads canned world state from
// $FAKE_HERDR_WORLD (JSON: {agents, workspaces, panes, tabs?}) and appends any
// `workspace rename` calls to $FAKE_HERDR_CALLS.
import { readFileSync, appendFileSync } from "node:fs";

// Only meaningful when spawned by the test harness; bail quietly if the
// node --test runner (or anything else) executes this file directly.
if (!process.env.FAKE_HERDR_WORLD) process.exit(0);

const world = JSON.parse(readFileSync(process.env.FAKE_HERDR_WORLD, "utf8"));
const [group, verb, ...rest] = process.argv.slice(2);

// world.panes is keyed by pane id; a pane without tab_id/workspace_id sits in
// its workspace's first tab.
const paneList = () =>
  Object.values(world.panes || {}).map((p) => {
    const wsId = p.pane_id.split(":")[0];
    return { workspace_id: wsId, tab_id: `${wsId}:t1`, ...p };
  });

if (group === "agent" && verb === "list") {
  process.stdout.write(
    JSON.stringify({ id: "cli:agent:list", result: { agents: world.agents, type: "agent_list" } }),
  );
} else if (group === "workspace" && verb === "list") {
  process.stdout.write(
    JSON.stringify({ id: "cli:workspace:list", result: { type: "workspace_list", workspaces: world.workspaces } }),
  );
} else if (group === "pane" && verb === "list") {
  process.stdout.write(
    JSON.stringify({ id: "cli:pane:list", result: { panes: paneList(), type: "pane_list" } }),
  );
} else if (group === "tab" && verb === "list") {
  // world.tabs (in display order) when a test needs it; otherwise one tab per
  // distinct tab_id among the panes, in pane order.
  const tabs =
    world.tabs ??
    [...new Map(paneList().map((p) => [p.tab_id, p.workspace_id]))].map(
      ([tab_id, workspace_id]) => ({ tab_id, workspace_id }),
    );
  process.stdout.write(JSON.stringify({ id: "cli:tab:list", result: { tabs, type: "tab_list" } }));
} else if (group === "pane" && verb === "get") {
  const pane = world.panes[rest[0]];
  if (!pane) {
    process.stderr.write("pane not found\n");
    process.exit(1);
  }
  process.stdout.write(
    JSON.stringify({ id: "cli:pane:get", result: { pane, type: "pane_info" } }),
  );
} else if (group === "workspace" && verb === "get") {
  const w = (world.workspaces || []).find((x) => x.workspace_id === rest[0]);
  if (!w) {
    process.stderr.write("workspace not found\n");
    process.exit(1);
  }
  // live_labels lets a test simulate a label changing between the sweep's
  // `workspace list` snapshot and its pre-rename `workspace get` re-check.
  const label = world.live_labels?.[rest[0]] ?? w.label;
  process.stdout.write(
    JSON.stringify({
      id: "cli:workspace:get",
      result: { type: "workspace_info", workspace: { ...w, label } },
    }),
  );
} else if (group === "workspace" && verb === "rename") {
  appendFileSync(process.env.FAKE_HERDR_CALLS, JSON.stringify(rest) + "\n");
  process.stdout.write(JSON.stringify({ id: "cli:workspace:rename", result: { ok: true } }));
} else if (group === "workspace" && verb === "report-metadata") {
  appendFileSync(process.env.FAKE_HERDR_META, JSON.stringify(rest) + "\n");
  // real herdr succeeds silently here — no stdout
} else {
  process.stderr.write(`fake-herdr: unknown command ${group} ${verb}\n`);
  process.exit(1);
}
