//! Conflict details (§3.7 "Show"): who won, which lines, and exactly the
//! text of yours that was not applied, with Copy and Re-insert.

use cosmix_edit_client::types::{Conflict, TabId};
use iced::widget::{column, container, scrollable};
use iced::{Element, Length};

use super::{DialogMsg, frame};
use crate::app::Msg;
use crate::chrome::Look;
use crate::chrome::infobar::{InfoAction, conflict_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictView {
    pub tab: TabId,
    pub conflict: Conflict,
}

impl ConflictView {
    pub fn view<'a>(&'a self, look: Look) -> Element<'a, Msg> {
        let t = look.tokens;
        let mut texts = column![].spacing(8);
        for text in &self.conflict.texts {
            texts = texts.push(
                container(look.code(text.as_str()))
                    .padding(8)
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(t.surface.into()),
                        border: iced::Border { color: t.border, width: 1.0, radius: t.radius.into() },
                        ..container::Style::default()
                    }),
            );
        }
        let body = column![
            look.text(conflict_text(&self.conflict)),
            look.small(format!("Remote edit: rev {}", self.conflict.rev)).color(t.muted_text),
            look.small("Your text that was not applied:").color(t.muted_text),
            scrollable(texts).height(Length::Shrink),
        ]
        .spacing(8);
        let (tab, rev) = (self.tab, self.conflict.rev);
        frame(
            look,
            "Edit conflict",
            body.into(),
            vec![
                look.button("Close", Some(Msg::Dialog(DialogMsg::Close))).style(look.secondary()).into(),
                look.button("Copy", Some(Msg::Info(InfoAction::CopyConflict(tab, rev)))).style(look.secondary()).into(),
                look.button("Re-insert at Caret", Some(Msg::Info(InfoAction::ReinsertConflict(tab, rev)))).style(look.primary()).into(),
            ],
            600.0,
        )
    }
}
