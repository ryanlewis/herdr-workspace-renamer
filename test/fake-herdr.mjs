#!/usr/bin/env node
// Fake herdr CLI for offline tests. Reads canned world state from
// $FAKE_HERDR_WORLD (JSON: {agents, workspaces, panes}) and appends any
// `workspace rename` calls to $FAKE_HERDR_CALLS.
import { readFileSync, appendFileSync } from "node:fs";

const world = JSON.parse(readFileSync(process.env.FAKE_HERDR_WORLD, "utf8"));
const [group, verb, ...rest] = process.argv.slice(2);

if (group === "agent" && verb === "list") {
  process.stdout.write(
    JSON.stringify({ id: "cli:agent:list", result: { agents: world.agents, type: "agent_list" } }),
  );
} else if (group === "workspace" && verb === "list") {
  process.stdout.write(
    JSON.stringify({ id: "cli:workspace:list", result: { type: "workspace_list", workspaces: world.workspaces } }),
  );
} else if (group === "pane" && verb === "get") {
  const pane = world.panes[rest[0]];
  if (!pane) {
    process.stderr.write("pane not found\n");
    process.exit(1);
  }
  process.stdout.write(
    JSON.stringify({ id: "cli:pane:get", result: { pane, type: "pane_info" } }),
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
