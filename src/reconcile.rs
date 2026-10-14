//! The reconcile core: from a herdr snapshot, the saved state and the
//! agents' session names, decide what the sweep should change. Pure apart
//! from the name lookups; [`crate::sweep`] carries the plan out.

use crate::herdr::{Agent, MetaOp, Snapshot, Token, Workspace};
use crate::providers::SessionNames;
use crate::state::State;
use crate::text::{basename, clean, creation_rank, live_title, tildify};

/// Renaming a workspace to this label hands it back to the plugin.
pub const SENTINEL: &str = "?";

/// What one sweep should do.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Metadata changes, issued first.
    pub metadata: Vec<MetaOp>,
    /// The state to save before any rename, when `persist` is set.
    pub state: State,
    pub persist: bool,
    /// Renames, issued only once `state` is saved.
    pub renames: Vec<Rename>,
}

/// One planned rename. `to` may equal `from` when a hand-back only claims a
/// label that already matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub workspace: String,
    /// The label seen in the snapshot; the rename is dropped if the live
    /// label no longer matches it.
    pub from: String,
    pub to: String,
    /// The root pane's directory, `~`-shortened, for the `$dir` token.
    pub dir: String,
    /// Whether the plugin owns the label afterwards. A hand-back to the
    /// default label doesn't: the default is always eligible.
    pub own: bool,
}

impl Rename {
    /// The `$dir` change that follows the rename.
    pub fn dir_op(&self) -> MetaOp {
        let workspace = self.workspace.clone();
        if self.own {
            MetaOp::Set {
                workspace,
                token: Token::Dir,
                value: self.dir.clone(),
            }
        } else {
            MetaOp::Clear {
                workspace,
                token: Token::Dir,
            }
        }
    }
}

/// The name a workspace's primary agent would give it.
struct Found {
    name: String,
    /// Set by the user, as opposed to a live terminal title.
    user_named: bool,
}

pub fn plan(
    snap: &Snapshot,
    mut state: State,
    names: &mut dyn SessionNames,
    home: &str,
    reset: Option<&str>,
) -> Plan {
    let mut metadata = Vec::new();
    let mut renames = Vec::new();
    let mut dirty = state.retain_live(|id| {
        snap.workspaces
            .iter()
            .any(|w| w.workspace_id.as_deref() == Some(id))
    });

    for ws in &snap.workspaces {
        let (Some(id), Some(label)) = (ws.workspace_id.as_deref(), ws.label.as_deref()) else {
            continue;
        };
        // Hand-back: a `?` label or the reset action makes this workspace
        // unclaimed again, whatever its label says.
        let hand_back = label == SENTINEL || reset == Some(id);
        // Never write the sentinel back.
        let found = session_name(snap, id, names).filter(|f| f.name != SENTINEL);
        let roots = root_candidates(snap, id);
        // A label matching any candidate's folder is the default. Otherwise
        // the presumed root (the first candidate) stands in, for `$dir` and
        // the folder a hand-back restores.
        let default_root = roots.iter().copied().find(|cwd| basename(cwd) == label);
        let root = default_root.or(roots.first().copied());
        // `$session` is shown only for a user-named workspace with a session
        // name it isn't carrying (the guard branch below); every other path
        // clears it.
        let Some(found) = found else {
            metadata.extend(session_op(ws, id, None));
            if hand_back && let Some(root) = root {
                plan_hand_back(id, label, None, root, &state, home, &mut renames);
            }
            continue;
        };
        // No root pane yet: leave the workspace as it is, `$session` too.
        let Some(root) = root else {
            continue;
        };
        if hand_back {
            metadata.extend(session_op(ws, id, None));
            plan_hand_back(
                id,
                label,
                Some(found.name),
                root,
                &state,
                home,
                &mut renames,
            );
            continue;
        }

        let owned = state.owns(id, label);

        // Ownership guard: only touch a label that is the default or the
        // plugin's own last write. Anything else means the user named this
        // workspace, and their choice stands until they hand it back.
        if default_root.is_none() && !owned {
            if state.release(id) {
                // The user overrode our write: locked from now on, and our
                // `$dir` row retires too.
                dirty = true;
                metadata.push(MetaOp::Clear {
                    workspace: id.to_owned(),
                    token: Token::Dir,
                });
            }
            // Only a name the user set is worth a `$session` row (a live
            // title would just repeat the terminal), and never one the label
            // already shows.
            let row = (found.user_named && found.name != label).then_some(found.name.as_str());
            metadata.extend(session_op(ws, id, row));
            continue;
        }

        // From here the label is (or is about to be) the session name.
        metadata.extend(session_op(ws, id, None));
        let dir = tildify(root, home);
        if found.name == label {
            // Already in sync: no rename (this also breaks any rename
            // feedback loop). If the label is ours, keep `$dir` alive.
            if owned {
                metadata.push(MetaOp::Set {
                    workspace: id.to_owned(),
                    token: Token::Dir,
                    value: dir,
                });
            }
            continue;
        }
        renames.push(Rename {
            workspace: id.to_owned(),
            from: label.to_owned(),
            to: found.name,
            dir,
            own: true,
        });
    }

    // Record intent before renaming: if a rename landed but the state write
    // didn't, the next sweep would misread our own label as user-named and
    // lock the workspace out permanently. The reverse failure (intent
    // recorded, rename lost) is harmless: the label stays default-eligible
    // and heals on the next sweep.
    for r in &renames {
        if r.own {
            state.claim(&r.workspace, &r.to);
        } else {
            state.release(&r.workspace);
        }
    }
    Plan {
        metadata,
        persist: dirty || !renames.is_empty(),
        state,
        renames,
    }
}

/// A handed-back workspace takes the session name if there is one, and
/// otherwise goes back to its default label, the folder of `root`.
fn plan_hand_back(
    id: &str,
    label: &str,
    want: Option<String>,
    root: &str,
    state: &State,
    home: &str,
    renames: &mut Vec<Rename>,
) {
    let own = want.is_some();
    let to = want.unwrap_or_else(|| basename(root).to_owned());
    // A root pane at "/" has no folder name to restore.
    if to.is_empty() || (!own && to == label && !state.contains(id)) {
        return;
    }
    renames.push(Rename {
        workspace: id.to_owned(),
        from: label.to_owned(),
        to,
        dir: tildify(root, home),
        own,
    });
}

/// The `$session` change, if any, that makes the workspace show `name` (or
/// nothing). Only differences cost a herdr call, so a quiet sweep makes none.
fn session_op(ws: &Workspace, id: &str, name: Option<&str>) -> Option<MetaOp> {
    let shown = ws.token("session");
    match name {
        Some(name) if shown.and_then(|v| v.as_str()) != Some(name) => Some(MetaOp::Set {
            workspace: id.to_owned(),
            token: Token::Session,
            value: name.to_owned(),
        }),
        None if shown.is_some() => Some(MetaOp::Clear {
            workspace: id.to_owned(),
            token: Token::Session,
        }),
        _ => None,
    }
}

/// The name the workspace's primary agent should give it. A user-set
/// session name wins; otherwise the agent's live terminal title (for Claude
/// Code, the task summary it keeps up to date) stands in.
fn session_name(snap: &Snapshot, id: &str, names: &mut dyn SessionNames) -> Option<Found> {
    let agents: Vec<&Agent> = snap
        .agents
        .iter()
        .filter(|a| a.workspace_id.as_deref() == Some(id))
        .collect();
    let primary = primary_agent(&agents)?;
    let session = primary.session()?;
    let kind = primary.agent.as_deref();
    let user_name = kind
        .and_then(|kind| names.user_name(kind, session))
        .map(|name| clean(&name))
        .filter(|name| !name.is_empty());
    if let Some(name) = user_name {
        return Some(Found {
            name,
            user_named: true,
        });
    }
    let title = live_title(kind?, primary.terminal_title_stripped.as_deref()?)?;
    Some(Found {
        name: title,
        user_named: false,
    })
}

fn tab_rank(a: &Agent) -> u64 {
    creation_rank(a.tab_id.as_deref(), 't')
}

fn pane_rank(id: Option<&str>) -> u64 {
    creation_rank(id, 'p')
}

/// The agent that drives the label; guest sessions are ignored. The primary
/// tab is the oldest tab holding a joined agent (a tab herdr couldn't join
/// at all doesn't block the workspace), but within that tab the oldest agent
/// pane wins even if unjoined: a sibling pane must never speak for the tab's
/// real owner. On a pane tie, prefer the joined record. `None` when the
/// primary has no session.
fn primary_agent<'a>(agents: &[&'a Agent]) -> Option<&'a Agent> {
    let first_tab = agents
        .iter()
        .filter(|a| a.session().is_some())
        .map(|a| tab_rank(a))
        .min()?;
    agents
        .iter()
        .copied()
        .filter(|a| tab_rank(a) == first_tab)
        .reduce(|best, a| {
            let (pa, pb) = (
                pane_rank(a.pane_id.as_deref()),
                pane_rank(best.pane_id.as_deref()),
            );
            if pa < pb || (pa == pb && best.session().is_none() && a.session().is_some()) {
                a
            } else {
                best
            }
        })
        .filter(|a| a.session().is_some())
}

/// The directories of the panes that could be the workspace's root, presumed
/// root first. herdr labels a workspace after its root pane's folder: the
/// first pane created in its first tab, which takes over when the old first
/// tab closes. herdr doesn't say which pane that is, and once the original
/// `p1` has closed it can be any pane of the first tab, so every pane there
/// is a candidate. Panes are tried in creation order, so the first is the
/// root unless panes have moved between tabs.
fn root_candidates<'a>(snap: &'a Snapshot, id: &str) -> Vec<&'a str> {
    let first_tab = snap
        .tabs
        .iter()
        .find(|t| t.workspace_id.as_deref() == Some(id))
        .and_then(|t| t.tab_id.as_deref());
    let Some(first_tab) = first_tab else {
        return Vec::new();
    };
    let mut panes: Vec<_> = snap
        .panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(first_tab))
        .filter_map(|p| Some((pane_rank(p.pane_id.as_deref()), p.cwd.as_deref()?)))
        .filter(|(_, cwd)| !cwd.is_empty())
        .collect();
    panes.sort_by_key(|&(rank, _)| rank);
    panes.into_iter().map(|(_, cwd)| cwd).collect()
}
