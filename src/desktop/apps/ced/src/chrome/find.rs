//! The find / replace bar (ced E1 plan §4.4). Searching runs in the `edit`
//! service (`edit.find`) — never over a local copy — so what is found is what
//! an agent would find. Find Next / Previous wait for the pipeline to drain,
//! search from the caret with one wrap; Replace is one `ApplyAt` with
//! `expect_rev`; Replace All pages `edit.find`, then preflights (≤ 10,000
//! matches and ≤ 1 MiB inserted) and sends one `ApplyAt`, so it is one undo
//! group.
//!
//! This module owns the bar's state and view and the pure pieces of the
//! protocol (whole-word patterns, `$n` expansion, the preflight); the
//! controller runs the requests (`ced.action search.*` carries the same
//! arguments, so a Bus caller gets identical behaviour).

use iced::widget::{button, container, row, text_input};
use iced::{Alignment, Element, Length, Padding};

use super::Look;
use crate::app::Msg;

/// The pattern field's widget id (focused when the bar opens).
pub const FIND_INPUT: &str = "ced-find-input";
pub const REPLACE_INPUT: &str = "ced-replace-input";

/// Replace All refuses more matches than this (E0 `MAX_OPS_PER_TXN`).
pub const MAX_REPLACE_MATCHES: usize = 10_000;
/// …or more inserted bytes than this (E0 per-request insert limit).
pub const MAX_REPLACE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindBar {
    pub open: bool,
    pub replace: bool,
    pub pattern: String,
    pub replacement: String,
    /// Case-sensitive (editd's default is true; Notepad++'s is false).
    pub case: bool,
    pub regex: bool,
    pub word: bool,
    /// "3 matches", "No matches", "Wrapped", an error.
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindMsg {
    Pattern(String),
    Replacement(String),
    ToggleCase,
    ToggleRegex,
    ToggleWord,
    Next,
    Prev,
    Replace,
    ReplaceAll,
    Close,
}

impl FindBar {
    /// The `ced.action` / `edit.find` arguments for the current query.
    pub fn args(&self) -> serde_json::Value {
        let (pattern, regex) = wire_pattern(&self.pattern, self.regex, self.word);
        serde_json::json!({ "pattern": pattern, "regex": regex, "case": self.case })
    }

    pub fn replace_args(&self) -> serde_json::Value {
        let mut v = self.args();
        v["replacement"] = serde_json::Value::String(self.replacement.clone());
        // `$n` references only mean something to a regex search.
        v["expand"] = serde_json::Value::Bool(self.regex);
        v
    }

    pub fn update(&mut self, msg: &FindMsg) {
        match msg {
            FindMsg::Pattern(p) => {
                self.pattern = p.clone();
                self.status = None;
            }
            FindMsg::Replacement(r) => self.replacement = r.clone(),
            FindMsg::ToggleCase => self.case = !self.case,
            FindMsg::ToggleRegex => self.regex = !self.regex,
            FindMsg::ToggleWord => self.word = !self.word,
            FindMsg::Close => self.open = false,
            FindMsg::Next | FindMsg::Prev | FindMsg::Replace | FindMsg::ReplaceAll => {}
        }
    }
}

/// The pattern editd searches for: whole-word wraps it in `\b…\b` (escaping a
/// literal first), which makes it a regex search.
pub fn wire_pattern(pattern: &str, regex: bool, word: bool) -> (String, bool) {
    match (word, regex) {
        (false, r) => (pattern.to_owned(), r),
        (true, true) => (format!(r"\b(?:{pattern})\b"), true),
        (true, false) => (format!(r"\b{}\b", escape(pattern)), true),
    }
}

/// Escape regex metacharacters (the `regex::escape` set).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Expand `$0`..`$9`, `${n}` and `$$` in a replacement template. A group that
/// did not participate expands to nothing; an out-of-range reference is kept
/// literally (so a typo is visible in the result, not silently dropped).
pub fn expand(template: &str, whole: &str, groups: &[Option<String>]) -> String {
    let group = |n: usize| -> Option<&str> {
        if n == 0 { Some(whole) } else { groups.get(n - 1).map(|g| g.as_deref().unwrap_or("")) }
    };
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
        } else if let Some(tail) = after.strip_prefix('{')
            && let Some(close) = tail.find('}')
            && let Ok(n) = tail[..close].parse::<usize>()
        {
            match group(n) {
                Some(g) => out.push_str(g),
                None => out.push_str(&rest[i..i + 2 + close + 1]),
            }
            rest = &tail[close + 1..];
        } else if let Some(d) = after.chars().next().filter(char::is_ascii_digit) {
            let n = d.to_digit(10).unwrap_or(0) as usize;
            match group(n) {
                Some(g) => out.push_str(g),
                None => {
                    out.push('$');
                    out.push(d);
                }
            }
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Replace All's preflight (codex #15): refuse what cannot be one undoable
/// transaction.
pub fn preflight(matches: usize, inserted_bytes: usize) -> Result<(), String> {
    if matches > MAX_REPLACE_MATCHES || inserted_bytes > MAX_REPLACE_BYTES {
        return Err(format!(
            "Too large for one undoable transaction ({matches} matches, {inserted_bytes} bytes); narrow the search or use a macro."
        ));
    }
    Ok(())
}

/// The bar.
pub fn view<'a>(look: Look, bar: &'a FindBar) -> Element<'a, Msg> {
    let t = look.tokens;
    let toggle = |label: &'static str, on: bool, msg: FindMsg, tip: &'static str| -> Element<'a, Msg> {
        let b = button(look.small(label).font(look.mono)).padding(Padding::from([3, 7])).on_press(Msg::Find(msg));
        let b = if on { b.style(look.primary()) } else { b.style(look.secondary()) };
        iced::widget::tooltip(b, container(look.small(tip)).padding(6).style(look.strip(t.popover, t.popover_text)), iced::widget::tooltip::Position::Top)
            .into()
    };
    let find = text_input("Find", &bar.pattern)
        .id(FIND_INPUT)
        .on_input(|s| Msg::Find(FindMsg::Pattern(s)))
        .on_submit(Msg::Find(FindMsg::Next))
        .font(look.mono)
        .size(look.small_px())
        .padding(Padding::from([4, 8]))
        .width(Length::FillPortion(3))
        .style(look.input());
    let mut line = row![
        find,
        toggle("Aa", bar.case, FindMsg::ToggleCase, "Match case"),
        toggle("W", bar.word, FindMsg::ToggleWord, "Whole word"),
        toggle(".*", bar.regex, FindMsg::ToggleRegex, "Regular expression"),
        look.button("↑", Some(Msg::Find(FindMsg::Prev))).style(look.secondary()),
        look.button("↓", Some(Msg::Find(FindMsg::Next))).style(look.secondary()),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if bar.replace {
        let replace = text_input("Replace", &bar.replacement)
            .id(REPLACE_INPUT)
            .on_input(|s| Msg::Find(FindMsg::Replacement(s)))
            .on_submit(Msg::Find(FindMsg::Replace))
            .font(look.mono)
            .size(look.small_px())
            .padding(Padding::from([4, 8]))
            .width(Length::FillPortion(2))
            .style(look.input());
        line = line
            .push(replace)
            .push(look.button("Replace", Some(Msg::Find(FindMsg::Replace))).style(look.secondary()))
            .push(look.button("All", Some(Msg::Find(FindMsg::ReplaceAll))).style(look.secondary()));
    }
    if let Some(status) = &bar.status {
        line = line.push(look.small(status.as_str()).color(t.muted_text));
    }
    line = line.push(
        button(look.text("×").color(t.muted_text)).padding(Padding::from([0, 6])).style(look.flat()).on_press(Msg::Find(FindMsg::Close)),
    );
    container(line)
        .padding(Padding::from([6, 10]))
        .width(Length::Fill)
        .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_word_escapes_literals() {
        assert_eq!(wire_pattern("a.b", false, false), ("a.b".into(), false));
        assert_eq!(wire_pattern("a.b", false, true), (r"\ba\.b\b".into(), true));
        assert_eq!(wire_pattern("x|y", true, true), (r"\b(?:x|y)\b".into(), true));
    }

    #[test]
    fn replacement_expansion() {
        let g = [Some("key".to_owned()), None, Some("v".to_owned())];
        assert_eq!(expand("$1=$3", "key: v", &g), "key=v");
        assert_eq!(expand("[$0]", "m", &g), "[m]");
        assert_eq!(expand("$2|", "m", &g), "|", "a non-participating group is empty");
        assert_eq!(expand("${1}x", "m", &g), "keyx");
        assert_eq!(expand("$$1 $9", "m", &g), "$1 $9", "$$ is a dollar; out of range stays literal");
        assert_eq!(expand("cost $", "m", &g), "cost $");
        assert_eq!(expand("${12}", "m", &g), "${12}");
    }

    #[test]
    fn preflight_limits() {
        assert!(preflight(10_000, 1024 * 1024).is_ok());
        let err = preflight(10_001, 5).unwrap_err();
        assert_eq!(err, "Too large for one undoable transaction (10001 matches, 5 bytes); narrow the search or use a macro.");
        assert!(preflight(1, 1024 * 1024 + 1).is_err());
    }

    #[test]
    fn args_carry_the_query() {
        let bar = FindBar { pattern: "foo".into(), word: true, case: true, replacement: "$1".into(), ..FindBar::default() };
        assert_eq!(bar.args(), serde_json::json!({"pattern": r"\bfoo\b", "regex": true, "case": true}));
        assert_eq!(bar.replace_args()["expand"], false);
    }
}
