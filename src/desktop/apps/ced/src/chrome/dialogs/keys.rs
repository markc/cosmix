//! Help > Keyboard Shortcuts: every bound chord by menu, the contextual keys
//! (Tab/Shift+Tab, F10, Alt+letter) and the macros' chords — read from the
//! same tables the key router uses, so the list cannot drift.

use iced::widget::{column, row, scrollable};
use iced::{Element, Length};

use super::{DialogMsg, frame};
use crate::actions::{ActionId, Menu};
use crate::app::Msg;
use crate::chrome::Look;
use crate::keymap;
use crate::macros::MacroDef;

/// `(menu, [(label, chords)])` for every action with a chord.
pub fn table() -> Vec<(Menu, Vec<(String, String)>)> {
    Menu::ALL
        .iter()
        .filter_map(|menu| {
            let rows: Vec<(String, String)> = ActionId::all()
                .into_iter()
                .filter(|a| a.menu() == *menu)
                .filter_map(|a| {
                    let chords = keymap::chords_for(a);
                    (!chords.is_empty()).then(|| (a.label(), chords.join(", ")))
                })
                .collect();
            (!rows.is_empty()).then_some((*menu, rows))
        })
        .collect()
}

pub fn view<'a>(look: Look, macros: &'a [MacroDef]) -> Element<'a, Msg> {
    let t = look.tokens;
    let entry = |label: String, keys: String| -> Element<'a, Msg> {
        row![look.small(label).width(Length::Fill), look.code(keys).color(t.muted_text)].spacing(12).into()
    };
    let mut body = column![].spacing(4);
    for (menu, rows) in table() {
        body = body.push(look.text(menu.label()).color(look.chrome.accent));
        for (label, keys) in rows {
            body = body.push(entry(label, keys));
        }
    }
    body = body.push(look.text("Everywhere").color(look.chrome.accent));
    for (keys, what) in keymap::CONTEXTUAL {
        body = body.push(entry((*what).to_owned(), (*keys).to_owned()));
    }
    body = body.push(entry("zoom".into(), "Ctrl+wheel".into()));
    let bound: Vec<&MacroDef> = macros.iter().filter(|m| m.chord.is_some()).collect();
    if !bound.is_empty() {
        body = body.push(look.text("Macros").color(look.chrome.accent));
        for m in bound {
            body = body.push(entry(m.label.clone(), m.chord.clone().unwrap_or_default()));
        }
    }
    frame(
        look,
        "Keyboard Shortcuts",
        scrollable(body).height(Length::Fixed(420.0)).into(),
        vec![look.button("Close", Some(Msg::Dialog(DialogMsg::Close))).style(look.primary()).into()],
        560.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chord_is_listed_once() {
        let listed: usize = table().iter().flat_map(|(_, rows)| rows).map(|(_, keys)| keys.split(", ").count()).sum();
        assert_eq!(listed, keymap::DEFAULT.len());
    }
}
