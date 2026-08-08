# herdr Workspace Renamer

A [herdr](https://herdr.dev) plugin that syncs agent session names onto herdr
workspace labels. When you name your session — `/rename` in Claude Code and
Codex alike — the containing workspace label follows, unless the workspace
already has a non-default name, in which case it is never touched. Manual
names win, permanently.

## Why

Workspace labels default to the folder name of the root pane, so several
workspaces opened in the same directory all look alike in the sidebar. The
agent session inside usually *has* a good name — this plugin makes the label
follow it.

## Behaviour

- Renaming a session (`/rename`) in a default-labelled workspace renames the
  workspace to match.
- Renaming the session again updates a label the plugin itself set.
- A workspace you named yourself — manually, via `worktree create --label`, or
  any other way — is never touched, even if the session is renamed later.
- Auto-derived session names (`my-project-3f`) never rename anything.
- In multi-agent workspaces only the primary agent (lowest-numbered agent pane
  of the first tab) drives the label; guest sessions are ignored.
- Every trigger event reconciles *all* workspaces, so missed events self-heal
  on the next one.

## How it works

On each herdr event the plugin joins each agent's session registry against
`herdr agent list` (session id ↔ pane ↔ workspace), then renames a workspace
only when its current label is the default (basename of the root pane's cwd)
or the plugin's own last write, tracked in plugin state.

Name resolution is provider-based: each agent type owns a small adapter that
maps a herdr agent record to the user's intended session name (or "no
opinion"). Current providers:

- **Claude Code** — reads `~/.claude/sessions/<pid>.json`; a session counts as
  user-named unless `nameSource` is `"derived"`.
- **Codex** — reads `~/.codex/session_index.jsonl`, which only contains
  explicitly named threads, so index presence is the user-named signal. Codex
  panes are joined via the session id that herdr's Codex SessionStart hook
  integration reports; panes the hook hasn't reported are left alone.

Adding another agent means writing one adapter, with no changes to the
reconcile core.

Fail-safe by design: any parse or shape surprise is a silent no-op with one
line to stderr (visible via
`herdr plugin log list --plugin io.rlew.workspace-renamer`). Doing nothing is
always acceptable; a wrong rename is the only real failure.

## Sidebar: directory under the workspace name

Once a workspace is renamed after a session, its label no longer says where
you are. For every workspace whose label it owns, the plugin also reports the
root pane's directory (`~`-shortened) as display-only workspace metadata under
the custom sidebar token `$dir` — refreshed on every sweep, cleared the moment
you rename the workspace yourself.

Rendering it is personal config: add `$dir` to the space rows in your herdr
`config.toml` (see [UI and sidebar](https://herdr.dev/docs/configuration/#ui-and-sidebar)).
With the git line kept from the default layout:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["$dir"],
  ["branch", "git_status"],
]
```

which renders renamed workspaces as:

```
herdr-plugin-mgr
~/dev/herdr-workspace-renamer
main ✚2
```

Workspaces the plugin hasn't renamed report no `$dir` token, and herdr
collapses the empty line — they render exactly as before.

## Install

```sh
herdr plugin install ryanlewis/herdr-workspace-renamer
```

Requires herdr ≥ 0.8.0 and Node ≥ 18. Zero npm dependencies.

To remove it:

```sh
herdr plugin uninstall io.rlew.workspace-renamer
```

The plugin normally runs off herdr events, but you can force a sweep at any
time with the bundled workspace action:

```sh
herdr plugin action invoke io.rlew.workspace-renamer.sync-now
```

### Development

```sh
git clone https://github.com/ryanlewis/herdr-workspace-renamer
cd herdr-workspace-renamer
herdr plugin link .      # and later: herdr plugin unlink io.rlew.workspace-renamer
```

Plugin state (a map of workspace id → last label the plugin wrote, used to
tell its own renames apart from yours) lives in
`~/.local/state/herdr/plugins/io.rlew.workspace-renamer/`. Deleting it is
safe: workspaces the plugin last renamed will just be treated as user-named
until they return to their default label. Their `$dir` sidebar token also
stops being refreshed or cleared; if one lingers, remove it with
`herdr workspace report-metadata <ws> --source io.rlew.workspace-renamer --clear-token dir`.

## Test

```sh
node test/sync.test.mjs   # offline: fake herdr CLI + fake session registries
node sync.mjs --dry-run   # against live herdr state, prints planned renames
```

Tests use the built-in `node:test` runner — Node ≥ 20 for development. (The
test file is run directly rather than via `node --test`: glob arguments need
Node ≥ 21, and this repo's `test/` directory name collides with the runner's
default discovery patterns.)

## Caveats

Both name sources are undocumented internals (shapes observed on Claude Code
2.1.226 and Codex 0.145.0). If a future release changes either, the plugin
degrades to doing nothing rather than renaming wrongly. For Codex
specifically: if a future version starts auto-titling threads into
`session_index.jsonl`, auto names would start syncing — visible and
reversible, and the provider would then need a real discriminator.

## License

MIT
