//! The Output panel (ced E1 plan §4.9): what macros print, stdout and
//! stderr interleaved as they arrive, stderr in the destructive colour. The
//! log is bounded so a chatty macro cannot grow ced without limit.

use std::collections::VecDeque;

use iced::widget::{column, container, scrollable};
use iced::{Element, Length, Padding};

use super::Look;
use super::problems::panel;
use crate::app::Msg;

/// Lines kept.
pub const MAX_LINES: usize = 5000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub stderr: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Output {
    lines: VecDeque<Line>,
    dropped: usize,
}

impl Output {
    pub fn push(&mut self, text: impl Into<String>, stderr: bool) {
        if self.lines.len() == MAX_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(Line { text: text.into(), stderr });
    }

    pub fn lines(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.dropped = 0;
    }
}

/// The scrollable's id (snapped to the end as lines arrive).
pub const SCROLL_ID: &str = "ced-output-scroll";

pub fn view<'a>(look: Look, output: &'a Output) -> Element<'a, Msg> {
    let t = look.tokens;
    let mut col = column![];
    if output.dropped > 0 {
        col = col.push(look.code(format!("… {} earlier lines dropped", output.dropped)).color(t.muted_text));
    }
    for line in output.lines() {
        let text = look.code(line.text.as_str());
        col = col.push(if line.stderr { text.color(t.destructive) } else { text });
    }
    let body = scrollable(container(col).padding(Padding::from([4, 12])).width(Length::Fill))
        .id(SCROLL_ID)
        .anchor_bottom()
        .height(Length::Fill);
    panel(look, "Output", body.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_bounded() {
        let mut o = Output::default();
        for i in 0..MAX_LINES + 3 {
            o.push(i.to_string(), false);
        }
        assert_eq!(o.lines().count(), MAX_LINES);
        assert_eq!(o.lines().next().unwrap().text, "3");
        assert_eq!(o.dropped, 3);
    }
}
