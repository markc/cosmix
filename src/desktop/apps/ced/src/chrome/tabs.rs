//! The tab strip (ced E1 plan §4.4): one tab per buffer — name, `●` dirty,
//! `!` disk modified or deleted, `◆` agent edits since the tab was last
//! focused, `⟳` while (re)attaching. Click selects, middle-click or the `×`
//! closes, the strip scrolls sideways on overflow.

use cosmix_edit_client::mirror::{Phase, DetachReason};
use cosmix_edit_client::types::TabId;
use cosmix_edit_core::wire::DiskState;
use iced::widget::{button, container, mouse_area, row, scrollable};
use iced::{Alignment, Background, Border, Element, Length, Padding};

use super::{Look, TABS_H};
use crate::app::Msg;
use crate::controller::Tab;

/// What a tab shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBadge {
    pub name: String,
    pub dirty: bool,
    pub disk_alert: bool,
    pub agent_edits: bool,
    pub attaching: bool,
    pub detached: bool,
    /// Tooltip / title: the full path, or the scratch name.
    pub title: String,
}

/// The display name of a tab: the file name, the buffer's own name, or the
/// path the user asked for while it is still opening.
pub fn display_name(tab: &Tab) -> String {
    let meta = tab.mirror.as_ref().map(|m| m.meta());
    let path = meta.and_then(|m| m.path.clone()).or_else(|| tab.path.clone());
    if let Some(path) = path {
        return std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(path);
    }
    meta.and_then(|m| m.name.clone()).unwrap_or_else(|| "untitled".to_owned())
}

pub fn badge(tab: &Tab, agent_edits: bool) -> TabBadge {
    let name = display_name(tab);
    let Some(mirror) = tab.mirror.as_ref() else {
        return TabBadge {
            title: tab.path.clone().unwrap_or_else(|| name.clone()),
            name,
            dirty: false,
            disk_alert: false,
            agent_edits: false,
            attaching: true,
            detached: false,
        };
    };
    let meta = mirror.meta();
    let (attaching, detached) = match mirror.phase() {
        Phase::Bootstrapping { .. } | Phase::Recovering { .. } => (true, false),
        Phase::Detached { reason: DetachReason::EpochChanged } => (true, false),
        Phase::Detached { .. } => (false, true),
        Phase::Live => (false, false),
    };
    TabBadge {
        title: meta.path.clone().unwrap_or_else(|| name.clone()),
        name,
        dirty: meta.dirty,
        disk_alert: matches!(meta.disk, DiskState::Modified | DiskState::Deleted),
        agent_edits,
        attaching,
        detached,
    }
}

impl TabBadge {
    /// The strip label: marks, then the name.
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.attaching {
            s.push_str("⟳ ");
        }
        if self.dirty {
            s.push_str("● ");
        }
        s.push_str(&self.name);
        if self.disk_alert {
            s.push_str(" !");
        }
        if self.agent_edits {
            s.push_str(" ◆");
        }
        s
    }
}

/// The strip. `agent_edits(id)` says whether an agent edited that tab since
/// it was last focused.
pub fn view<'a>(look: Look, tabs: &'a [Tab], active: Option<TabId>, agent_edits: impl Fn(TabId) -> bool) -> Element<'a, Msg> {
    let t = look.tokens;
    let accent = look.chrome.accent;
    let strip = look.chrome.secondary;
    let mut items = row![].spacing(1).align_y(Alignment::End);
    for tab in tabs {
        let b = badge(tab, agent_edits(tab.id));
        let is_active = active == Some(tab.id);
        let fg = if b.detached { t.muted_text } else if is_active { t.text } else { look.chrome.secondary_text };
        let label = look.text(b.label()).color(fg);
        let close = button(look.small("×").color(t.muted_text))
            .padding(Padding::from([0, 4]))
            .style(look.flat())
            .on_press(Msg::CloseTab(tab.id));
        let body = row![label, close].spacing(8).align_y(Alignment::Center);
        let id = tab.id;
        let tab_button = button(body)
            .padding(Padding { top: 5.0, bottom: 5.0, left: 12.0, right: 6.0 })
            .on_press(Msg::SelectTab(id))
            .style(move |_, status| button::Style {
                background: Some(Background::Color(if is_active {
                    t.surface
                } else if matches!(status, button::Status::Hovered) {
                    t.muted_surface
                } else {
                    strip
                })),
                text_color: fg,
                border: Border {
                    // The active tab carries an accent rule along its top edge.
                    color: if is_active { accent } else { strip },
                    width: if is_active { 1.0 } else { 0.0 },
                    radius: iced::border::Radius { top_left: t.radius, top_right: t.radius, ..Default::default() },
                },
                ..button::Style::default()
            });
        items = items.push(mouse_area(tab_button).on_middle_press(Msg::CloseTab(id)));
    }
    let new_tab = button(look.text("+").color(look.chrome.secondary_text))
        .padding(Padding::from([4, 10]))
        .style(look.flat())
        .on_press(Msg::Action(crate::actions::ActionId::FileNew));
    let strip_row = row![
        scrollable(items).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new().width(3).scroller_width(3))).width(Length::Shrink),
        new_tab
    ]
    .align_y(Alignment::End)
    .padding(Padding { left: 4.0, ..Padding::ZERO });
    container(strip_row)
        .width(Length::Fill)
        .height(Length::Fixed(TABS_H))
        .align_y(Alignment::End)
        .style(look.strip(strip, look.chrome.secondary_text))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_carry_every_mark() {
        let b = TabBadge {
            name: "scene.mix".into(),
            dirty: true,
            disk_alert: true,
            agent_edits: true,
            attaching: false,
            detached: false,
            title: "/x/scene.mix".into(),
        };
        assert_eq!(b.label(), "● scene.mix ! ◆");
        let b = TabBadge { dirty: false, disk_alert: false, agent_edits: false, attaching: true, ..b };
        assert_eq!(b.label(), "⟳ scene.mix");
    }
}
