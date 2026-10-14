# herdr Workspace Renamer

A [herdr](https://herdr.dev) plugin that syncs agent session names onto herdr
workspace labels. When you name your session — `/rename` in Claude Code and
Codex alike — the containing workspace label follows. Until you do, the label
follows the agent's live terminal title (for Claude Code, the task summary it
keeps up to date as you work). A workspace that already has a non-default name
is left alone until you hand it back.

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
  any other way — is left alone, even if the session is renamed later, until
  you hand it back (see below).
- A session with no name you set (Claude Code's auto-derived `my-project-3f`,
  or an unnamed Codex thread) uses the agent's live terminal title instead,
  and the label follows the title as it changes. Placeholder titles that only
  name the product ("Claude Code", "claude: <dir>") are ignored, and titles
  longer than 32 characters are cut at a word with `…`. A name you set with
  `/rename` always wins over the title.
- In multi-agent workspaces only the primary agent (lowest-numbered agent pane
  of the first tab) drives the label; guest sessions are ignored.
- Every trigger event reconciles *all* workspaces, so missed events self-heal
  on the next one.

## Handing a workspace back

To let the plugin name a workspace you named yourself, either:

- rename the workspace to `?`, or
- run the **Hand workspace back to Workspace Renamer** action from that
  workspace (`herdr plugin action invoke io.rlew.workspace-renamer.reset`).

Either way the plugin syncs the workspace straight away: it takes the session
name if the primary agent has a user-named session, and otherwise goes back to
its default label (the root pane's folder name), where later session renames
will pick it up.

## How it works

The plugin is a single Rust binary, which herdr runs from
`target/release/herdr-workspace-renamer` on each event. On each run the plugin joins each agent's session registry against
`herdr agent list` (session id ↔ pane ↔ workspace), then renames a workspace
only when its current label is the default (basename of the root pane's cwd)
or the plugin's own last write, tracked in plugin state. herdr's root pane is the first
pane created in the workspace's first tab, and herdr doesn't report which one
that is once the original has closed, so a label matching the folder of any
pane in the first tab counts as the default.

Name resolution is provider-based: each agent type owns a small adapter that
maps a herdr agent record to the user's intended session name (or "no
opinion"). Current providers:

- **Claude Code** — reads `~/.claude/sessions/<pid>.json`; a session counts as
  user-named unless `nameSource` is `"derived"`.
- **Codex** — reads `~/.codex/session_index.jsonl`, which only contains
  explicitly named threads, so index presence is the user-named signal. Codex
  panes are joined via the session id that herdr's Codex SessionStart hook
  integration reports; panes the hook hasn't reported are left alone.

When the adapter has no user-set name, the primary agent's terminal title
(`terminal_title_stripped` in `herdr agent list`) is used instead.

Adding another agent means writing one adapter (a `Provider` in
`src/providers.rs`), with no changes to the reconcile core.

Overlapping runs are serialized by a lock file in the plugin state
directory, so two sweeps never race on the state file. A run that finds the
lock busy waits up to 3 seconds, and skips if a sweep that started after it
arrived has already covered its event.

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

## Sidebar: session name under a workspace you named

The reverse case: when you name a workspace yourself, the plugin leaves the
label alone, so the session name it would have used is not shown anywhere. For
those workspaces the plugin reports the primary agent's user-named session
name as the custom sidebar token `$session` (a live terminal title is never
shown here). It is cleared when it no longer
applies: the label equals the session name, the session is gone, or the
session has no user-set name. A workspace labelled after its session never
shows it twice.

Add `$session` next to `$dir`; each workspace shows at most one of them:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["$dir"],
  ["$session"],
  ["branch", "git_status"],
]
```

which renders a workspace you named `auth-spike` whose session is
`fix-login-redirect` as:

```
auth-spike
fix-login-redirect
main ✚2
```

## Install

```sh
herdr plugin install ryanlewis/herdr-workspace-renamer
```

Requires herdr ≥ 0.8.0. The install step downloads a prebuilt binary for your
platform when the plugin version has a release, and checks its SHA-256. If it
can't, it builds from source with cargo, which needs a Rust toolchain.

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

Needs a Rust toolchain (stable, edition 2024).

```sh
git clone https://github.com/ryanlewis/herdr-workspace-renamer
cd herdr-workspace-renamer
cargo build --release    # herdr runs target/release/herdr-workspace-renamer
herdr plugin link .      # and later: herdr plugin unlink io.rlew.workspace-renamer
```

Rebuild with `cargo build --release` after a change; the linked plugin
picks up the new binary on its next event.

Plugin state (a map of workspace id → last label the plugin wrote, used to
tell its own renames apart from yours) lives in
`~/.local/state/herdr/plugins/io.rlew.workspace-renamer/`. Deleting it is
safe: workspaces the plugin last renamed will just be treated as user-named
until they return to their default label or are handed back. Their `$dir`
sidebar token also stops being refreshed or cleared; if one lingers, remove it with
`herdr workspace report-metadata <ws> --source io.rlew.workspace-renamer --clear-token dir`.

### Releasing

Bump `version` in both `Cargo.toml` and `herdr-plugin.toml` (they must match),
commit, then tag `vX.Y.Z` and push the tag. The release workflow checks the tag
against both files, builds the five binaries, and publishes the release.

## Coexistence

Pairs with [herdr-auto-title](https://github.com/kryptamine/herdr-auto-title),
which names *tabs* and *panes*, while this plugin names *workspaces*. Leave
auto-title's `HERDR_AUTO_TITLE_WORKSPACES` option off (the default): when on,
it also renames one-tab workspaces, and the two plugins would compete for
those labels.

## Test

```sh
cargo test                 # offline: in-memory and fake herdr, fake session registries
cargo run -- --dry-run     # against live herdr state, prints planned renames
```

Most scenarios are unit tests that run a sweep against an in-memory herdr
(`src/sweep/tests.rs`). `tests/cli.rs` runs the built binary against a fake
herdr CLI (`tests/fake-herdr.sh`) for the command line, environment and
lock behaviour. Lock races are staged through the `LockFs` seam in
`src/lock.rs`.

`--dry-run` outside herdr reads the real plugin state directory (if present),
so the preview matches what event-driven runs would actually do — including
updates to workspaces the plugin already owns.

`--version` prints the version and exits without touching herdr or state.

## Caveats

Both name sources are undocumented internals (shapes observed on Claude Code
2.1.226 and Codex 0.145.0). If a future release changes either, the plugin
degrades to doing nothing rather than renaming wrongly. For Codex
specifically: if a future version starts auto-titling threads into
`session_index.jsonl`, auto names would start syncing — visible and
reversible, and the provider would then need a real discriminator.

## License

MIT
