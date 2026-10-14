//! The herdr CLI: the records the plugin reads from it and the calls it makes.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::PLUGIN_ID;
use crate::json::lenient;

const TIMEOUT: Duration = Duration::from_secs(10);

/// One record from `herdr agent list`.
#[derive(Debug, Deserialize)]
pub struct Agent {
    /// The agent type, e.g. `claude` or `codex`.
    #[serde(default, deserialize_with = "lenient")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    agent_session: Option<AgentSession>,
    #[serde(default, deserialize_with = "lenient")]
    pub pane_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub tab_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub workspace_id: Option<String>,
    /// The agent's live terminal title (OSC title, spinner stripped).
    #[serde(default, deserialize_with = "lenient")]
    pub terminal_title_stripped: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentSession {
    #[serde(default, deserialize_with = "lenient")]
    value: Option<String>,
}

impl Agent {
    /// The agent's session id, the key into its agent's session registry,
    /// when herdr has joined one.
    pub fn session(&self) -> Option<&str> {
        let value = self.agent_session.as_ref()?.value.as_deref()?;
        (!value.is_empty()).then_some(value)
    }
}

/// One record from `herdr workspace list`.
#[derive(Debug, Deserialize)]
pub struct Workspace {
    #[serde(default, deserialize_with = "lenient")]
    pub workspace_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub label: Option<String>,
    /// The sidebar tokens herdr shows for this workspace now.
    #[serde(default, deserialize_with = "lenient")]
    tokens: Option<Map<String, Value>>,
}

impl Workspace {
    pub fn token(&self, name: &str) -> Option<&Value> {
        self.tokens.as_ref()?.get(name)
    }
}

/// One record from `herdr tab list`.
#[derive(Debug, Deserialize)]
pub struct Tab {
    #[serde(default, deserialize_with = "lenient")]
    pub tab_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub workspace_id: Option<String>,
}

/// One record from `herdr pane list`.
#[derive(Debug, Deserialize)]
pub struct Pane {
    #[serde(default, deserialize_with = "lenient")]
    pub pane_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub tab_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub cwd: Option<String>,
}

/// Everything one sweep reads from herdr up front.
#[derive(Debug)]
pub struct Snapshot {
    pub agents: Vec<Agent>,
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<Pane>,
}

impl Snapshot {
    /// Builds a snapshot from the four `list` replies. Each must carry its
    /// records as an array under `result`; records that aren't objects are
    /// dropped.
    pub fn from_replies(
        agents: Option<&Value>,
        workspaces: Option<&Value>,
        tabs: Option<&Value>,
        panes: Option<&Value>,
    ) -> Result<Self, Error> {
        Ok(Self {
            agents: records(agents, "agents").ok_or(Error::Shape)?,
            workspaces: records(workspaces, "workspaces").ok_or(Error::Shape)?,
            tabs: records(tabs, "tabs").ok_or(Error::Shape)?,
            panes: records(panes, "panes").ok_or(Error::Shape)?,
        })
    }
}

fn records<T: DeserializeOwned>(reply: Option<&Value>, key: &str) -> Option<Vec<T>> {
    let items = reply?.get("result")?.get(key)?.as_array()?;
    Some(
        items
            .iter()
            .filter(|v| v.is_object())
            .filter_map(|v| T::deserialize(v).ok())
            .collect(),
    )
}

/// The custom sidebar tokens the plugin reports (see README).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// `$dir`: the root pane's directory, under a label the plugin owns.
    Dir,
    /// `$session`: the session name, under a label the user chose.
    Session,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Token::Dir => "dir",
            Token::Session => "session",
        })
    }
}

/// A display-only workspace metadata change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaOp {
    Set {
        workspace: String,
        token: Token,
        value: String,
    },
    Clear {
        workspace: String,
        token: Token,
    },
}

impl MetaOp {
    /// The arguments after `workspace report-metadata`. The workspace id
    /// comes first: herdr's parser rejects options-then-positional here.
    pub fn args(&self) -> Vec<String> {
        let (workspace, tail) = match self {
            MetaOp::Set {
                workspace,
                token,
                value,
            } => (
                workspace,
                ["--token".to_owned(), format!("{token}={value}")],
            ),
            MetaOp::Clear { workspace, token } => {
                (workspace, ["--clear-token".to_owned(), token.to_string()])
            }
        };
        let mut args = vec![workspace.clone(), "--source".into(), PLUGIN_ID.into()];
        args.extend(tail);
        args
    }
}

/// What the plugin needs from herdr.
pub trait Herdr {
    fn snapshot(&self) -> Result<Snapshot, Error>;
    /// The workspace's label right now, re-read just before a rename.
    fn label(&self, workspace: &str) -> Result<Option<String>, Error>;
    fn rename(&self, workspace: &str, label: &str) -> Result<(), Error>;
    fn report_metadata(&self, op: &MetaOp) -> Result<(), Error>;
}

#[derive(Debug)]
pub enum Error {
    Spawn {
        command: String,
        source: io::Error,
    },
    Timeout {
        command: String,
    },
    Failed {
        command: String,
        status: ExitStatus,
        stderr: String,
    },
    Json {
        command: String,
        source: serde_json::Error,
    },
    /// A `list` reply without its records array.
    Shape,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Spawn { command, source } => write!(f, "{command}: {source}"),
            Error::Timeout { command } => {
                write!(f, "{command}: timed out after {}s", TIMEOUT.as_secs())
            }
            Error::Failed {
                command,
                status,
                stderr,
            } => match status.code() {
                Some(code) => write!(f, "{command}: exit {code} {stderr}"),
                None => write!(f, "{command}: {status} {stderr}"),
            },
            Error::Json { command, source } => write!(f, "{command}: {source}"),
            Error::Shape => f.write_str("unexpected herdr list output shape; no-op"),
        }
    }
}

/// herdr reached through its CLI (`HERDR_BIN_PATH`, else `herdr` on PATH).
pub struct Cli<'a> {
    bin: OsString,
    /// Called before every herdr call: a slow sweep keeps its lock fresh
    /// between calls so it isn't mistaken for a crashed one.
    keepalive: &'a dyn Fn(),
}

impl<'a> Cli<'a> {
    pub fn new(bin: OsString, keepalive: &'a dyn Fn()) -> Self {
        Self { bin, keepalive }
    }

    /// Runs one herdr command and parses its stdout. Some verbs (e.g.
    /// report-metadata) succeed silently with no output: that is `None`.
    fn run(&self, args: &[&str]) -> Result<Option<Value>, Error> {
        (self.keepalive)();
        let command = format!("herdr {}", args.join(" "));
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                command: command.clone(),
                source,
            })?;
        let stdout = read_in_background(child.stdout.take());
        let stderr = read_in_background(child.stderr.take());

        let deadline = Instant::now() + TIMEOUT;
        let timeout = || Error::Timeout {
            command: command.clone(),
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(timeout());
                }
                Ok(None) => thread::sleep(Duration::from_millis(2)),
                Err(source) => {
                    return Err(Error::Spawn {
                        command: command.clone(),
                        source,
                    });
                }
            }
        };
        // A grandchild could hold the pipes open after herdr exits; don't
        // wait past the deadline for them.
        let left = deadline.saturating_duration_since(Instant::now());
        let stdout = stdout.recv_timeout(left).map_err(|_| timeout())?;
        let stderr = stderr.recv_timeout(left).unwrap_or_default();

        if !status.success() {
            return Err(Error::Failed {
                command,
                status,
                stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
            });
        }
        let stdout = String::from_utf8_lossy(&stdout);
        if stdout.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&stdout)
            .map(Some)
            .map_err(|source| Error::Json { command, source })
    }
}

fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

impl Herdr for Cli<'_> {
    fn snapshot(&self) -> Result<Snapshot, Error> {
        let agents = self.run(&["agent", "list"])?;
        let workspaces = self.run(&["workspace", "list"])?;
        let tabs = self.run(&["tab", "list"])?;
        let panes = self.run(&["pane", "list"])?;
        Snapshot::from_replies(
            agents.as_ref(),
            workspaces.as_ref(),
            tabs.as_ref(),
            panes.as_ref(),
        )
    }

    fn label(&self, workspace: &str) -> Result<Option<String>, Error> {
        let reply = self.run(&["workspace", "get", workspace])?;
        Ok(reply
            .as_ref()
            .and_then(|r| r.pointer("/result/workspace/label"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    fn rename(&self, workspace: &str, label: &str) -> Result<(), Error> {
        self.run(&["workspace", "rename", workspace, label])
            .map(drop)
    }

    fn report_metadata(&self, op: &MetaOp) -> Result<(), Error> {
        let args = op.args();
        let mut argv = vec!["workspace", "report-metadata"];
        argv.extend(args.iter().map(String::as_str));
        self.run(&argv).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn odd_fields_and_records_are_tolerated() {
        let agents = json!({"result": {"agents": [
            {"agent": 7, "agent_session": "nope", "pane_id": "w1:p1", "workspace_id": "w1"},
            "not a record",
            ["w1", "w1:p2"],
        ]}});
        let empty = |key: &str| json!({"result": {key: []}});
        let snap = Snapshot::from_replies(
            Some(&agents),
            Some(&empty("workspaces")),
            Some(&empty("tabs")),
            Some(&empty("panes")),
        )
        .expect("valid shape");
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].agent, None);
        assert_eq!(snap.agents[0].session(), None);
        assert_eq!(snap.agents[0].pane_id.as_deref(), Some("w1:p1"));
    }

    #[test]
    fn missing_records_array_is_a_shape_error() {
        let ok = |key: &str| json!({"result": {key: []}});
        let bad = json!({"result": {"agents": {}}});
        let r = Snapshot::from_replies(
            Some(&bad),
            Some(&ok("workspaces")),
            Some(&ok("tabs")),
            Some(&ok("panes")),
        );
        assert!(matches!(r, Err(Error::Shape)));
        let r = Snapshot::from_replies(
            None,
            Some(&ok("workspaces")),
            Some(&ok("tabs")),
            Some(&ok("panes")),
        );
        assert!(matches!(r, Err(Error::Shape)));
    }
}
