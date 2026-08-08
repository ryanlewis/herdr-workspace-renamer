# herdr Workspace Renamer

A [herdr](https://github.com/ryanlewis) plugin that syncs Claude Code session
names onto herdr workspace labels. When you `/rename` a session, the containing
workspace label follows — unless the workspace already has a non-default name,
in which case it is never touched (manual names win, permanently).

Spec & design: `~/dev/notes/projects/herdr-workspace-renamer/spec.md`.

## How it works

Every subscribed event triggers a global idempotent reconcile (`sync.mjs`):

1. Read the Claude session registry (`~/.claude/sessions/<pid>.json` — `name`,
   `nameSource`) and join it against `herdr agent list` via
   `agent_session.value == sessionId` to map sessions to workspaces.
2. For each workspace, the **primary** agent (lowest-numbered agent pane of the
   first tab) drives the label; guest sessions are ignored.
3. Skip auto-derived session names (`nameSource: "derived"`).
4. Guard: only rename when the current label is the *default* (basename of the
   root pane's cwd) or the plugin's own last write (tracked in plugin state).
   Anything else means the user named it — locked forever.
5. `herdr workspace rename` only when the label would actually change.

Fail-safe: any parse or shape surprise is a silent no-op with one line to
stderr (visible in `herdr plugin log list --plugin io.rlew.workspace-renamer`).

## Install (dev)

```sh
herdr plugin link .
```

## Test

```sh
node test/run-tests.mjs   # offline: fake herdr + fake registry
node sync.mjs --dry-run   # against live herdr state, prints planned renames
```

Zero npm dependencies; plain Node ≥ 18.
