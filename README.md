# herdr Workspace Renamer

A [herdr](https://herdr.dev) plugin that syncs Claude Code session names onto
herdr workspace labels. When you `/rename` a session, the containing workspace
label follows — unless the workspace already has a non-default name, in which
case it is never touched. Manual names win, permanently.

## Why

Workspace labels default to the folder name of the root pane, so several
workspaces opened in the same directory all look alike in the sidebar. The
Claude session inside usually *has* a good name — this plugin makes the label
follow it.

## Behaviour

- Renaming a Claude session (`/rename`) in a default-labelled workspace renames
  the workspace to match.
- Renaming the session again updates a label the plugin itself set.
- A workspace you named yourself — manually, via `worktree create --label`, or
  any other way — is never touched, even if the session is renamed later.
- Auto-derived session names (`my-project-3f`) never rename anything.
- In multi-agent workspaces only the primary agent (lowest-numbered agent pane
  of the first tab) drives the label; guest sessions are ignored.
- Every trigger event reconciles *all* workspaces, so missed events self-heal
  on the next one.

## How it works

Claude Code keeps a per-process session registry (`~/.claude/sessions/<pid>.json`)
with each session's current name. On each herdr event the plugin joins that
registry against `herdr agent list` (session id ↔ pane ↔ workspace), then
renames a workspace only when its current label is the default (basename of the
root pane's cwd) or the plugin's own last write, tracked in plugin state.

Fail-safe by design: any parse or shape surprise is a silent no-op with one
line to stderr (visible via
`herdr plugin log list --plugin io.rlew.workspace-renamer`). Doing nothing is
always acceptable; a wrong rename is the only real failure.

## Install

```sh
git clone https://github.com/ryanlewis/herdr-workspace-renamer
cd herdr-workspace-renamer
herdr plugin link .
```

Requires herdr ≥ 0.8.0 and Node ≥ 18. Zero npm dependencies.

The plugin normally runs off herdr events, but you can force a sweep at any
time with the bundled workspace action:

```sh
herdr plugin action invoke io.rlew.workspace-renamer.sync-now
```

Plugin state (a map of workspace id → last label the plugin wrote, used to
tell its own renames apart from yours) lives in
`~/.local/state/herdr/plugins/io.rlew.workspace-renamer/`. Deleting it is
safe: workspaces the plugin last renamed will just be treated as user-named
until they return to their default label.

## Test

```sh
node test/run-tests.mjs   # offline: fake herdr CLI + fake session registry
node sync.mjs --dry-run   # against live herdr state, prints planned renames
```

## Caveats

The Claude Code session registry is an undocumented internal (shape observed on
2.1.226). If a future Claude Code release changes it, the plugin degrades to
doing nothing rather than renaming wrongly.
