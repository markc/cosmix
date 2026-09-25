//! The editor widget. **Stage S stub**: the signature is frozen; the body
//! draws a plain filled area. Stage E1e replaces it with the custom
//! `iced::advanced::Widget` of plan §4.2 (virtualised cell-grid text,
//! selection, carets, markers, IME, clipboard, scrolling) that also emits
//! [`EditorMsg::Layout`](super::EditorMsg::Layout) each frame.
//!
//! [`EditorWidget::with`] (E1f, additive) is what the app calls: it adds the
//! [`EditorView`] — font, zoom, measurement and View toggles — that the frozen
//! `new` has no parameter for.

use cosmix_edit_client::diag::Diagnostics;
use cosmix_edit_client::highlight::Highlight;
use cosmix_edit_client::model::EditorModel;
use cosmix_edit_core::text::Text;
use iced::Element;

use super::{EditorMsg, EditorView, Palette};

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
        Self::with(text, model, highlight, palette, diagnostics, &EditorView::default())
    }

    /// The widget the app builds every frame. Contract for E1e: draw with
    /// `view.font` at `view.px` (line height `px × view.line_height`), measure
    /// with `view.measure`, honour the three View toggles, draw the caret and
    /// request the input method only while `view.focused`. The app's root key
    /// router consumes every keymap chord, Alt+letter and Ctrl+wheel before
    /// this widget sees them, and blocks all keyboard/IME input to it while a
    /// modal dialog is open.
    pub fn with<'a>(
        text: &'a Text,
        model: &'a EditorModel,
        highlight: &'a Highlight,
        palette: &'a Palette,
        diagnostics: &'a Diagnostics,
        view: &EditorView,
    ) -> Element<'a, EditorMsg> {
        let _ = (model, highlight, diagnostics);
        let background = palette.background;
        iced::widget::container(iced::widget::text(format!("{} bytes", text.len())).font(view.font).size(view.px))
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .style(move |_theme| iced::widget::container::Style {
                background: Some(background.into()),
                ..Default::default()
            })
            .into()
    }
}
