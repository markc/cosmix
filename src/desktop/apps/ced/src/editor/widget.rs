//! The editor widget. **Stage S stub**: the signature is frozen; the body
//! draws a plain filled area. Stage E1e replaces it with the custom
//! `iced::advanced::Widget` of plan §4.2 (virtualised cell-grid text,
//! selection, carets, markers, IME, clipboard, scrolling) that also emits
//! [`EditorMsg::Layout`](super::EditorMsg::Layout) each frame.

use cosmix_edit_client::diag::Diagnostics;
use cosmix_edit_client::highlight::Highlight;
use cosmix_edit_client::model::EditorModel;
use cosmix_edit_core::text::Text;
use iced::Element;

use super::{EditorMsg, Palette};

pub struct EditorWidget;

impl EditorWidget {
    #[allow(clippy::new_ret_no_self)]
    pub fn new<'a>(
        text: &'a Text,
        model: &'a EditorModel,
        highlight: &'a Highlight,
        palette: &'a Palette,
        diagnostics: &'a Diagnostics,
    ) -> Element<'a, EditorMsg> {
        let _ = (model, highlight, diagnostics);
        let background = palette.background;
        iced::widget::container(iced::widget::text(format!("{} bytes", text.len())))
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .style(move |_theme| iced::widget::container::Style {
                background: Some(background.into()),
                ..Default::default()
            })
            .into()
    }
}
