//! Infobars (ced E1 plan §4.4): one strip per condition that needs a
//! decision or deserves attention — disk modified / deleted, conflicts
//! (§3.7), a reattach whose texts differ (§3.8), a detached tab, recovery
//! unhealthy, and warnings/errors the controller posted. Each is derived from
//! state every frame (state is truth); the app only remembers which ones the
//! user dismissed.

use cosmix_edit_client::mirror::{DetachReason, Phase};
use cosmix_edit_client::types::{Conflict, Level, TabId};
use cosmix_edit_core::wire::DiskState;
use iced::widget::{button, column, container, row};
use iced::{Alignment, Background, Border, Element, Length, Padding};

use super::Look;
use crate::app::Msg;
use crate::controller::Tab;

/// Identity of a dismissible infobar.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InfoKey {
    /// Disk modified (the app forgets the dismissal once the disk is clean).
    DiskModified(TabId),
    DiskDeleted(TabId),
    Conflict(TabId, u64),
    DetachedCopy(TabId, u64),
    Detached(TabId),
    Unprotected,
    Notice(u64),
}

/// A decision an infobar button makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoAction {
    Dismiss(InfoKey),
    Reload(TabId),
    Save(TabId),
    CloseTab(TabId),
    ShowConflict(TabId, u64),
    CopyConflict(TabId, u64),
    ReinsertConflict(TabId, u64),
    KeepMine(TabId),
    TakeService(TabId),
    SaveMineAs(TabId),
    KeepAsNew(TabId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub key: InfoKey,
    pub level: Level,
    pub text: String,
    pub actions: Vec<(String, InfoAction)>,
}

/// The characters of a conflict's reverted texts.
pub fn conflict_chars(c: &Conflict) -> usize {
    c.texts.iter().map(|t| t.chars().count()).sum()
}

/// The infobar sentence for a conflict (§3.7 wording).
pub fn conflict_text(c: &Conflict) -> String {
    let who = c.remote_origin.as_deref().unwrap_or("Another origin");
    let lines = if c.lines.0 == c.lines.1 { format!("line {}", c.lines.0) } else { format!("lines {}–{}", c.lines.0, c.lines.1) };
    let n = conflict_chars(c);
    let chars = if n == 1 { "character was".to_owned() } else { format!("{n} characters were") };
    format!("{who} edited {lines} while you were typing there; your {chars} not applied.")
}

/// Infobars for `tab` (in display order), minus the dismissed ones.
pub fn for_tab(tab: &Tab, dismissed: &dyn Fn(&InfoKey) -> bool) -> Vec<Info> {
    let mut out = Vec::new();
    let Some(m) = tab.mirror.as_ref() else { return out };
    let id = tab.id;
    let meta = m.meta();
    if let Phase::Detached { reason } = m.phase() {
        match reason {
            DetachReason::ClosedRemotely { by } => out.push(Info {
                key: InfoKey::Detached(id),
                level: Level::Warn,
                text: format!(
                    "{} closed this buffer in the edit service. The text here is read-only.",
                    by.as_deref().unwrap_or("Someone")
                ),
                actions: vec![("Keep as New Buffer".into(), InfoAction::KeepAsNew(id)), ("Close Tab".into(), InfoAction::CloseTab(id))],
            }),
            DetachReason::OpenFailed { msg } => out.push(Info {
                key: InfoKey::Detached(id),
                level: Level::Error,
                text: format!("Could not open: {msg}"),
                actions: vec![("Close Tab".into(), InfoAction::CloseTab(id))],
            }),
            DetachReason::EpochChanged => out.push(Info {
                key: InfoKey::Detached(id),
                level: Level::Info,
                text: "The edit service restarted — reattaching…".into(),
                actions: Vec::new(),
            }),
        }
    }
    if let Some(copy) = m.detached_copy() {
        let differ = copy.text.len().abs_diff(m.text().len());
        out.push(Info {
            key: InfoKey::DetachedCopy(id, copy.rev_seen),
            level: Level::Warn,
            text: format!(
                "The edit service's copy differs from yours ({} bytes yours, {} theirs{}).",
                copy.text.len(),
                m.text().len(),
                if differ == 0 { ", same length" } else { "" }
            ),
            actions: vec![
                ("Keep Mine".into(), InfoAction::KeepMine(id)),
                ("Take the Service's".into(), InfoAction::TakeService(id)),
                ("Save Mine As…".into(), InfoAction::SaveMineAs(id)),
            ],
        });
    }
    match meta.disk {
        DiskState::Modified => out.push(Info {
            key: InfoKey::DiskModified(id),
            level: Level::Warn,
            text: if meta.dirty {
                "The file changed on disk, and you have unsaved changes. Saving will ask before overwriting.".into()
            } else {
                "The file changed on disk.".into()
            },
            actions: vec![
                ("Reload".into(), InfoAction::Reload(id)),
                ("Ignore".into(), InfoAction::Dismiss(InfoKey::DiskModified(id))),
            ],
        }),
        DiskState::Deleted => out.push(Info {
            key: InfoKey::DiskDeleted(id),
            level: Level::Warn,
            text: "The file was deleted on disk. The text is still here.".into(),
            actions: vec![("Save".into(), InfoAction::Save(id)), ("Close Tab".into(), InfoAction::CloseTab(id))],
        }),
        _ => {}
    }
    for c in m.conflicts() {
        out.push(Info {
            key: InfoKey::Conflict(id, c.rev),
            level: Level::Warn,
            text: conflict_text(c),
            actions: vec![
                ("Show".into(), InfoAction::ShowConflict(id, c.rev)),
                ("Copy".into(), InfoAction::CopyConflict(id, c.rev)),
                ("Re-insert at Caret".into(), InfoAction::ReinsertConflict(id, c.rev)),
            ],
        });
    }
    out.retain(|i| !dismissed(&i.key));
    out
}

/// The strips.
pub fn view<'a>(look: Look, infos: Vec<Info>) -> Element<'a, Msg> {
    let t = look.tokens;
    let mut col = column![];
    for info in infos {
        let (edge, fill) = match info.level {
            Level::Error => (t.destructive, t.card),
            Level::Warn => (look.chrome.warning, t.card),
            Level::Info => (t.ring, t.card),
        };
        let mut buttons = row![].spacing(6).align_y(Alignment::Center);
        for (i, (label, action)) in info.actions.into_iter().enumerate() {
            let b = button(look.small(label)).padding(Padding::from([3, 10])).on_press(Msg::Info(action));
            buttons = buttons.push(if i == 0 { b.style(look.primary()) } else { b.style(look.secondary()) });
        }
        let close = button(look.text("×").color(t.muted_text))
            .padding(Padding::from([0, 6]))
            .style(look.flat())
            .on_press(Msg::Info(InfoAction::Dismiss(info.key.clone())));
        let body = row![
            look.text(info.text).width(Length::Fill),
            buttons,
            close
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        col = col.push(
            container(body)
                .padding(Padding { top: 6.0, bottom: 6.0, left: 12.0, right: 8.0 })
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(fill)),
                    text_color: Some(t.card_text),
                    border: Border { color: edge, width: 1.0, radius: 0.0.into() },
                    ..container::Style::default()
                }),
        );
    }
    col.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_wording_matches_the_plan() {
        let c = Conflict { rev: 44, remote_origin: Some("agent:ctl-90".into()), lines: (12, 18), texts: vec!["abc".into(), "defg".into()] };
        assert_eq!(
            conflict_text(&c),
            "agent:ctl-90 edited lines 12–18 while you were typing there; your 7 characters were not applied."
        );
        let one = Conflict { lines: (3, 3), texts: vec!["é".into()], ..c };
        assert_eq!(conflict_text(&one), "agent:ctl-90 edited line 3 while you were typing there; your character was not applied.");
    }
}
