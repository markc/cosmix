//! Cosmix dark theme constants and style helpers for Iced widgets.

use iced::widget::{button, container};
use iced::{color, Border, Color, Theme};

// ── Palette ──────────────────────────────────────────────────────────

pub const BG_PRIMARY: Color = color!(0x030712);
pub const BG_SECONDARY: Color = color!(0x111827);
pub const BG_INPUT: Color = color!(0x1f2937);
pub const FG_PRIMARY: Color = color!(0xf3f4f6);
pub const FG_SECONDARY: Color = color!(0x9ca3af);
pub const ACCENT: Color = color!(0x3b82f6);
pub const ACCENT_HOVER: Color = color!(0x2563eb);
pub const BTN_SECONDARY: Color = color!(0x374151);
pub const BTN_SECONDARY_HOVER: Color = color!(0x4b5563);
pub const BORDER: Color = color!(0x808080, 0.3);
pub const BORDER_FOCUS: Color = color!(0x3b82f6);
pub const DANGER: Color = color!(0xef4444);
pub const WARNING: Color = color!(0xf59e0b);
pub const INFO: Color = color!(0x3b82f6);

// ── Dialog frame ─────────────────────────────────────────────────────

/// Rounded dark container for the dialog frame.
pub fn dialog_frame(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_PRIMARY.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    }
}

/// Footer container (darker, top border).
pub fn dialog_footer(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_SECONDARY.into()),
        border: Border {
            color: BORDER,
            width: 0.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    }
}

// ── Buttons ──────────────────────────────────────────────────────────

/// Primary action button (blue).
pub fn btn_primary(theme: &Theme, status: button::Status) -> button::Style {
    let base = button::primary(theme, status);
    button::Style {
        background: Some(match status {
            button::Status::Hovered | button::Status::Pressed => ACCENT_HOVER.into(),
            _ => ACCENT.into(),
        }),
        text_color: Color::WHITE,
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..base
    }
}

/// Secondary action button (gray).
pub fn btn_secondary(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(match status {
            button::Status::Hovered | button::Status::Pressed => BTN_SECONDARY_HOVER.into(),
            _ => BTN_SECONDARY.into(),
        }),
        text_color: FG_PRIMARY,
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}
