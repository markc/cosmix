//! Go to Line (`L[:C]`), Ctrl+G. Lines are 1-based; the column is editd's
//! (scalars, 1-based) and optional.

use iced::widget::{column, text_input};
use iced::{Element, Padding};

use super::{DialogMsg, frame};
use crate::app::Msg;
use crate::chrome::Look;

pub const INPUT: &str = "ced-goto-input";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Goto {
    pub input: String,
    pub lines: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotoMsg {
    Input(String),
    Submit,
}

/// Parse `L`, `L:C` or `L,C`; `line` is clamped to `1..=lines` by the caller.
pub fn parse(input: &str) -> Result<(usize, Option<usize>), String> {
    let input = input.trim();
    let (l, c) = match input.split_once([':', ',']) {
        Some((l, c)) => (l, Some(c)),
        None => (input, None),
    };
    let line: usize = l.trim().parse().map_err(|_| format!("“{l}” is not a line number"))?;
    let col = match c {
        Some(c) if !c.trim().is_empty() => Some(c.trim().parse().map_err(|_| format!("“{c}” is not a column"))?),
        _ => None,
    };
    if line == 0 || col == Some(0) {
        return Err("lines and columns start at 1".into());
    }
    Ok((line, col))
}

impl Goto {
    pub fn new(lines: usize) -> Self {
        Goto { lines, ..Goto::default() }
    }

    /// `Some((line, col))` when the input is valid.
    pub fn update(&mut self, msg: GotoMsg) -> Option<(usize, Option<usize>)> {
        match msg {
            GotoMsg::Input(s) => {
                self.input = s;
                self.error = None;
                None
            }
            GotoMsg::Submit => match parse(&self.input) {
                Ok((line, col)) => Some((line.min(self.lines.max(1)), col)),
                Err(e) => {
                    self.error = Some(e);
                    None
                }
            },
        }
    }

    pub fn view<'a>(&'a self, look: Look) -> Element<'a, Msg> {
        let m = |g: GotoMsg| Msg::Dialog(DialogMsg::Goto(g));
        let mut body = column![
            look.small(format!("Line (1–{}), optionally :column", self.lines.max(1))).color(look.tokens.muted_text),
            text_input("42 or 42:7", &self.input)
                .id(INPUT)
                .on_input(move |s| m(GotoMsg::Input(s)))
                .on_submit(m(GotoMsg::Submit))
                .font(look.mono)
                .size(look.ui_px)
                .padding(Padding::from([6, 10]))
                .style(look.input()),
        ]
        .spacing(8);
        if let Some(e) = &self.error {
            body = body.push(look.small(e.as_str()).color(look.tokens.destructive));
        }
        frame(
            look,
            "Go to Line",
            body.into(),
            vec![
                look.button("Cancel", Some(Msg::Dialog(DialogMsg::Close))).style(look.secondary()).into(),
                look.button("Go", Some(m(GotoMsg::Submit))).style(look.primary()).into(),
            ],
            360.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_and_column() {
        assert_eq!(parse("42"), Ok((42, None)));
        assert_eq!(parse(" 42:7 "), Ok((42, Some(7))));
        assert_eq!(parse("42,7"), Ok((42, Some(7))));
        assert_eq!(parse("42:"), Ok((42, None)));
        assert!(parse("0").is_err() && parse("x").is_err() && parse("3:0").is_err());
    }

    #[test]
    fn clamps_to_the_last_line() {
        let mut g = Goto::new(10);
        g.update(GotoMsg::Input("99".into()));
        assert_eq!(g.update(GotoMsg::Submit), Some((10, None)));
        g.update(GotoMsg::Input("nope".into()));
        assert_eq!(g.update(GotoMsg::Submit), None);
        assert!(g.error.is_some());
    }
}
