//! Recovered buffers (§3.8): on launch, buffers the edit service restored
//! from its recovery files that no one holds are offered once — Open, or
//! Discard (`edit.close force:true`, which deletes their recovery files).

use iced::widget::{column, row};
use iced::{Alignment, Element, Length};

use super::{DialogMsg, frame};
use crate::app::Msg;
use crate::chrome::Look;

/// One offered buffer (the controller's row).
pub use crate::controller::RecoveredRow as RecoveredBuffer;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovered {
    pub buffers: Vec<RecoveredBuffer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveredMsg {
    Open(String),
    Discard(String),
    OpenAll,
}

impl Recovered {
    /// Remove a handled buffer; `true` when none are left.
    pub fn handled(&mut self, buffer: &str) -> bool {
        self.buffers.retain(|b| b.buffer != buffer);
        self.buffers.is_empty()
    }

    pub fn view<'a>(&'a self, look: Look) -> Element<'a, Msg> {
        let t = look.tokens;
        let m = |r: RecoveredMsg| Msg::Dialog(DialogMsg::Recovered(r));
        let mut list = column![
            look.text("The edit service restored these unsaved buffers after a restart. Nothing is holding them.")
        ]
        .spacing(8);
        for b in &self.buffers {
            let detail = match (&b.path, b.bytes) {
                (Some(p), _) => p.clone(),
                (None, Some(n)) => format!("scratch · {n} bytes"),
                (None, None) => "scratch".to_owned(),
            };
            list = list.push(
                row![
                    column![look.text(b.name.as_str()), look.code(detail).color(t.muted_text)].width(Length::Fill),
                    look.button("Discard", Some(m(RecoveredMsg::Discard(b.buffer.clone())))).style(look.danger()),
                    look.button("Open", Some(m(RecoveredMsg::Open(b.buffer.clone())))).style(look.primary()),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        frame(
            look,
            "Recovered buffers",
            list.into(),
            vec![
                look.button("Later", Some(Msg::Dialog(DialogMsg::Close))).style(look.secondary()).into(),
                look.button("Open All", Some(m(RecoveredMsg::OpenAll))).style(look.primary()).into(),
            ],
            560.0,
        )
    }
}
