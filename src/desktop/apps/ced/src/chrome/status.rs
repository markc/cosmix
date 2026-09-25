//! The status bar (ced E1 plan §4.4): `Ln L, Col C` (C is editd's col —
//! scalars, CR counted), selection size, rev, `UTF-8 · LF · BOM`, language,
//! Modified/Saved, disk state, the last remote edit (clickable: jumps to it),
//! INS/OVR, the pending count when above 0, and a red `UNPROTECTED` while the
//! edit service is volatile or its recovery is degraded.

use cosmix_edit_client::mirror::{Mirror, Phase};
use cosmix_edit_core::wire::{DiskState, Eol};
use iced::widget::{button, container, row};
use iced::{Alignment, Element, Length, Padding};

use super::{Look, STATUS_H};
use crate::app::Msg;
use crate::controller::Tab;

/// Selections larger than this report bytes instead of counting scalars
/// every frame.
const COUNT_CHARS_MAX: usize = 64 * 1024;

/// One status field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Plain,
    /// Draws muted.
    Quiet,
    /// Draws in the destructive colour.
    Alarm,
    /// Clickable: jumps to the last remote edit.
    LastRemote,
}

fn plain(text: impl Into<String>) -> Field {
    Field { text: text.into(), kind: FieldKind::Plain }
}

fn quiet(text: impl Into<String>) -> Field {
    Field { text: text.into(), kind: FieldKind::Quiet }
}

/// Left-hand fields (position and selection) and right-hand fields.
pub fn fields(tab: &Tab, unprotected: bool) -> (Vec<Field>, Vec<Field>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    if unprotected {
        right.push(Field { text: "UNPROTECTED".into(), kind: FieldKind::Alarm });
    }
    let Some(mirror) = tab.mirror.as_ref() else {
        left.push(quiet("Opening…"));
        return (left, right);
    };
    let text = mirror.text();
    let sel = tab.editor.sel;
    let head = text.point(sel.head);
    left.push(plain(format!("Ln {}, Col {}", head.line, head.col)));
    let (a, b) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
    if a != b {
        let lines = text.point(b).line - text.point(a).line + 1;
        let size = if b - a <= COUNT_CHARS_MAX {
            let mut s = String::new();
            text.read(a..b, &mut s);
            format!("{}", s.chars().count())
        } else {
            human_bytes(b - a)
        };
        left.push(plain(if lines > 1 { format!("Sel {size} | {lines} lines") } else { format!("Sel {size}") }));
    }
    if mirror.pending() > 0 {
        left.push(quiet(format!("{} pending", mirror.pending())));
    }
    if let Some(mark) = mirror.last_remote() {
        right.push(Field { text: format!("{} · rev {}", mark.origin, mark.rev), kind: FieldKind::LastRemote });
    }
    right.extend(meta_fields(mirror));
    right.push(plain(if tab.editor.overwrite { "OVR" } else { "INS" }));
    (left, right)
}

fn meta_fields(mirror: &Mirror) -> Vec<Field> {
    let meta = mirror.meta();
    let mut out = Vec::new();
    match mirror.phase() {
        Phase::Live => {}
        Phase::Bootstrapping { .. } => out.push(quiet("loading")),
        Phase::Recovering { .. } => out.push(quiet("resyncing")),
        Phase::Detached { .. } => out.push(Field { text: "detached".into(), kind: FieldKind::Alarm }),
    }
    match meta.disk {
        DiskState::Modified => out.push(Field { text: "changed on disk".into(), kind: FieldKind::Alarm }),
        DiskState::Deleted => out.push(Field { text: "deleted on disk".into(), kind: FieldKind::Alarm }),
        DiskState::Unwatched => out.push(quiet("unwatched")),
        DiskState::Clean | DiskState::None => {}
    }
    out.push(plain(if meta.dirty { "Modified" } else if meta.path.is_some() { "Saved" } else { "Unsaved" }));
    out.push(quiet(format!("rev {}", mirror.rev())));
    out.push(plain(encoding(meta.eol, meta.bom)));
    out.push(plain(meta.language.clone()));
    out
}

/// `UTF-8 · LF` (· BOM).
pub fn encoding(eol: Eol, bom: bool) -> String {
    let eol = match eol {
        Eol::Lf | Eol::None => "LF",
        Eol::Crlf => "CRLF",
        Eol::Mixed => "Mixed EOL",
    };
    if bom { format!("UTF-8 · {eol} · BOM") } else { format!("UTF-8 · {eol}") }
}

pub fn human_bytes(n: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

/// The bar. `message` is a transient status line (left, after the fields).
pub fn view<'a>(look: Look, tab: Option<&'a Tab>, unprotected: bool, message: Option<&'a str>) -> Element<'a, Msg> {
    let t = look.tokens;
    let (left, right) = match tab {
        Some(tab) => fields(tab, unprotected),
        None => (Vec::new(), if unprotected { vec![Field { text: "UNPROTECTED".into(), kind: FieldKind::Alarm }] } else { Vec::new() }),
    };
    let draw = |f: Field| -> Element<'a, Msg> {
        match f.kind {
            FieldKind::Plain => look.small(f.text).into(),
            FieldKind::Quiet => look.small(f.text).color(t.muted_text).into(),
            FieldKind::Alarm => container(look.small(f.text).color(t.destructive_text))
                .padding(Padding::from([1, 6]))
                .style(look.strip(t.destructive, t.destructive_text))
                .into(),
            FieldKind::LastRemote => button(look.small(f.text).color(look.chrome.accent))
                .padding(Padding::ZERO)
                .style(look.flat())
                .on_press(Msg::JumpLastRemote)
                .into(),
        }
    };
    let mut l = row![].spacing(18).align_y(Alignment::Center);
    for f in left {
        l = l.push(draw(f));
    }
    if let Some(message) = message {
        l = l.push(look.small(message).color(t.muted_text));
    }
    let mut r = row![].spacing(18).align_y(Alignment::Center);
    for f in right {
        r = r.push(draw(f));
    }
    container(row![l, iced::widget::space().width(Length::Fill), r].align_y(Alignment::Center))
        .padding(Padding::from([0, 12]))
        .width(Length::Fill)
        .height(Length::Fixed(STATUS_H))
        .align_y(Alignment::Center)
        .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_and_sizes() {
        assert_eq!(encoding(Eol::Lf, false), "UTF-8 · LF");
        assert_eq!(encoding(Eol::Crlf, true), "UTF-8 · CRLF · BOM");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(3 * 1024 * 1024 / 2), "1.5 MiB");
    }
}
