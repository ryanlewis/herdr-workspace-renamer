//! Workspace Renamer: sync agent session names onto herdr workspace labels.
//!
//! Every invocation sweeps all workspaces (the event payload is ignored), so
//! a missed event self-heals on the next one. A workspace whose label isn't
//! the default (or the plugin's own last write) is never touched: manual
//! names win until the user hands the workspace back (`?` label or the reset
//! action).
//!
//! Fail-safe posture: any parse or shape surprise is a skip plus one line on
//! stderr. Doing nothing is always acceptable; a wrong rename is the only
//! real failure.

mod herdr;
mod json;
mod lock;
mod providers;
mod reconcile;
mod state;
mod sweep;
mod text;

use std::env;
use std::ffi::OsString;
use std::fmt::Display;
use std::io::{self, Write};
use std::path::PathBuf;

use herdr::Cli;
use lock::RealFs;
use providers::Registry;
use state::StateStore;
use sweep::Options;

/// The plugin id: also the source tag on the sidebar metadata it reports and
/// the name of herdr's state directory for it.
pub const PLUGIN_ID: &str = "io.rlew.workspace-renamer";

/// Writes one `workspace-renamer: ` line to stderr, which herdr keeps in the
/// plugin log. A failed write is ignored: there is nowhere else to report it.
pub fn warn(msg: impl Display) {
    let line = format!("workspace-renamer: {msg}\n");
    #[cfg(test)]
    test_support::capture(&line);
    #[cfg(not(test))]
    let _ = io::stderr().write_all(line.as_bytes());
}

fn env_nonempty(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|v| !v.is_empty())
}

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);

    if flag("--version") {
        let _ = writeln!(
            io::stdout(),
            "{} {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        return;
    }

    let dry = flag("--dry-run");
    let reset = flag("--reset");
    // --reset is the "hand back" action: the invoking workspace is treated as
    // unclaimed for this sweep. Event hooks may carry HERDR_WORKSPACE_ID too,
    // so only the flag (set by the action's command line) turns this on.
    let reset_id = env_nonempty("HERDR_WORKSPACE_ID")
        .filter(|_| reset)
        .map(|id| id.to_string_lossy().into_owned());
    let home = env::home_dir().unwrap_or_default();
    // --dry-run from a plain shell must read the real ownership state or the
    // preview diverges from event-driven behaviour, so herdr's default plugin
    // state path fills in when the env var is absent. Real runs still require
    // the env var: its presence is the proof we're running under herdr.
    let state_dir = env_nonempty("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| dry.then(|| home.join(".local/state/herdr/plugins").join(PLUGIN_ID)));
    let bin = env_nonempty("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let home_str = home.to_string_lossy();
    let opts = Options {
        home: &home_str,
        reset: reset_id.as_deref(),
        dry,
    };
    let mut names = Registry::new(home.clone());

    if dry {
        let herdr = Cli::new(bin, &|| {});
        let store = StateStore::read_only(state_dir);
        sweep::run(&herdr, &store, &mut names, &opts);
        return;
    }
    // Outside herdr (no state dir) a real run would rename workspaces without
    // recording ownership, permanently orphaning them from future syncs, so
    // refuse rather than half-work. --dry-run stays available for previewing.
    let Some(dir) = state_dir else {
        warn(
            "HERDR_PLUGIN_STATE_DIR is not set (not running under herdr?); refusing to rename without state tracking — use --dry-run to preview",
        );
        return;
    };
    if reset && reset_id.is_none() {
        warn("reset: no invoking workspace (HERDR_WORKSPACE_ID unset); plain sweep only");
    }
    lock::serialized(&dir, reset_id.as_deref(), &RealFs, |lock| {
        let keepalive = || lock.touch();
        let herdr = Cli::new(bin, &keepalive);
        let store = StateStore::new(dir.clone());
        sweep::run(&herdr, &store, &mut names, &opts);
    });
}

#[cfg(test)]
mod test_support {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        static STDERR: RefCell<String> = const { RefCell::new(String::new()) };
    }

    pub fn capture(line: &str) {
        STDERR.with(|s| s.borrow_mut().push_str(line));
    }

    /// Everything `warn` wrote on this thread so far, clearing the buffer.
    pub fn take_stderr() -> String {
        STDERR.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    /// A fresh directory removed on drop.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("wsren-unit-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
