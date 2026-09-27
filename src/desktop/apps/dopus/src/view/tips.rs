//! Shared iced tooltips, including transparent regions over custom-drawn icons.
use super::Look;
use crate::verbs::ActionRow;
use cosmix_actions::ActionId;
use iced::advanced::{layout, widget::Tree};
use iced::widget::{Space, text, tooltip};
use iced::{Element, Rectangle, Size};
use iced_tiny_skia::Renderer;

pub fn action_label(actions: &[ActionRow], action: ActionId, label: &str) -> String {
    match actions.iter().find(|row| row.id == action.as_str()) {
        Some(row) if !row.keys.is_empty() => {
            let mut keys: Vec<&str> = row.keys.iter().map(String::as_str).collect();
            keys.sort_by_key(|key| !key.starts_with("Ctrl+"));
            format!("{label} ({})", keys.join(", "))
        }
        _ => label.to_owned(),
    }
}

pub fn tip<'a, M: 'a>(
    look: Look,
    content: impl Into<Element<'a, M>>,
    label: String,
) -> Element<'a, M> {
    // The compiled design dictionary currently has no timing tokens. Keep
    // iced's default delay until the design supplies a duration role.
    tooltip(
        content,
        text(label)
            .font(look.ui_font)
            .size(look.small_px)
            .shaping(iced::advanced::text::Shaping::Advanced)
            .wrapping(iced::advanced::text::Wrapping::WordOrGlyph),
        tooltip::Position::Bottom,
    )
    .padding(look.chrome.pad)
    .gap(look.chrome.small)
    .style(move |_| look.tokens.tooltip_style(look.chrome.edge))
    .into()
}

/// Build only visible hit regions. These children draw no pixels: their
/// stock iced Tooltip supplies hover timing and a viewport-clamped overlay.
pub fn regions<M: 'static>(
    look: Look,
    regions: Vec<(Rectangle, String)>,
    children: &mut Vec<Element<'static, M>>,
    tree: &mut Tree,
    renderer: &Renderer,
    size: Size,
) -> layout::Node {
    *children = regions
        .iter()
        .map(|(bounds, label)| {
            tip(
                look,
                Space::new().width(bounds.width).height(bounds.height),
                label.clone(),
            )
        })
        .collect();
    tree.diff_children(children.as_slice());
    let nodes = children
        .iter_mut()
        .zip(&mut tree.children)
        .zip(regions)
        .map(|((child, state), (bounds, _))| {
            child
                .as_widget_mut()
                .layout(
                    state,
                    renderer,
                    &layout::Limits::new(Size::ZERO, bounds.size()),
                )
                .move_to(bounds.position())
        })
        .collect();
    layout::Node::with_children(size, nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_include_function_keys_and_desktop_alternates() {
        let actions = crate::verbs::action_table(&crate::keys::load(None).unwrap());
        assert_eq!(
            action_label(&actions, cosmix_actions::filemgr::VIEW_REFRESH, "Refresh"),
            "Refresh (Ctrl+R, F5)"
        );
        for (action, label, original, alternate) in [
            (
                cosmix_actions::filemgr::VIEW_REFRESH,
                "Refresh",
                "F5",
                "Ctrl+R",
            ),
            (
                cosmix_actions::filemgr::FILE_RENAME,
                "Rename",
                "F2",
                "Ctrl+E",
            ),
            (
                cosmix_actions::filemgr::NAV_SWITCH_PANE,
                "Switch pane",
                "F6",
                "Tab",
            ),
        ] {
            let tip = action_label(&actions, action, label);
            assert!(tip.contains(original), "{tip}");
            assert!(tip.contains(alternate), "{tip}");
        }
    }

    #[test]
    fn labels_use_effective_remaps_and_omit_unbound_keys() {
        let mut keymap = crate::keys::load(None).unwrap();
        keymap.custom = cosmix_actions::parse_keymap(r#"{
          version: 1, chord_timeout_ms: 1000, defaults: [], custom: [
            {action: "nav.back", chord: ["Ctrl+J"], scope: "global", repeat: "ignore", allow_in_editable: false},
            {action: "nav.forward", chord: nil, scope: "global"}
          ]
        }"#).unwrap().custom;
        let actions = crate::verbs::action_table(&keymap);
        assert_eq!(
            action_label(&actions, cosmix_actions::filemgr::NAV_BACK, "Back"),
            "Back (Ctrl+J)"
        );
        assert_eq!(
            action_label(&actions, cosmix_actions::filemgr::NAV_FORWARD, "Forward"),
            "Forward"
        );
    }
}
