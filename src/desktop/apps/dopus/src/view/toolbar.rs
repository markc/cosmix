//! One window-wide navigation strip, dispatched through the existing actions.
use cosmix_actions::{ActionId, filemgr};
use cosmix_dopus_core::PaneModel;
use iced::widget::{button, container, row};
use iced::{Element, Length};

use super::Look;
use crate::app::Msg;
use crate::icons::{Icon, Icons};

pub fn navigation<'a>(
    look: Look,
    icons: &Icons,
    tint: &str,
    active: &PaneModel,
    busy: bool,
    panels_open: [bool; 2],
    actions: &[crate::verbs::ActionRow],
) -> Element<'a, Msg> {
    let disabled_tint = crate::icons::hex(look.tokens.muted_text);
    let control = |icon, action, label| {
        let enabled = enabled(active, busy, action);
        let tint = if enabled { tint } else { &disabled_tint };
        let button = button(super::image_widget(look, icons, tint, icon))
            .padding(look.chrome.small)
            .on_press_maybe(enabled.then_some(Msg::Actions(vec![action])))
            .style(super::button_look(&look));
        super::tips::tip(
            look,
            button,
            super::tips::action_label(actions, action, label),
        )
    };
    let controls = row![
        control(Icon::ArrowLeft, filemgr::NAV_BACK, "Back"),
        control(Icon::ArrowRight, filemgr::NAV_FORWARD, "Forward"),
        control(Icon::ArrowUp, filemgr::NAV_PARENT, "Up"),
        control(Icon::House, filemgr::NAV_HOME, "Home"),
        control(Icon::Refresh, filemgr::VIEW_REFRESH, "Refresh"),
        control(Icon::FolderOpen, filemgr::FILE_OPEN, "Open"),
        control(Icon::Folder, filemgr::FILE_NEW_FOLDER, "New folder"),
        control(Icon::FileText, filemgr::FILE_RENAME, "Rename"),
        control(Icon::Copy, filemgr::FILE_COPY, "Copy to other pane"),
        control(
            Icon::MoveHorizontal,
            filemgr::FILE_MOVE,
            "Move to other pane"
        ),
        control(Icon::Trash, filemgr::FILE_DELETE, "Delete"),
        control(
            if active.show_hidden {
                Icon::EyeOff
            } else {
                Icon::Eye
            },
            filemgr::VIEW_TOGGLE_HIDDEN,
            if active.show_hidden {
                "Hide hidden files"
            } else {
                "Show hidden files"
            },
        ),
    ]
    .spacing(look.chrome.small)
    .align_y(iced::Alignment::Center);
    let panel = |icon, action, name: &str, open| {
        let tint = crate::icons::hex(if open {
            look.tokens.selection_text
        } else {
            look.tokens.muted_text
        });
        let button = button(super::image_widget(look, icons, &tint, icon))
            .padding(look.chrome.small)
            .on_press(Msg::Actions(vec![action]))
            .style(move |theme, status| {
                let mut style = super::button_look(&look)(theme, status);
                if open {
                    style.background = Some(look.tokens.selection.into());
                }
                style
            });
        let label = format!("{} {name}", if open { "Hide" } else { "Show" });
        super::tips::tip(
            look,
            button,
            super::tips::action_label(actions, action, &label),
        )
    };
    // Equal-sized edge buttons leave the navigation centred in the window,
    // independent of either sidebar's width or open state.
    container(
        row![
            panel(
                Icon::PanelLeft,
                cosmix_actions::view::TOGGLE_PLACES,
                "Places",
                panels_open[0]
            ),
            container(controls).center_x(Length::Fill),
            panel(
                Icon::PanelRight,
                cosmix_actions::view::TOGGLE_PROPERTIES,
                "Properties",
                panels_open[1]
            ),
        ]
        .align_y(iced::Alignment::Center),
    )
    .width(Length::Fill)
    .padding([look.chrome.small, look.chrome.pad])
    .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
    .into()
}

pub fn enabled(pane: &PaneModel, busy: bool, action: ActionId) -> bool {
    if action == filemgr::NAV_BACK {
        !pane.history.back.is_empty()
    } else if action == filemgr::NAV_FORWARD {
        !pane.history.forward.is_empty()
    } else if action == filemgr::NAV_PARENT {
        pane.path.parent().is_some()
    } else if action == filemgr::FILE_NEW_FOLDER {
        !busy
    } else if action == filemgr::FILE_OPEN {
        !pane.listing && pane.selected.is_some()
    } else if action == filemgr::FILE_RENAME {
        !busy && !pane.listing && pane.selected_paths.len() == 1
    } else if [filemgr::FILE_COPY, filemgr::FILE_MOVE, filemgr::FILE_DELETE].contains(&action) {
        !busy && !pane.listing && !pane.selected_paths.is_empty()
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_dopus_core::{DOpusConfig, DopusCore, PaneId};

    #[test]
    fn history_availability_follows_the_active_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = DOpusConfig::default();
        config.left.path = dir.path().to_owned();
        config.right.path = dir.path().to_owned();
        let (mut core, _events) = DopusCore::new(config, None);
        assert!(!enabled(core.pane(core.active()), false, filemgr::NAV_BACK));
        core.go_parent_in(PaneId::Left);
        assert!(enabled(core.pane(core.active()), false, filemgr::NAV_BACK));
        core.switch_pane();
        assert!(!enabled(core.pane(core.active()), false, filemgr::NAV_BACK));
        core.set_active_pane(PaneId::Left);
        core.go_back();
        assert!(!enabled(core.pane(core.active()), false, filemgr::NAV_BACK));
        assert!(enabled(
            core.pane(core.active()),
            false,
            filemgr::NAV_FORWARD
        ));
        core.navigate(PaneId::Right, "/".into());
        core.set_active_pane(PaneId::Right);
        assert!(!enabled(
            core.pane(core.active()),
            false,
            filemgr::NAV_PARENT
        ));
        assert!(enabled(
            core.pane(core.active()),
            false,
            filemgr::VIEW_REFRESH
        ));
    }
}
