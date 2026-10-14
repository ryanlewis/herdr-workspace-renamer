//! Name providers: one per agent type, each answering "what name did the
//! user give this session?"
//!
//! A provider reads whatever registry its agent keeps and reduces it to a
//! map of session id → user-set name. `None` means "no opinion" and always
//! collapses to the no-op path, so adding a provider can never weaken the
//! fail-safe posture. Everything else (workspace join, primary selection,
//! ownership guard, state) is agent-agnostic and lives in `reconcile`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::json::lenient;

/// Session id → the name the user set, or `None` for a session the registry
/// knows but the user hasn't named.
pub type Names = HashMap<String, Option<String>>;

pub trait Provider {
    /// The herdr agent type this provider claims (`agent` in `herdr agent list`).
    fn agent(&self) -> &'static str;
    /// Reads the agent's registry under `home`; `None` when there is no data.
    fn load(&self, home: &Path) -> Option<Names>;
}

/// Looks up user-set session names by agent type.
pub trait SessionNames {
    fn user_name(&mut self, agent: &str, session: &str) -> Option<String>;
}

/// The real providers. A provider's registry is read lazily, at most once per
/// sweep, and only when an agent of its type is primary in some workspace (no
/// point parsing the Codex index on a Claude-only machine).
pub struct Registry {
    home: PathBuf,
    providers: Vec<Box<dyn Provider>>,
    loaded: HashMap<&'static str, Option<Names>>,
}

impl Registry {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            providers: vec![Box::new(Claude), Box::new(Codex)],
            loaded: HashMap::new(),
        }
    }
}

impl SessionNames for Registry {
    fn user_name(&mut self, agent: &str, session: &str) -> Option<String> {
        let provider = self.providers.iter().find(|p| p.agent() == agent)?;
        let home = &self.home;
        self.loaded
            .entry(provider.agent())
            .or_insert_with(|| provider.load(home))
            .as_ref()?
            .get(session)?
            .clone()
    }
}

/// Each text parsed as JSON into `T`; anything unparseable (mid-write, stale
/// junk, wrong shape) is skipped. Bad data means "no session info here",
/// never a hard failure.
fn parse_each<'a, T: serde::de::DeserializeOwned>(
    texts: impl IntoIterator<Item = &'a str>,
) -> impl Iterator<Item = T> {
    texts
        .into_iter()
        .filter_map(|text| serde_json::from_str(text).ok())
}

/// Claude Code: `~/.claude/sessions/<pid>.json`, one file per running
/// session. An undocumented internal (shape observed on 2.1.226): `name` is
/// the session name, and `nameSource: "derived"` marks auto-generated names;
/// it is absent on user-renamed sessions.
struct Claude;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSession {
    session_id: String,
    name: String,
    #[serde(default, deserialize_with = "lenient")]
    name_source: Option<String>,
}

impl Provider for Claude {
    fn agent(&self) -> &'static str {
        "claude"
    }

    fn load(&self, home: &Path) -> Option<Names> {
        let dir = fs::read_dir(home.join(".claude/sessions")).ok()?;
        let texts: Vec<String> = dir
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            // A file can vanish mid-sweep: skip it.
            .filter_map(|e| fs::read(e.path()).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect();
        let names: Names = parse_each::<ClaudeSession>(texts.iter().map(String::as_str))
            .filter(|s| !s.session_id.is_empty())
            .map(|s| {
                let user_set = s.name_source.as_deref() != Some("derived");
                (s.session_id, user_set.then_some(s.name))
            })
            .collect();
        (!names.is_empty()).then_some(names)
    }
}

/// Codex: `~/.codex/session_index.jsonl`, append-only, one line per *named*
/// thread (`{id, thread_name, updated_at}`). Auto/unnamed sessions never
/// appear, so index presence doubles as the "explicitly named" signal (Codex
/// has no equivalent of Claude Code's nameSource field). For duplicate ids
/// the entry with the newest `updated_at` wins, falling back to line order
/// when the timestamps don't decide; an entry whose `thread_name` is no
/// longer a string is a tombstone that clears the name. Codex panes get the
/// session id that joins against this from herdr's Codex SessionStart hook
/// integration.
struct Codex;

#[derive(Deserialize)]
struct CodexEntry {
    id: String,
    #[serde(default, deserialize_with = "lenient")]
    thread_name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    updated_at: Option<String>,
}

impl Provider for Codex {
    fn agent(&self) -> &'static str {
        "codex"
    }

    fn load(&self, home: &Path) -> Option<Names> {
        let bytes = fs::read(home.join(".codex/session_index.jsonl")).ok()?;
        let raw = String::from_utf8_lossy(&bytes);
        let lines = raw.split('\n').filter(|l| !l.trim().is_empty());
        let mut newest: HashMap<String, (Option<String>, Option<i64>)> = HashMap::new();
        for entry in parse_each::<CodexEntry>(lines).filter(|e| !e.id.is_empty()) {
            let at = entry.updated_at.as_deref().and_then(parse_timestamp);
            // Keep the previous entry only when both timestamps are usable
            // and the previous one is strictly newer; otherwise the later
            // line wins.
            if let (Some(at), Some((_, Some(prev)))) = (at, newest.get(&entry.id))
                && at < *prev
            {
                continue;
            }
            newest.insert(entry.id, (entry.thread_name, at));
        }
        let names: Names = newest
            .into_iter()
            .map(|(id, (name, _))| (id, name))
            .collect();
        (!names.is_empty()).then_some(names)
    }
}

/// Milliseconds since the epoch for an ISO 8601 timestamp: `YYYY-MM-DD`,
/// optionally followed by `THH:MM[:SS[.fff…]]` and `Z` or `±HH:MM`. A time
/// without an offset is read as UTC. `None` when it doesn't parse.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let mut c = Cursor(s.as_bytes());
    let year = c.digits(4)?;
    c.expect(b'-')?;
    let month = c.digits(2)?;
    c.expect(b'-')?;
    let day = c.digits(2)?;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let date_ms = days_from_civil(year, month, day) * 86_400_000;
    if c.is_empty() {
        return Some(date_ms);
    }
    c.expect(b'T')?;
    let hour = c.digits(2)?;
    c.expect(b':')?;
    let minute = c.digits(2)?;
    let (mut second, mut millis) = (0, 0);
    if c.eat(b':') {
        second = c.digits(2)?;
        if c.eat(b'.') {
            let frac = c.take_while(|b| b.is_ascii_digit());
            if frac.is_empty() {
                return None;
            }
            millis = frac
                .iter()
                .chain(b"00")
                .take(3)
                .fold(0, |n, d| n * 10 + i64::from(d - b'0'));
        }
    }
    if minute > 59 || second > 59 || hour > 24 || (hour == 24 && minute + second + millis > 0) {
        return None;
    }
    let offset_min = if c.eat(b'Z') {
        0
    } else if let Some(sign) = [(b'+', 1), (b'-', -1)]
        .into_iter()
        .find_map(|(ch, sign)| c.eat(ch).then_some(sign))
    {
        let h = c.digits(2)?;
        c.expect(b':')?;
        let m = c.digits(2)?;
        sign * (h * 60 + m)
    } else {
        0
    };
    if !c.is_empty() {
        return None;
    }
    Some(date_ms + ((hour * 60 + minute - offset_min) * 60 + second) * 1000 + millis)
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn eat(&mut self, b: u8) -> bool {
        match self.0.split_first() {
            Some((&first, rest)) if first == b => {
                self.0 = rest;
                true
            }
            _ => false,
        }
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        self.eat(b).then_some(())
    }

    fn take_while(&mut self, pred: impl Fn(u8) -> bool) -> &[u8] {
        let n = self.0.iter().take_while(|&&b| pred(b)).count();
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        head
    }

    fn digits(&mut self, n: usize) -> Option<i64> {
        let head = self.0.get(..n)?;
        if !head.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.0 = &self.0[n..];
        Some(head.iter().fold(0, |acc, d| acc * 10 + i64::from(d - b'0')))
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use serde_json::{Value, json};

    fn claude_home(sessions: &[Value]) -> TempDir {
        let home = TempDir::new();
        let dir = home.path().join(".claude/sessions");
        fs::create_dir_all(&dir).unwrap();
        for (i, s) in sessions.iter().enumerate() {
            fs::write(dir.join(format!("{}.json", 1000 + i)), s.to_string()).unwrap();
        }
        home
    }

    /// Writes the Codex index; string entries are written verbatim so tests
    /// can mix corrupt lines with valid ones.
    fn codex_home(lines: &[Value]) -> TempDir {
        let home = TempDir::new();
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        let text: Vec<String> = lines
            .iter()
            .map(|l| match l {
                Value::String(raw) => raw.clone(),
                other => other.to_string(),
            })
            .collect();
        fs::write(
            home.path().join(".codex/session_index.jsonl"),
            text.join("\n") + "\n",
        )
        .unwrap();
        home
    }

    fn lookup(home: &TempDir, agent: &str, session: &str) -> Option<String> {
        Registry::new(home.path().to_owned()).user_name(agent, session)
    }

    #[test]
    fn claude_user_named_session() {
        let home = claude_home(&[json!({"pid": 1, "sessionId": "s1", "name": "my-cool-task"})]);
        assert_eq!(
            lookup(&home, "claude", "s1").as_deref(),
            Some("my-cool-task")
        );
    }

    #[test]
    fn claude_derived_name_is_no_opinion() {
        let home = claude_home(&[
            json!({"pid": 1, "sessionId": "s1", "name": "notes-27", "nameSource": "derived"}),
        ]);
        assert_eq!(lookup(&home, "claude", "s1"), None);
    }

    #[test]
    fn claude_unknown_session_and_bad_files_skipped() {
        let home = claude_home(&[
            json!({"pid": 1, "sessionId": "someone-else", "name": "irrelevant"}),
            json!({"pid": 2, "sessionId": "", "name": "empty-id"}),
            json!({"pid": 3, "sessionId": "s3", "name": 42}),
        ]);
        fs::write(
            home.path().join(".claude/sessions/9.json"),
            "{\"sessionId\": GARBAGE",
        )
        .unwrap();
        assert_eq!(lookup(&home, "claude", "not-in-registry"), None);
        assert_eq!(lookup(&home, "claude", ""), None);
        assert_eq!(lookup(&home, "claude", "s3"), None);
        assert_eq!(
            lookup(&home, "claude", "someone-else").as_deref(),
            Some("irrelevant")
        );
    }

    #[test]
    fn unknown_agent_type_has_no_provider() {
        let home = claude_home(&[json!({"pid": 1, "sessionId": "s1", "name": "x"})]);
        assert_eq!(lookup(&home, "gemini", "s1"), None);
    }

    #[test]
    fn codex_named_thread() {
        let home = codex_home(&[
            json!({"id": "cdx-1", "thread_name": "codex-task-name", "updated_at": "2026-08-08T21:40:59Z"}),
        ]);
        assert_eq!(
            lookup(&home, "codex", "cdx-1").as_deref(),
            Some("codex-task-name")
        );
        assert_eq!(lookup(&home, "codex", "cdx-unnamed"), None);
    }

    #[test]
    fn codex_empty_id_never_joins() {
        let home = codex_home(&[
            json!({"id": "", "thread_name": "stray-name", "updated_at": "2026-08-08T21:40:59Z"}),
        ]);
        assert_eq!(lookup(&home, "codex", ""), None);
    }

    #[test]
    fn codex_newest_entry_wins() {
        let home = codex_home(&[
            json!({"id": "cdx-1", "thread_name": "old-name", "updated_at": "2026-08-08T21:00:00Z"}),
            json!({"id": "cdx-1", "thread_name": "new-name", "updated_at": "2026-08-08T22:00:00Z"}),
        ]);
        assert_eq!(lookup(&home, "codex", "cdx-1").as_deref(), Some("new-name"));
    }

    #[test]
    fn codex_updated_at_beats_line_order() {
        let home = codex_home(&[
            json!({"id": "cdx-1", "thread_name": "new-name", "updated_at": "2026-08-08T22:00:00Z"}),
            json!({"id": "cdx-1", "thread_name": "old-name", "updated_at": "2026-08-08T21:00:00Z"}),
        ]);
        assert_eq!(lookup(&home, "codex", "cdx-1").as_deref(), Some("new-name"));
    }

    #[test]
    fn codex_unusable_timestamp_falls_back_to_line_order() {
        let home = codex_home(&[
            json!({"id": "cdx-1", "thread_name": "first", "updated_at": "2026-08-08T22:00:00Z"}),
            json!({"id": "cdx-1", "thread_name": "second", "updated_at": "yesterday"}),
        ]);
        assert_eq!(lookup(&home, "codex", "cdx-1").as_deref(), Some("second"));
    }

    #[test]
    fn codex_tombstone_clears_name() {
        let home = codex_home(&[
            json!({"id": "cdx-1", "thread_name": "old-name", "updated_at": "2026-08-08T21:00:00Z"}),
            json!({"id": "cdx-1", "thread_name": null, "updated_at": "2026-08-08T22:00:00Z"}),
        ]);
        assert_eq!(lookup(&home, "codex", "cdx-1"), None);
    }

    #[test]
    fn codex_corrupt_line_skipped() {
        let home = codex_home(&[
            json!(r#"{"id":"cdx-1","thread_na GARBAGE"#),
            json!({"id": "cdx-2", "thread_name": "good-name", "updated_at": "2026-08-08T22:00:00Z"}),
        ]);
        assert_eq!(
            lookup(&home, "codex", "cdx-2").as_deref(),
            Some("good-name")
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("1970-01-01"), Some(0));
        assert_eq!(parse_timestamp("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(
            parse_timestamp("2026-08-08T21:40:59Z"),
            Some(1_786_225_259_000)
        );
        assert_eq!(
            parse_timestamp("2026-08-08T21:40:59.123456Z"),
            Some(1_786_225_259_123)
        );
        assert_eq!(
            parse_timestamp("2026-08-08T23:40:59+02:00"),
            parse_timestamp("2026-08-08T21:40:59Z")
        );
        for bad in [
            "",
            "yesterday",
            "2026-13-01",
            "2026-02-30",
            "2026-08-08T25:00Z",
            "2026-08-08Tx",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }
}
