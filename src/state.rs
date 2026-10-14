//! Plugin state: which workspace labels the plugin wrote itself.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde_json::{Map, Value};

/// Workspace id → the label the plugin last wrote there. Stored as
/// `state.json` in herdr's state directory for the plugin. Values are kept
/// as raw JSON so a hand-edited file round-trips unchanged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State(Map<String, Value>);

impl State {
    /// Whether `label` is the plugin's own last write to `workspace`.
    pub fn owns(&self, workspace: &str, label: &str) -> bool {
        self.0.get(workspace).and_then(Value::as_str) == Some(label)
    }

    pub fn contains(&self, workspace: &str) -> bool {
        self.0.contains_key(workspace)
    }

    pub fn claim(&mut self, workspace: &str, label: &str) {
        self.0.insert(workspace.to_owned(), label.into());
    }

    /// Forgets `workspace`; true when there was an entry.
    pub fn release(&mut self, workspace: &str) -> bool {
        self.0.shift_remove(workspace).is_some()
    }

    /// Drops entries for workspaces that no longer exist; true when any went.
    pub fn retain_live(&mut self, live: impl Fn(&str) -> bool) -> bool {
        let before = self.0.len();
        self.0.retain(|id, _| live(id));
        self.0.len() != before
    }

    #[cfg(test)]
    pub fn from_value(v: Value) -> Self {
        match v {
            Value::Object(map) => Self(map),
            _ => Self::default(),
        }
    }
}

/// Where state lives. With no directory (a dry run outside herdr with no
/// state yet) reads are empty and writes do nothing.
pub struct StateStore {
    dir: Option<PathBuf>,
    read_only: bool,
}

impl StateStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir: Some(dir),
            read_only: false,
        }
    }

    /// A store for --dry-run: reads the real state so the preview matches
    /// event-driven behaviour, never writes.
    pub fn read_only(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            read_only: true,
        }
    }

    /// The saved state. A missing or unreadable file is empty state.
    pub fn read(&self) -> State {
        let Some(dir) = &self.dir else {
            return State::default();
        };
        fs::read(dir.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
            .map(State)
            .unwrap_or_default()
    }

    /// Saves `state` by atomic replace. A torn state.json would read as
    /// empty in a concurrent sweep, which the ownership guard would misread
    /// as "the user named these workspaces", permanently. Write-then-rename
    /// makes that unobservable.
    pub fn write(&self, state: &State) -> io::Result<()> {
        let Some(dir) = self.dir.as_ref().filter(|_| !self.read_only) else {
            return Ok(());
        };
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("state.json.tmp-{}", std::process::id()));
        let mut text = serde_json::to_string_pretty(&state.0).map_err(io::Error::other)?;
        text.push('\n');
        fs::write(&tmp, text)?;
        fs::rename(&tmp, dir.join("state.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use serde_json::json;

    #[test]
    fn written_like_the_node_version() {
        let dir = TempDir::new();
        let store = StateStore::new(dir.path().to_owned());
        let mut state = State::from_value(json!({"w2": "b", "w1": "a"}));
        state.claim("w3", "c");
        store.write(&state).unwrap();
        let text = fs::read_to_string(dir.path().join("state.json")).unwrap();
        assert_eq!(
            text,
            "{\n  \"w2\": \"b\",\n  \"w1\": \"a\",\n  \"w3\": \"c\"\n}\n"
        );
        assert_eq!(store.read(), state);
        state.release("w2");
        store.write(&state).unwrap();
        let text = fs::read_to_string(dir.path().join("state.json")).unwrap();
        assert_eq!(text, "{\n  \"w1\": \"a\",\n  \"w3\": \"c\"\n}\n");
    }

    #[test]
    fn unreadable_state_is_empty() {
        let dir = TempDir::new();
        let store = StateStore::new(dir.path().to_owned());
        assert_eq!(store.read(), State::default());
        fs::write(dir.path().join("state.json"), "[1, 2]").unwrap();
        assert_eq!(store.read(), State::default());
        fs::write(dir.path().join("state.json"), "{\"w1\": ").unwrap();
        assert_eq!(store.read(), State::default());
    }

    #[test]
    fn read_only_store_never_writes() {
        let dir = TempDir::new();
        let store = StateStore::read_only(Some(dir.path().to_owned()));
        store.write(&State::from_value(json!({"w1": "a"}))).unwrap();
        assert!(!dir.path().join("state.json").exists());
    }
}
