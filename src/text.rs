//! Pure text helpers: label cleaning, terminal titles, herdr id order, paths.

/// Longest label the plugin writes, in characters.
pub const MAX_LABEL: usize = 32;

/// JavaScript's `\s`: what the Node version collapsed and trimmed. Unlike
/// `char::is_whitespace` it includes U+FEFF and excludes U+0085.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Control characters dropped outright. Tab, newline and carriage return
/// are kept so they become spaces.
fn is_dropped_control(c: char) -> bool {
    matches!(c, '\0'..='\u{08}' | '\u{0B}' | '\u{0C}' | '\u{0E}'..='\u{1F}' | '\u{7F}')
}

/// Strips control characters, collapses whitespace runs to one space and
/// trims the ends.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.chars().filter(|&c| !is_dropped_control(c)) {
        if is_space(c) {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    out
}

/// A name the user typed, made fit for a label: scrubbed and capped at
/// [`MAX_LABEL`] characters. No re-slugging: the user typed what they want.
pub fn clean(name: &str) -> String {
    let capped: String = scrub(name).chars().take(MAX_LABEL).collect();
    capped.trim_matches(is_space).to_owned()
}

/// The agent's live terminal title, cleaned, or `None` when there is no real
/// one yet. Before Claude Code writes its first task summary the title is a
/// product-name placeholder ("Claude Code", "claude", "claude: <dir>") that
/// says nothing. Matching stays exact so a real summary that merely contains
/// the name survives.
pub fn live_title(agent_type: &str, raw: &str) -> Option<String> {
    let title = scrub(raw);
    let t = title.to_lowercase();
    let a = agent_type.to_lowercase();
    if title.is_empty() || t == a || t == format!("{a} code") || t.starts_with(&format!("{a}: ")) {
        return None;
    }
    // A summary is prose, so an over-long one is cut at the last word that
    // fits and marked with "…", rather than mid-word the way clean() caps a
    // name the user typed.
    if title.chars().count() <= MAX_LABEL {
        return Some(title);
    }
    let head: String = title.chars().take(MAX_LABEL - 1).collect();
    let head = match head.rfind(' ') {
        Some(cut) if cut > 0 => &head[..cut],
        _ => &head,
    };
    Some(format!("{}…", head.trim_end_matches(is_space)))
}

/// herdr's id digits, lowest first: bijective base 32.
const ID_DIGITS: &str = "123456789ABCDEFGHJKMNPQRSTVWXYZ0";

/// Where a herdr pane (`kind` `p`) or tab (`t`) id falls in creation order.
///
/// Ids like `w1:p7`, `w1:tM`, `w1:p11` end in a per-workspace creation
/// counter written in bijective base 32: digits 1-9, then A-Z without I, L,
/// O and U, then 0 standing for 32 (so pane 32 is `p0`, 33 is `p11`, 64 is
/// `p10`). Counters only go up and are never reused, so a lower rank always
/// means created earlier. Ids that don't parse rank last, level with each
/// other.
pub fn creation_rank(id: Option<&str>, kind: char) -> u64 {
    let digits = id
        .and_then(|id| id.rsplit_once(':'))
        .and_then(|(_, tail)| tail.strip_prefix(kind))
        .filter(|d| !d.is_empty());
    let Some(digits) = digits else {
        return u64::MAX;
    };
    digits
        .chars()
        .try_fold(0u64, |n, ch| {
            let d = ID_DIGITS.find(ch)? as u64;
            n.checked_mul(32)?.checked_add(d + 1)
        })
        .unwrap_or(u64::MAX)
}

/// The last component of a path, as Node's `path.basename` gives it: trailing
/// slashes ignored, `""` for `/`.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// `path` with the home directory shortened to `~`.
pub fn tildify(path: &str, home: &str) -> String {
    if path == home {
        return "~".to_owned();
    }
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_collapses_strips_and_caps() {
        let got = clean(&format!("  Fix\tthe   thing\x07 {}", "x".repeat(60)));
        assert!(got.starts_with("Fix the thing"), "{got}");
        assert!(got.chars().count() <= MAX_LABEL);
        assert!(!got.chars().any(|c| c.is_control()));
        assert_eq!(clean("fix \x07 thing"), "fix thing");
        assert_eq!(clean(&"🚀".repeat(40)), "🚀".repeat(32));
        assert_eq!(clean(" ? "), "?");
    }

    #[test]
    fn live_title_ignores_placeholders() {
        for t in ["Claude Code", "claude", "claude: notes", "  "] {
            assert_eq!(live_title("claude", t), None, "{t:?}");
        }
        assert_eq!(
            live_title("claude", "Claude Code hooks setup").as_deref(),
            Some("Claude Code hooks setup")
        );
    }

    #[test]
    fn long_live_title_cut_at_a_word() {
        assert_eq!(
            live_title("claude", "Ntfy arloo-estate-alerts JSON formatting").as_deref(),
            Some("Ntfy arloo-estate-alerts JSON…")
        );
        let unbroken = "x".repeat(40);
        assert_eq!(
            live_title("claude", &unbroken),
            Some(format!("{}…", "x".repeat(31)))
        );
    }

    #[test]
    fn creation_rank_is_bijective_base_32() {
        let p = |id| creation_rank(Some(id), 'p');
        assert_eq!(p("w1:p1"), 1);
        assert_eq!(p("w1:p9"), 9);
        assert_eq!(p("w1:pA"), 10);
        assert_eq!(p("w1:pC"), 12);
        assert_eq!(p("w1:p0"), 32);
        assert_eq!(p("w1:p11"), 33);
        assert_eq!(p("w1:p10"), 64);
        assert_eq!(creation_rank(Some("w1:tM"), 't'), 20);
        for bad in ["w1:pI", "w1:p", "w1:t1", "p1", "w1:p1x"] {
            assert_eq!(p(bad), u64::MAX, "{bad}");
        }
        assert_eq!(creation_rank(None, 'p'), u64::MAX);
    }

    #[test]
    fn basename_and_tildify() {
        assert_eq!(basename("/Users/ryan/dev/notes"), "notes");
        assert_eq!(basename("/Users/ryan/dev/notes/"), "notes");
        assert_eq!(basename("/"), "");
        assert_eq!(tildify("/home/r", "/home/r"), "~");
        assert_eq!(tildify("/home/r/dev", "/home/r"), "~/dev");
        assert_eq!(tildify("/home/rx/dev", "/home/r"), "/home/rx/dev");
    }
}
