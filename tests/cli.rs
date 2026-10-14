//! End-to-end tests: the built binary against a fake herdr CLI
//! (tests/fake-herdr.sh), covering what only the whole process shows: the
//! command line, environment, lock and the exact herdr arguments.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-workspace-renamer");
const FAKE_HERDR: &str = include_str!("fake-herdr.sh");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// A sandbox whose herdr has one workspace `w1` labelled `label`, rooted
    /// in $HOME/dev/notes, with a Claude Code agent in session `s1`, named
    /// `session_name` by the user.
    fn new(label: &str, session_name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "wsren-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let sb = Self { root };
        for dir in ["home/.claude/sessions", "herdr", "state"] {
            fs::create_dir_all(sb.root.join(dir)).unwrap();
        }
        let fake = sb.root.join("herdr/herdr");
        fs::write(&fake, FAKE_HERDR).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            sb.root.join("home/.claude/sessions/1000.json"),
            json!({"pid": 1, "sessionId": "s1", "name": session_name}).to_string(),
        )
        .unwrap();

        let home = sb.home();
        let ws = json!({"workspace_id": "w1", "label": label});
        let replies = [
            (
                "agent-list",
                json!({"result": {"agents": [{
                    "agent": "claude",
                    "agent_session": {"value": "s1"},
                    "pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1",
                }]}}),
            ),
            ("workspace-list", json!({"result": {"workspaces": [ws]}})),
            (
                "tab-list",
                json!({"result": {"tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}]}}),
            ),
            (
                "pane-list",
                json!({"result": {"panes": [{
                    "pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1",
                    "cwd": format!("{home}/dev/notes"),
                }]}}),
            ),
            ("get-w1", json!({"result": {"workspace": ws}})),
        ];
        for (name, reply) in replies {
            fs::write(
                sb.root.join(format!("herdr/{name}.json")),
                reply.to_string(),
            )
            .unwrap();
        }
        sb
    }

    fn home(&self) -> String {
        self.root.join("home").to_string_lossy().into_owned()
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("HERDR_BIN_PATH", self.root.join("herdr/herdr"))
            .env("HERDR_PLUGIN_STATE_DIR", self.state_dir())
            .env("FAKE_HERDR_DIR", self.root.join("herdr"));
        cmd
    }

    fn run(&self, cmd: &mut Command) -> Outcome {
        let out = cmd.output().expect("run the binary");
        Outcome {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            calls: self.log("calls.log"),
            meta: self.log("meta.log"),
            state: fs::read_to_string(self.state_dir().join("state.json")).ok(),
            lock: fs::read_to_string(self.state_dir().join(".lock")).ok(),
        }
    }

    fn log(&self, name: &str) -> Vec<Vec<String>> {
        let text = fs::read_to_string(self.root.join("herdr").join(name)).unwrap_or_default();
        text.split('\u{1e}')
            .filter(|call| !call.is_empty())
            .map(|call| {
                call.split('\u{1f}')
                    .filter(|a| !a.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    }

    /// A lock owned by pid 99999, last refreshed `age` ago.
    fn foreign_lock(&self, age: Duration) {
        let path = self.state_dir().join(".lock");
        fs::write(&path, "99999").unwrap();
        set_mtime(&path, SystemTime::now() - age);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn set_mtime(path: &Path, t: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

#[derive(Debug)]
struct Outcome {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    calls: Vec<Vec<String>>,
    meta: Vec<Vec<String>>,
    state: Option<String>,
    lock: Option<String>,
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn version_prints_and_touches_nothing() {
    let sb = Sandbox::new("notes", "wants-this");
    let r = sb.run(
        sb.command(&["--version"])
            .env("HERDR_BIN_PATH", "/nonexistent"),
    );
    assert_eq!(r.status, Some(0));
    assert_eq!(r.stdout, "herdr-workspace-renamer 0.1.0\n");
    assert_eq!(r.stderr, "");
    assert_eq!(r.calls, Vec::<Vec<String>>::new());
    assert!(
        fs::read_dir(sb.state_dir()).unwrap().next().is_none(),
        "no state written"
    );
}

#[test]
fn sweep_renames_records_state_and_releases_the_lock() {
    let sb = Sandbox::new("notes", "my-cool-task");
    let r = sb.run(&mut sb.command(&[]));
    assert_eq!(r.status, Some(0), "{}", r.stderr);
    assert_eq!(r.calls, [args(&["w1", "my-cool-task"])]);
    assert_eq!(
        r.meta,
        [args(&[
            "w1",
            "--source",
            "io.rlew.workspace-renamer",
            "--token",
            "dir=~/dev/notes"
        ])]
    );
    assert_eq!(
        r.state.as_deref(),
        Some("{\n  \"w1\": \"my-cool-task\"\n}\n")
    );
    assert_eq!(r.lock, None);
    assert_eq!(
        r.stderr,
        "workspace-renamer: renamed w1 \"notes\" -> \"my-cool-task\"\n"
    );
}

#[test]
fn user_override_clears_dir_then_shows_session() {
    let sb = Sandbox::new("user-chose-this", "plugin-wants-this");
    fs::write(
        sb.state_dir().join("state.json"),
        r#"{"w1": "old-plugin-name"}"#,
    )
    .unwrap();
    let r = sb.run(&mut sb.command(&[]));
    assert_eq!(r.calls, Vec::<Vec<String>>::new());
    assert_eq!(
        r.meta,
        [
            args(&[
                "w1",
                "--source",
                "io.rlew.workspace-renamer",
                "--clear-token",
                "dir"
            ]),
            args(&[
                "w1",
                "--source",
                "io.rlew.workspace-renamer",
                "--token",
                "session=plugin-wants-this"
            ]),
        ]
    );
    assert_eq!(r.state.as_deref(), Some("{}\n"));
}

#[test]
fn no_state_dir_refuses_and_renames_nothing() {
    let sb = Sandbox::new("notes", "wants-this");
    let r = sb.run(sb.command(&[]).env_remove("HERDR_PLUGIN_STATE_DIR"));
    assert_eq!(r.status, Some(0));
    assert_eq!(r.calls, Vec::<Vec<String>>::new());
    assert!(r.stderr.contains("refusing to rename"), "{}", r.stderr);
}

#[test]
fn dry_run_previews_without_changes() {
    let sb = Sandbox::new("first-name", "second-name");
    fs::write(sb.state_dir().join("state.json"), r#"{"w1": "first-name"}"#).unwrap();
    let r = sb.run(&mut sb.command(&["--dry-run"]));
    assert_eq!(r.status, Some(0));
    assert_eq!(r.calls, Vec::<Vec<String>>::new());
    assert_eq!(r.meta, Vec::<Vec<String>>::new());
    assert_eq!(
        r.stderr,
        "workspace-renamer: [dry-run] would rename w1 \"first-name\" -> \"second-name\"\n"
    );
    assert_eq!(r.state.as_deref(), Some(r#"{"w1": "first-name"}"#));
}

#[test]
fn dry_run_outside_herdr_reads_the_default_state_dir() {
    let sb = Sandbox::new("first-name", "second-name");
    let default =
        Path::new(&sb.home()).join(".local/state/herdr/plugins/io.rlew.workspace-renamer");
    fs::create_dir_all(&default).unwrap();
    fs::write(default.join("state.json"), r#"{"w1": "first-name"}"#).unwrap();
    let r = sb.run(
        sb.command(&["--dry-run"])
            .env_remove("HERDR_PLUGIN_STATE_DIR"),
    );
    assert!(r.stderr.contains("would rename w1"), "{}", r.stderr);
}

#[test]
fn reset_flag_hands_back_the_invoking_workspace() {
    let sb = Sandbox::new("user-chose-this", "session-name");
    let r = sb.run(sb.command(&["--reset"]).env("HERDR_WORKSPACE_ID", "w1"));
    assert_eq!(r.calls, [args(&["w1", "session-name"])], "{}", r.stderr);
}

#[test]
fn event_hooks_carrying_workspace_id_never_reset() {
    let sb = Sandbox::new("user-chose-this", "session-name");
    let r = sb.run(sb.command(&[]).env("HERDR_WORKSPACE_ID", "w1"));
    assert_eq!(r.calls, Vec::<Vec<String>>::new(), "{}", r.stderr);
}

#[test]
fn reset_without_workspace_id_says_so_and_sweeps() {
    let sb = Sandbox::new("notes", "wants-this");
    let r = sb.run(&mut sb.command(&["--reset"]));
    assert!(
        r.stderr.contains("reset: no invoking workspace"),
        "{}",
        r.stderr
    );
    assert_eq!(r.calls, [args(&["w1", "wants-this"])]);
}

#[test]
fn reset_runs_even_when_a_newer_sweep_already_started() {
    let sb = Sandbox::new("user-chose-this", "session-name");
    let stamp = sb.state_dir().join(".last-sweep");
    fs::write(&stamp, "").unwrap();
    set_mtime(&stamp, SystemTime::now() + Duration::from_secs(60));
    let r = sb.run(sb.command(&["--reset"]).env("HERDR_WORKSPACE_ID", "w1"));
    assert_eq!(r.calls, [args(&["w1", "session-name"])], "{}", r.stderr);
}

#[test]
fn fresh_lock_held_by_another_sweep_skips_and_keeps_it() {
    let sb = Sandbox::new("notes", "wants-this");
    sb.foreign_lock(Duration::ZERO);
    let r = sb.run(&mut sb.command(&[]));
    assert_eq!(r.status, Some(0));
    assert_eq!(r.calls, Vec::<Vec<String>>::new());
    assert_eq!(
        r.lock.as_deref(),
        Some("99999"),
        "foreign lock must not be removed"
    );
}

#[test]
fn reset_blocked_by_a_busy_lock_gives_up_with_one_line() {
    let sb = Sandbox::new("user-chose-this", "session-name");
    sb.foreign_lock(Duration::ZERO);
    let r = sb.run(sb.command(&["--reset"]).env("HERDR_WORKSPACE_ID", "w1"));
    assert_eq!(r.status, Some(0));
    assert_eq!(r.calls, Vec::<Vec<String>>::new());
    assert_eq!(
        r.stderr,
        "workspace-renamer: reset of w1 skipped: another sweep held the lock\n"
    );
}

#[test]
fn stale_lock_taken_over_sweep_proceeds() {
    let sb = Sandbox::new("notes", "wants-this");
    sb.foreign_lock(Duration::from_secs(60));
    let r = sb.run(&mut sb.command(&[]));
    assert_eq!(r.calls, [args(&["w1", "wants-this"])], "{}", r.stderr);
    assert_eq!(r.lock, None);
}

#[test]
fn failing_herdr_is_a_no_op_with_one_line() {
    let sb = Sandbox::new("notes", "wants-this");
    fs::remove_file(sb.root.join("herdr/agent-list.json")).unwrap();
    let r = sb.run(&mut sb.command(&[]));
    assert_eq!(r.status, Some(0));
    assert_eq!(r.calls, Vec::<Vec<String>>::new());
    assert_eq!(r.stderr.lines().count(), 1, "{}", r.stderr);
    assert!(
        r.stderr
            .starts_with("workspace-renamer: herdr agent list: exit 1"),
        "{}",
        r.stderr
    );
    assert_eq!(r.lock, None);
}
