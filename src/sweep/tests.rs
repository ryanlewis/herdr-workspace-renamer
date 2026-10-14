//! Sweep scenarios against an in-memory herdr, real session registries in a
//! temporary home, and a real state directory. Each covers one rename,
//! guard or fail-safe behaviour.

use std::cell::RefCell;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use serde_json::{Value, json};

use super::{Options, run};
use crate::herdr::{Error, Herdr, MetaOp, Snapshot, Token};
use crate::providers::Registry;
use crate::state::StateStore;
use crate::test_support::{TempDir, take_stderr};

/// herdr as the tests see it. `world` holds `agents`, `workspaces`, `panes`
/// (a list; a pane without `tab_id` sits in its workspace's `t1`), optional
/// `tabs` (otherwise one per distinct pane tab, in pane order) and optional
/// `live_labels`, which change a label between the sweep's snapshot and its
/// pre-rename re-read.
struct FakeHerdr {
    world: Value,
    calls: RefCell<Vec<(String, String)>>,
    meta: RefCell<Vec<MetaOp>>,
}

impl FakeHerdr {
    fn list(&self, key: &str) -> Vec<Value> {
        self.world[key].as_array().cloned().unwrap_or_default()
    }

    fn panes(&self) -> Vec<Value> {
        self.list("panes")
            .into_iter()
            .map(|mut p| {
                let ws = p["pane_id"]
                    .as_str()
                    .and_then(|id| id.split(':').next())
                    .unwrap_or_default()
                    .to_owned();
                let obj = p.as_object_mut().expect("pane is an object");
                obj.entry("tab_id").or_insert(json!(format!("{ws}:t1")));
                obj.entry("workspace_id").or_insert(json!(ws));
                p
            })
            .collect()
    }

    fn tabs(&self, panes: &[Value]) -> Vec<Value> {
        if self.world.get("tabs").is_some() {
            return self.list("tabs");
        }
        let mut tabs: Vec<Value> = Vec::new();
        for p in panes {
            if !tabs.iter().any(|t| t["tab_id"] == p["tab_id"]) {
                tabs.push(json!({"tab_id": p["tab_id"], "workspace_id": p["workspace_id"]}));
            }
        }
        tabs
    }
}

fn reply(key: &str, items: Vec<Value>) -> Value {
    json!({"result": {key: items}})
}

impl Herdr for FakeHerdr {
    fn snapshot(&self) -> Result<Snapshot, Error> {
        let panes = self.panes();
        Snapshot::from_replies(
            Some(&json!({"result": {"agents": self.world["agents"]}})),
            Some(&reply("workspaces", self.list("workspaces"))),
            Some(&reply("tabs", self.tabs(&panes))),
            Some(&reply("panes", panes)),
        )
    }

    fn label(&self, workspace: &str) -> Result<Option<String>, Error> {
        if let Some(live) = self.world["live_labels"][workspace].as_str() {
            return Ok(Some(live.to_owned()));
        }
        self.list("workspaces")
            .iter()
            .find(|w| w["workspace_id"] == workspace)
            .map(|w| w["label"].as_str().map(str::to_owned))
            .ok_or_else(|| Error::Failed {
                command: format!("herdr workspace get {workspace}"),
                status: ExitStatus::from_raw(1 << 8),
                stderr: "workspace not found".into(),
            })
    }

    fn rename(&self, workspace: &str, label: &str) -> Result<(), Error> {
        self.calls
            .borrow_mut()
            .push((workspace.to_owned(), label.to_owned()));
        Ok(())
    }

    fn report_metadata(&self, op: &MetaOp) -> Result<(), Error> {
        self.meta.borrow_mut().push(op.clone());
        Ok(())
    }
}

#[derive(Default)]
struct Scenario {
    /// Claude Code session registry files.
    sessions: Vec<Value>,
    /// Codex index lines; strings are written verbatim.
    codex: Vec<Value>,
    world: Value,
    state: Option<Value>,
    dry: bool,
    reset: Option<&'static str>,
    /// A directory at state.json makes the atomic replace fail.
    state_unwritable: bool,
}

struct Outcome {
    calls: Vec<(String, String)>,
    meta: Vec<MetaOp>,
    state: Value,
    stderr: String,
}

impl Scenario {
    fn run(self) -> Outcome {
        let sandbox = TempDir::new();
        let home = sandbox.path().join("home");
        let sessions = home.join(".claude/sessions");
        fs::create_dir_all(&sessions).unwrap();
        for (i, s) in self.sessions.iter().enumerate() {
            fs::write(sessions.join(format!("{}.json", 1000 + i)), s.to_string()).unwrap();
        }
        if !self.codex.is_empty() {
            fs::create_dir_all(home.join(".codex")).unwrap();
            let lines: Vec<String> = self
                .codex
                .iter()
                .map(|l| l.as_str().map_or_else(|| l.to_string(), str::to_owned))
                .collect();
            fs::write(
                home.join(".codex/session_index.jsonl"),
                lines.join("\n") + "\n",
            )
            .unwrap();
        }
        let home_str = home.to_string_lossy().into_owned();
        // "$HOME" in fixture paths becomes the sandbox home, for `~` tests.
        let world = serde_json::from_str(&self.world.to_string().replace("$HOME", &home_str))
            .expect("world is JSON");
        let state_dir = sandbox.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        if let Some(state) = &self.state {
            fs::write(state_dir.join("state.json"), state.to_string()).unwrap();
        }
        if self.state_unwritable {
            fs::create_dir(state_dir.join("state.json")).unwrap();
        }

        let herdr = FakeHerdr {
            world,
            calls: RefCell::default(),
            meta: RefCell::default(),
        };
        let store = if self.dry {
            StateStore::read_only(Some(state_dir.clone()))
        } else {
            StateStore::new(state_dir.clone())
        };
        let opts = Options {
            home: &home_str,
            reset: self.reset,
            dry: self.dry,
        };
        take_stderr();
        run(&herdr, &store, &mut Registry::new(home.clone()), &opts);
        let state = fs::read_to_string(state_dir.join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({}));
        Outcome {
            calls: herdr.calls.into_inner(),
            meta: herdr.meta.into_inner(),
            state,
            stderr: take_stderr(),
        }
    }
}

fn agent(session: &str, pane: &str, tab: &str, ws: &str) -> Value {
    typed_agent(session, pane, tab, ws, "claude")
}

fn typed_agent(session: &str, pane: &str, tab: &str, ws: &str, kind: &str) -> Value {
    json!({
        "agent": kind,
        "agent_session": {"agent": kind, "kind": "id", "source": format!("herdr:{kind}"), "value": session},
        "agent_status": "idle",
        "pane_id": pane,
        "tab_id": tab,
        "workspace_id": ws,
    })
}

fn titled(title: &str, session: &str, pane: &str, tab: &str, ws: &str) -> Value {
    let mut a = agent(session, pane, tab, ws);
    a["terminal_title_stripped"] = json!(title);
    a
}

fn ws(id: &str, label: &str) -> Value {
    json!({"workspace_id": id, "label": label, "number": 1, "tab_count": 1, "pane_count": 1})
}

fn ws_tokens(id: &str, label: &str, tokens: Value) -> Value {
    let mut w = ws(id, label);
    w["tokens"] = tokens;
    w
}

fn root_pane(ws: &str, cwd: &str) -> Value {
    json!({"pane_id": format!("{ws}:p1"), "cwd": cwd})
}

fn pane(id: &str, cwd: &str, tab: &str) -> Value {
    json!({"pane_id": id, "cwd": cwd, "tab_id": tab})
}

fn session(id: &str, name: &str) -> Value {
    json!({"pid": 1, "sessionId": id, "name": name})
}

fn derived(id: &str, name: &str) -> Value {
    json!({"pid": 1, "sessionId": id, "name": name, "nameSource": "derived"})
}

/// One workspace `w1` labelled `label`, rooted in ~/dev/notes, holding
/// `agents`.
fn notes(label: &str, agents: Vec<Value>) -> Value {
    json!({
        "agents": agents,
        "workspaces": [ws("w1", label)],
        "panes": [root_pane("w1", "/Users/ryan/dev/notes")],
    })
}

fn renamed(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(w, l)| ((*w).to_owned(), (*l).to_owned()))
        .collect()
}

fn set(token: Token, value: &str) -> MetaOp {
    MetaOp::Set {
        workspace: "w1".into(),
        token,
        value: value.into(),
    }
}

fn clear(token: Token) -> MetaOp {
    MetaOp::Clear {
        workspace: "w1".into(),
        token,
    }
}

// ---- renames and the ownership guard -------------------------------------

#[test]
fn user_named_session_renames_default_labelled_workspace() {
    let r = Scenario {
        sessions: vec![session("s1", "my-cool-task")],
        world: notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "my-cool-task")]), "{}", r.stderr);
    assert_eq!(
        r.state["w1"], "my-cool-task",
        "rename recorded in plugin state"
    );
}

#[test]
fn re_rename_updates_a_label_the_plugin_set() {
    let r = Scenario {
        sessions: vec![session("s1", "second-name")],
        world: notes("first-name", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        state: Some(json!({"w1": "first-name"})),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "second-name")]), "{}", r.stderr);
}

#[test]
fn manually_named_workspace_never_touched_and_state_entry_dropped() {
    let r = Scenario {
        sessions: vec![session("s1", "plugin-wants-this")],
        world: notes("user-chose-this", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        state: Some(json!({"w1": "old-plugin-name"})),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert!(
        r.state.get("w1").is_none(),
        "state entry dropped when user overrides label"
    );
}

#[test]
fn derived_session_name_never_renames() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
}

#[test]
fn guest_session_rename_ignored() {
    let r = Scenario {
        sessions: vec![
            derived("primary", "prim-3f"),
            session("guest", "guest-renamed"),
        ],
        world: notes(
            "notes",
            vec![
                agent("primary", "w1:p1", "w1:t1", "w1"),
                agent("guest", "w1:p3", "w1:t1", "w1"),
            ],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
}

#[test]
fn primary_drives_the_label_regardless_of_agent_list_order() {
    let r = Scenario {
        sessions: vec![
            session("primary", "primary-name"),
            session("guest", "guest-name"),
        ],
        world: notes(
            "notes",
            vec![
                agent("guest", "w1:p3", "w1:t1", "w1"),
                agent("primary", "w1:p1", "w1:t1", "w1"),
            ],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "primary-name")]), "{}", r.stderr);
}

#[test]
fn no_op_when_label_already_matches() {
    let r = Scenario {
        sessions: vec![session("s1", "notes")],
        world: notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
}

#[test]
fn clean_collapses_whitespace_strips_controls_and_caps() {
    let r = Scenario {
        sessions: vec![session(
            "s1",
            &format!("  Fix\tthe   thing\x07 {}", "x".repeat(60)),
        )],
        world: notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls.len(), 1, "{}", r.stderr);
    let got = &r.calls[0].1;
    assert!(got.starts_with("Fix the thing"), "{got}");
    assert!(got.chars().count() <= 32, "{got}");
    assert!(!got.chars().any(|c| c.is_control()), "{got}");
}

#[test]
fn clean_caps_code_points() {
    let r = Scenario {
        sessions: vec![session("s1", &"🚀".repeat(40))],
        world: notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.calls,
        renamed(&[("w1", &"🚀".repeat(32))]),
        "{}",
        r.stderr
    );
}

#[test]
fn clean_control_char_between_spaces_collapses() {
    let r = Scenario {
        sessions: vec![session("s1", "fix \x07 thing")],
        world: notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "fix thing")]), "{}", r.stderr);
}

#[test]
fn primary_of_a_type_with_no_provider_skips_the_workspace() {
    let r = Scenario {
        sessions: vec![session("guest", "guest-renamed")],
        world: notes(
            "notes",
            vec![
                typed_agent("gemini-sess", "w1:p1", "w1:t1", "w1", "gemini"),
                agent("guest", "w1:p3", "w1:t1", "w1"),
            ],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn unjoined_primary_blocks_its_tab() {
    let mut no_session = typed_agent("x", "w1:p1", "w1:t1", "w1", "codex");
    no_session.as_object_mut().unwrap().remove("agent_session");
    let r = Scenario {
        sessions: vec![session("guest", "guest-renamed")],
        world: notes(
            "notes",
            vec![no_session, agent("guest", "w1:p3", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn unjoinable_first_tab_falls_through_to_the_next_tab() {
    let mut no_session = typed_agent("x", "w1:p1", "w1:t1", "w1", "codex");
    no_session.as_object_mut().unwrap().remove("agent_session");
    let r = Scenario {
        sessions: vec![session("cl-1", "tab-two-name")],
        world: notes(
            "notes",
            vec![no_session, agent("cl-1", "w1:p2", "w1:t2", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "tab-two-name")]), "{}", r.stderr);
}

#[test]
fn empty_session_id_never_joins() {
    let r = Scenario {
        codex: vec![
            json!({"id": "", "thread_name": "stray-name", "updated_at": "2026-08-08T21:40:59Z"}),
        ],
        world: notes(
            "notes",
            vec![typed_agent("", "w1:p1", "w1:t1", "w1", "codex")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn codex_named_thread_renames_default_labelled_workspace() {
    let r = Scenario {
        codex: vec![json!({"id": "cdx-1", "thread_name": "codex-task-name", "updated_at": "2026-08-08T21:40:59Z"})],
        world: notes(
            "notes",
            vec![typed_agent("cdx-1", "w1:p1", "w1:t1", "w1", "codex")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.calls,
        renamed(&[("w1", "codex-task-name")]),
        "{}",
        r.stderr
    );
}

#[test]
fn codex_unnamed_session_never_renames() {
    let r = Scenario {
        codex: vec![json!({"id": "other", "thread_name": "someone-else", "updated_at": "2026-08-08T21:40:59Z"})],
        world: notes(
            "notes",
            vec![typed_agent("cdx-unnamed", "w1:p1", "w1:t1", "w1", "codex")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn mixed_sweep_claude_and_codex_both_sync() {
    let r = Scenario {
        sessions: vec![session("cl-1", "claude-name")],
        codex: vec![json!({"id": "cdx-1", "thread_name": "codex-name", "updated_at": "2026-08-08T21:40:59Z"})],
        world: json!({
            "agents": [
                agent("cl-1", "w1:p1", "w1:t1", "w1"),
                typed_agent("cdx-1", "w2:p1", "w2:t1", "w2", "codex"),
            ],
            "workspaces": [ws("w1", "notes"), ws("w2", "notes")],
            "panes": [root_pane("w1", "/Users/ryan/dev/notes"), root_pane("w2", "/Users/ryan/dev/notes")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.calls,
        renamed(&[("w1", "claude-name"), ("w2", "codex-name")]),
        "{}",
        r.stderr
    );
}

#[test]
fn session_id_missing_from_registry_skips_the_workspace() {
    let r = Scenario {
        sessions: vec![session("someone-else", "irrelevant")],
        world: notes(
            "notes",
            vec![agent("not-in-registry", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
}

#[test]
fn no_panes_in_the_workspace_is_a_safe_skip() {
    let r = Scenario {
        sessions: vec![session("s1", "wants-this")],
        world: json!({
            "agents": [agent("s1", "w1:p2", "w1:t1", "w1")],
            "workspaces": [ws("w1", "notes")],
            "panes": [],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert_eq!(r.stderr, "");
}

#[test]
fn global_sweep_renames_only_eligible_workspaces() {
    let r = Scenario {
        sessions: vec![
            session("s1", "eligible-name"),
            session("s2", "blocked-name"),
        ],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1"), agent("s2", "w2:p1", "w2:t1", "w2")],
            "workspaces": [ws("w1", "notes"), ws("w2", "manual-label")],
            "panes": [root_pane("w1", "/Users/ryan/dev/notes"), root_pane("w2", "/Users/ryan/dev/notes")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "eligible-name")]), "{}", r.stderr);
}

#[test]
fn state_write_failure_aborts_all_renames() {
    let r = Scenario {
        sessions: vec![session("s1", "wants-this")],
        world: notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        state_unwritable: true,
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert!(r.stderr.contains("state write failed"), "{}", r.stderr);
}

#[test]
fn mid_sweep_manual_rename_wins() {
    let mut world = notes("notes", vec![agent("s1", "w1:p1", "w1:t1", "w1")]);
    world["live_labels"] = json!({"w1": "user-just-renamed"});
    let r = Scenario {
        sessions: vec![session("s1", "wants-this")],
        world,
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn unexpected_list_shape_is_a_no_op() {
    let r = Scenario {
        sessions: vec![session("s1", "wants-this")],
        world: json!({
            "agents": {"not": "a list"},
            "workspaces": [ws("w1", "notes")],
            "panes": [root_pane("w1", "/Users/ryan/dev/notes")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert_eq!(r.meta, []);
    assert_eq!(
        r.stderr,
        "workspace-renamer: unexpected herdr list output shape; no-op\n"
    );
}

// ---- dry run ---------------------------------------------------------------

#[test]
fn dry_run_reads_existing_state() {
    let r = Scenario {
        sessions: vec![session("s1", "second-name")],
        world: notes("first-name", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        state: Some(json!({"w1": "first-name"})),
        dry: true,
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert!(
        r.stderr
            .contains(r#"would rename w1 "first-name" -> "second-name""#),
        "{}",
        r.stderr
    );
    assert_eq!(r.state, json!({"w1": "first-name"}), "state untouched");
}

#[test]
fn dry_run_reports_no_session_metadata() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: notes("user-chose-this", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        dry: true,
        ..Default::default()
    }
    .run();
    assert_eq!(r.meta, []);
}

// ---- $dir ------------------------------------------------------------------

#[test]
fn rename_reports_tilde_shortened_dir() {
    let r = Scenario {
        sessions: vec![session("s1", "my-cool-task")],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1")],
            "workspaces": [ws("w1", "notes")],
            "panes": [root_pane("w1", "$HOME/dev/notes")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "my-cool-task")]), "{}", r.stderr);
    assert_eq!(r.meta, [set(Token::Dir, "~/dev/notes")]);
}

#[test]
fn in_sync_owned_workspace_refreshes_dir_without_renaming() {
    let r = Scenario {
        sessions: vec![session("s1", "our-name")],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1")],
            "workspaces": [ws("w1", "our-name")],
            "panes": [root_pane("w1", "$HOME/dev/notes")],
        }),
        state: Some(json!({"w1": "our-name"})),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert_eq!(r.meta, [set(Token::Dir, "~/dev/notes")]);
}

#[test]
fn user_override_clears_dir() {
    let r = Scenario {
        sessions: vec![session("s1", "plugin-wants-this")],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1")],
            "workspaces": [ws("w1", "user-chose-this")],
            "panes": [root_pane("w1", "$HOME/dev/notes")],
        }),
        state: Some(json!({"w1": "old-plugin-name"})),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert_eq!(
        r.meta,
        [clear(Token::Dir), set(Token::Session, "plugin-wants-this")]
    );
}

#[test]
fn untouched_workspace_gets_no_dir() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1")],
            "workspaces": [ws("w1", "notes")],
            "panes": [root_pane("w1", "$HOME/dev/notes")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.meta, []);
}

// ---- handing back ----------------------------------------------------------

#[test]
fn sentinel_label_takes_the_session_name_and_ownership() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1")],
            "workspaces": [ws("w1", "?")],
            "panes": [root_pane("w1", "$HOME/dev/notes")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "session-name")]), "{}", r.stderr);
    assert_eq!(r.state["w1"], "session-name");
    assert_eq!(r.meta, [set(Token::Dir, "~/dev/notes")]);
}

#[test]
fn sentinel_label_without_user_named_session_restores_default() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes("?", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        state: Some(json!({"w1": "old-plugin-name"})),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "notes")]), "{}", r.stderr);
    assert!(
        r.state.get("w1").is_none(),
        "default label needs no ownership record"
    );
    assert_eq!(r.meta, [clear(Token::Dir)]);
}

#[test]
fn sentinel_label_with_no_agent_restores_default() {
    let r = Scenario {
        world: notes("?", vec![]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "notes")]), "{}", r.stderr);
}

#[test]
fn session_named_sentinel_is_never_written_back() {
    let r = Scenario {
        sessions: vec![session("s1", " ? ")],
        world: notes("?", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "notes")]), "{}", r.stderr);
}

#[test]
fn sentinel_label_changed_again_mid_sweep_newer_name_wins() {
    let mut world = notes("?", vec![agent("s1", "w1:p1", "w1:t1", "w1")]);
    world["live_labels"] = json!({"w1": "user-changed-mind"});
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world,
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn sentinel_label_with_root_pane_at_slash_left_alone() {
    let r = Scenario {
        world: json!({
            "agents": [],
            "workspaces": [ws("w1", "?")],
            "panes": [root_pane("w1", "/")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

fn two_manual_workspaces() -> Value {
    json!({
        "agents": [agent("s1", "w1:p1", "w1:t1", "w1"), agent("s2", "w2:p1", "w2:t1", "w2")],
        "workspaces": [ws("w1", "user-chose-this"), ws("w2", "also-manual")],
        "panes": [root_pane("w1", "$HOME/dev/notes"), root_pane("w2", "$HOME/dev/other")],
    })
}

#[test]
fn reset_hands_back_only_the_invoking_workspace() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: two_manual_workspaces(),
        reset: Some("w1"),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "session-name")]), "{}", r.stderr);
    assert_eq!(r.state["w1"], "session-name");
    assert!(r.state.get("w2").is_none());
}

#[test]
fn reset_without_user_named_session_restores_default() {
    let r = Scenario {
        world: two_manual_workspaces(),
        reset: Some("w1"),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "notes")]), "{}", r.stderr);
}

#[test]
fn reset_claims_a_label_that_already_equals_the_session_name() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1")],
            "workspaces": [ws("w1", "session-name")],
            "panes": [root_pane("w1", "$HOME/dev/notes")],
        }),
        reset: Some("w1"),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
    assert_eq!(r.state["w1"], "session-name");
    assert_eq!(r.meta, [set(Token::Dir, "~/dev/notes")]);
}

#[test]
fn without_reset_manual_workspaces_stay_put() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: two_manual_workspaces(),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

// ---- $session --------------------------------------------------------------

#[test]
fn user_named_workspace_shows_the_session_name() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: notes("user-chose-this", vec![agent("s1", "w1:p1", "w1:t1", "w1")]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert_eq!(
        r.meta,
        [set(Token::Session, "session-name")],
        "{}",
        r.stderr
    );
}

fn with_tokens(label: &str, tokens: Value, agents: Vec<Value>) -> Value {
    json!({
        "agents": agents,
        "workspaces": [ws_tokens("w1", label, tokens)],
        "panes": [root_pane("w1", "$HOME/dev/notes")],
    })
}

#[test]
fn session_already_shown_needs_no_call() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: with_tokens(
            "user-chose-this",
            json!({"session": "session-name"}),
            vec![agent("s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.meta, [], "{}", r.stderr);
}

#[test]
fn session_follows_a_session_rename() {
    let r = Scenario {
        sessions: vec![session("s1", "new-name")],
        world: with_tokens(
            "user-chose-this",
            json!({"session": "old-name"}),
            vec![agent("s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.meta, [set(Token::Session, "new-name")], "{}", r.stderr);
}

#[test]
fn session_cleared_when_label_equals_session_name() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: with_tokens(
            "session-name",
            json!({"session": "session-name"}),
            vec![agent("s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert_eq!(r.meta, [clear(Token::Session)], "{}", r.stderr);
}

#[test]
fn session_cleared_when_no_longer_user_named() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: with_tokens(
            "user-chose-this",
            json!({"session": "old-name"}),
            vec![agent("s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.meta, [clear(Token::Session)], "{}", r.stderr);
}

#[test]
fn session_cleared_when_the_session_is_gone() {
    let r = Scenario {
        world: with_tokens("user-chose-this", json!({"session": "old-name"}), vec![]),
        ..Default::default()
    }
    .run();
    assert_eq!(r.meta, [clear(Token::Session)], "{}", r.stderr);
}

#[test]
fn session_cleared_when_the_plugin_renames_to_the_session() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: with_tokens(
            "notes",
            json!({"session": "stale"}),
            vec![agent("s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "session-name")]), "{}", r.stderr);
    assert_eq!(
        r.meta,
        [clear(Token::Session), set(Token::Dir, "~/dev/notes")]
    );
}

#[test]
fn hand_back_clears_session_as_the_workspace_takes_it() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: with_tokens(
            "?",
            json!({"session": "session-name"}),
            vec![agent("s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "session-name")]), "{}", r.stderr);
    assert_eq!(
        r.meta,
        [clear(Token::Session), set(Token::Dir, "~/dev/notes")]
    );
}

#[test]
fn hand_back_with_no_root_pane_keeps_session() {
    let r = Scenario {
        sessions: vec![session("s1", "session-name")],
        world: json!({
            "agents": [agent("s1", "w1:p1", "w1:t1", "w1")],
            "workspaces": [ws_tokens("w1", "?", json!({"session": "session-name"}))],
            "panes": [],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, []);
    assert_eq!(r.meta, [], "{}", r.stderr);
}

// ---- live terminal title fallback ------------------------------------------

#[test]
fn live_title_renames_default_labelled_workspace() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes(
            "notes",
            vec![titled("Fix login redirect", "s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.calls,
        renamed(&[("w1", "Fix login redirect")]),
        "{}",
        r.stderr
    );
    assert_eq!(
        r.state["w1"], "Fix login redirect",
        "title rename recorded as ours"
    );
}

#[test]
fn live_title_change_updates_a_label_the_plugin_set() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes(
            "First topic",
            vec![titled("Second topic", "s1", "w1:p1", "w1:t1", "w1")],
        ),
        state: Some(json!({"w1": "First topic"})),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "Second topic")]), "{}", r.stderr);
}

#[test]
fn user_set_session_name_beats_the_live_title() {
    let r = Scenario {
        sessions: vec![session("s1", "my-cool-task")],
        world: notes(
            "notes",
            vec![titled("Some summary", "s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "my-cool-task")]), "{}", r.stderr);
}

#[test]
fn product_name_placeholder_titles_never_rename() {
    for t in ["Claude Code", "claude", "claude: notes", "  "] {
        let r = Scenario {
            sessions: vec![derived("s1", "notes-27")],
            world: notes("notes", vec![titled(t, "s1", "w1:p1", "w1:t1", "w1")]),
            ..Default::default()
        }
        .run();
        assert_eq!(r.calls, [], "title {t:?}: {}", r.stderr);
    }
}

#[test]
fn summary_containing_the_agent_name_still_renames() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes(
            "notes",
            vec![titled(
                "Claude Code hooks setup",
                "s1",
                "w1:p1",
                "w1:t1",
                "w1",
            )],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.calls,
        renamed(&[("w1", "Claude Code hooks setup")]),
        "{}",
        r.stderr
    );
}

#[test]
fn long_live_title_cut_at_a_word_with_ellipsis() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes(
            "notes",
            vec![titled(
                "Ntfy arloo-estate-alerts JSON formatting",
                "s1",
                "w1:p1",
                "w1:t1",
                "w1",
            )],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.calls,
        renamed(&[("w1", "Ntfy arloo-estate-alerts JSON…")]),
        "{}",
        r.stderr
    );
}

#[test]
fn live_title_never_touches_a_user_named_workspace() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes(
            "user-chose-this",
            vec![titled("Some summary", "s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
    assert_eq!(r.meta, [], "a live title is never shown as $session");
}

#[test]
fn guest_live_title_ignored() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes(
            "notes",
            vec![
                agent("s1", "w1:p1", "w1:t1", "w1"),
                titled("Guest summary", "guest", "w1:p2", "w1:t1", "w1"),
            ],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn sentinel_label_with_only_a_live_title_hands_back_to_the_title() {
    let r = Scenario {
        sessions: vec![derived("s1", "notes-27")],
        world: notes(
            "?",
            vec![titled("Fix login redirect", "s1", "w1:p1", "w1:t1", "w1")],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(
        r.calls,
        renamed(&[("w1", "Fix login redirect")]),
        "{}",
        r.stderr
    );
    assert_eq!(r.state["w1"], "Fix login redirect");
}

// ---- root pane after the original p1 closed --------------------------------

#[test]
fn first_pane_closed_next_first_tab_pane_is_the_default() {
    let r = Scenario {
        sessions: vec![session("s1", "wants-this")],
        world: json!({
            "agents": [agent("s1", "w1:p2", "w1:t1", "w1")],
            "workspaces": [ws("w1", "beta")],
            "panes": [pane("w1:p2", "/Users/ryan/dev/beta", "w1:t1")],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "wants-this")]), "{}", r.stderr);
}

#[test]
fn first_tab_closed_new_first_tab_pane_is_the_root() {
    let r = Scenario {
        sessions: vec![session("s1", "wants-this")],
        world: json!({
            "agents": [agent("s1", "w1:p3", "w1:t2", "w1")],
            "workspaces": [ws("w1", "delta")],
            "panes": [
                pane("w1:p4", "/Users/ryan/dev/eps", "w1:t2"),
                pane("w1:p3", "/Users/ryan/dev/delta", "w1:t2"),
            ],
            "tabs": [{"tab_id": "w1:t2", "workspace_id": "w1"}],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "wants-this")]), "{}", r.stderr);
}

#[test]
fn later_tab_folder_never_makes_a_label_default() {
    let r = Scenario {
        sessions: vec![session("s1", "wants-this")],
        world: json!({
            "agents": [agent("s1", "w1:p2", "w1:t1", "w1")],
            "workspaces": [ws("w1", "floor")],
            "panes": [
                pane("w1:p2", "/Users/ryan/dev/notes", "w1:t1"),
                pane("w1:p7", "/Users/ryan/dev/floor", "w1:t3"),
            ],
            "tabs": [
                {"tab_id": "w1:t1", "workspace_id": "w1"},
                {"tab_id": "w1:t3", "workspace_id": "w1"},
            ],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, [], "{}", r.stderr);
}

#[test]
fn sentinel_label_with_p1_gone_restores_the_presumed_root() {
    let r = Scenario {
        world: json!({
            "agents": [],
            "workspaces": [ws("w1", "?")],
            "panes": [
                pane("w1:p3", "/Users/ryan/dev/gamma", "w1:t1"),
                pane("w1:p2", "/Users/ryan/dev/beta", "w1:t1"),
            ],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "beta")]), "{}", r.stderr);
}

// ---- herdr id order (bijective base 32) ------------------------------------

#[test]
fn pane_p0_is_pane_32_never_primary_over_p2() {
    let r = Scenario {
        sessions: vec![session("s2", "owner-name"), session("s32", "late-guest")],
        world: notes(
            "notes",
            vec![
                agent("s32", "w1:p0", "w1:t1", "w1"),
                agent("s2", "w1:p2", "w1:t1", "w1"),
            ],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "owner-name")]), "{}", r.stderr);
}

#[test]
fn pane_p11_is_pane_33_never_primary_over_pc() {
    let r = Scenario {
        sessions: vec![session("s12", "owner-name"), session("s33", "late-guest")],
        world: notes(
            "notes",
            vec![
                agent("s33", "w1:p11", "w1:t1", "w1"),
                agent("s12", "w1:pC", "w1:t1", "w1"),
            ],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "owner-name")]), "{}", r.stderr);
}

#[test]
fn lettered_tab_ids_order_correctly() {
    let r = Scenario {
        sessions: vec![session("sA", "first-tab"), session("sM", "later-tab")],
        world: notes(
            "notes",
            vec![
                agent("sM", "w1:p1", "w1:tM", "w1"),
                agent("sA", "w1:p9", "w1:tA", "w1"),
            ],
        ),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "first-tab")]), "{}", r.stderr);
}

#[test]
fn sentinel_label_presumed_root_is_oldest_pane_p2_before_p0() {
    let r = Scenario {
        world: json!({
            "agents": [],
            "workspaces": [ws("w1", "?")],
            "panes": [
                pane("w1:p0", "/Users/ryan/dev/late", "w1:t1"),
                pane("w1:p2", "/Users/ryan/dev/beta", "w1:t1"),
            ],
        }),
        ..Default::default()
    }
    .run();
    assert_eq!(r.calls, renamed(&[("w1", "beta")]), "{}", r.stderr);
}
