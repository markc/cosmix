//! Dialog chrome and presentation state (scene-editor plan §4.3 Q2).
//!
//! A dialog is one centred overlay surface per host, not a carousel page.
//! [`QuoinDialog`] is the renderer-neutral state every party shares:
//!
//! - the scene adapter mirrors the seat (`seat`) and parents the accepted
//!   scene's content under the dialog chrome (`content`);
//! - the Bus verbs `shell.dialog.show|hide` and the frame controls (× and
//!   Escape) set `visible`;
//! - the host maps a surface for the chrome root (`root`) while
//!   [`QuoinDialog::wants_surface`] holds, and reports the Bevy window and
//!   output origin it mapped (`window`, `origin`), or `None` when it unmaps.
//!
//! × is a frame control: its hit area and handler belong to Quoin, not to the
//! scene, so a wedged scene behaviour can always be dismissed. Escape hides
//! the dialog whenever the dialog window holds the keyboard, except during an
//! IME preedit (tracked from the window's `Ime` messages).

use bevy::a11y::AccessibilityNode;
use bevy::ecs::observer::On;
use bevy::input::ButtonState;
use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::input_focus::InputFocus;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::{UiRect, percent, px};
use bevy::ui_widgets::{Activate, Button as WidgetButton};
use bevy::window::{Ime, RequestRedraw};
use ctk::theme::tokens;

use crate::core::{DialogSeat, OutputKey};

/// Height of the dialog title bar in logical px. Scene content starts below
/// it, so `shell.scene.layout` node rects are offset by this much on a
/// chromed dialog.
pub const DIALOG_TITLE_BAR_PX: f32 = 32.0;

/// Clear space kept between a fitted dialog and each side of the zone it
/// fits in, in logical px.
pub const DIALOG_FIT_MARGIN_PX: f32 = 24.0;

/// A fitted dialog never shrinks below this (the smallest size
/// cosmix-scene accepts for an authored dialog).
pub const DIALOG_MIN_PX: f32 = 240.0;

/// The dialog's actual size (Stage R, live nested gate): each authored side
/// is kept when it fits, else shrunk to the zone less a margin on both
/// sides, never below [`DIALOG_MIN_PX`]; whole logical px.
///
/// `w = max(240, min(declared_w, zone_w − 2·24))`, and likewise for `h`.
/// `zone` is the output less the exclusive zones of Quoin's docked panels
/// (`host::panel_layout(..).canvas`), the same area comp centres an
/// unanchored overlay in.
pub fn fit_dialog_size(declared: Vec2, zone: Vec2) -> Vec2 {
    let fit = |declared: f32, zone: f32| {
        declared
            .min((zone - 2.0 * DIALOG_FIT_MARGIN_PX).floor())
            .max(DIALOG_MIN_PX)
            .round()
    };
    Vec2::new(fit(declared.x, zone.x), fit(declared.y, zone.y))
}

/// Shared dialog state. See the module documentation for who writes what.
#[derive(Resource, Debug, Default)]
pub struct QuoinDialog {
    /// Mirror of the scene adapter's seat; `None` while no dialog is loaded.
    pub seat: Option<DialogSeat>,
    /// The dialog should be on screen. Cleared when the seat changes hands
    /// or empties, and by × and Escape.
    pub visible: bool,
    /// Chrome root: the entity a host renders. Spawned on first mount.
    pub root: Option<Entity>,
    /// Scene content currently parented in the chrome's content host.
    pub content: Option<Entity>,
    /// Bevy window of the mapped surface, while mapped (layer host).
    pub window: Option<Entity>,
    /// Output-logical position of the mapped surface's top-left corner.
    pub origin: Option<Vec2>,
    /// An IME preedit is active in the dialog window: Escape belongs to the
    /// IME. Tracked from `Ime` messages; cleared when the surface unmaps.
    pub preedit: bool,
    /// The size hosts map and notices report: the authored size fitted to
    /// the current zone ([`fit_dialog_size`]); `None` without a seat.
    pub fitted: Option<Vec2>,
    /// `window` also serves other shell surfaces (the embedded host's one
    /// output window). Keys and IME events then count for the dialog only
    /// while the input focus is inside the dialog root, so Escape in a panel
    /// field never closes the dialog.
    pub window_shared: bool,
}

/// `dialog` in `shell.props.get` / `shell.panel.changed`: the fields whose
/// change publishes a new revision.
#[derive(Clone, Debug, PartialEq)]
pub struct DialogNotice {
    pub scene: String,
    pub visible: bool,
    pub w: f32,
    pub h: f32,
    pub output: String,
}

impl QuoinDialog {
    /// Show the seated dialog. `Ok(applied)` is whether anything changed;
    /// `None` when `scene` does not hold the seat.
    pub fn show(&mut self, scene: &str) -> Option<bool> {
        self.seat.as_ref().filter(|seat| seat.scene == scene)?;
        let applied = !self.visible;
        self.visible = true;
        Some(applied)
    }

    /// Hide the seated dialog; the scene and its state are kept.
    pub fn hide(&mut self, scene: &str) -> Option<bool> {
        self.seat.as_ref().filter(|seat| seat.scene == scene)?;
        let applied = self.visible;
        self.visible = false;
        Some(applied)
    }

    /// Replace the seat mirror. A different holder (or none) starts hidden:
    /// visibility belongs to the scene that was shown, never to its
    /// successor.
    pub fn set_seat(&mut self, seat: Option<DialogSeat>) {
        let same_holder = match (&self.seat, &seat) {
            (Some(old), Some(new)) => old.scene == new.scene && old.owner == new.owner,
            _ => false,
        };
        if !same_holder {
            self.visible = false;
        }
        self.seat = seat;
    }

    /// Everything a host needs before mapping: a visible, seated dialog with
    /// mounted content under a chrome root.
    pub fn wants_surface(&self) -> bool {
        self.visible && self.seat.is_some() && self.root.is_some() && self.content.is_some()
    }

    /// Logical surface size a host maps: the fitted size once computed,
    /// else the authored size.
    pub fn size(&self) -> Option<Vec2> {
        self.seat
            .as_ref()
            .map(|seat| self.fitted.unwrap_or(Vec2::new(seat.w, seat.h)))
    }

    /// `w`/`h` are the actual (fitted) size, not the authored one.
    pub fn notice(&self) -> Option<DialogNotice> {
        let size = self.size()?;
        self.seat.as_ref().map(|seat| DialogNotice {
            scene: seat.scene.clone(),
            visible: self.visible,
            w: size.x,
            h: size.y,
            output: seat.output.as_str().to_owned(),
        })
    }

    /// The output the dialog maps on.
    pub fn output(&self) -> Option<&OutputKey> {
        self.seat.as_ref().map(|seat| &seat.output)
    }
}

/// The chrome root's parts.
#[derive(Component, Debug)]
pub struct QuoinDialogParts {
    pub title_bar: Entity,
    pub title_label: Entity,
    pub close: Entity,
    pub content_host: Entity,
}

/// The × frame control.
#[derive(Component, Debug)]
pub struct QuoinDialogClose;

pub(crate) fn install(app: &mut App) {
    app.init_resource::<QuoinDialog>()
        // Hosts with a WindowPlugin register it already; idempotent.
        .add_message::<Ime>()
        .add_observer(on_close)
        .add_systems(
            Update,
            escape_dialog
                .in_set(crate::runtime::ShellRuntimeSet::Input)
                .after(crate::runtime::ShellStagedIngress),
        )
        .add_systems(
            Update,
            fit_dialog
                .after(crate::runtime::ShellRuntimeSet::Model)
                .before(crate::runtime::ShellRuntimeSet::Presentation),
        )
        .add_systems(
            Update,
            present_dialog.in_set(crate::runtime::ShellRuntimeSet::Presentation),
        );
}

/// Re-fit after every model update: an output change or a docked panel's
/// exclusive zone moving changes the zone, and hosts remap at the new size.
fn fit_dialog(frame: Res<crate::runtime::ShellFrameState>, mut dialog: ResMut<QuoinDialog>) {
    let fitted = dialog.seat.as_ref().map(|seat| {
        let canvas = crate::host::panel_layout(&frame.0).canvas;
        fit_dialog_size(Vec2::new(seat.w, seat.h), Vec2::new(canvas.width, canvas.height))
    });
    if dialog.fitted != fitted {
        dialog.fitted = fitted;
    }
}

/// Spawn the chrome root once; later calls return the same root.
pub fn ensure_dialog_chrome(world: &mut World) -> Entity {
    if let Some(root) = world.resource::<QuoinDialog>().root
        && world.get_entity(root).is_ok()
    {
        return root;
    }
    let mut queue = bevy::ecs::world::CommandQueue::default();
    let mut commands = Commands::new(&mut queue, world);
    let title_label = super::text(
        &mut commands,
        "",
        13.0,
        false,
        Some(ctk::theme::CtkTextRole::Ui),
    );
    let close_label = super::text(&mut commands, "×", 16.0, false, None);
    let mut accessibility = accesskit::Node::new(accesskit::Role::Button);
    accessibility.set_label("Close");
    let close = commands
        .spawn((
            Node {
                width: px(28),
                height: px(24),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::CONTROL),
            WidgetButton,
            Pickable::default(),
            Hovered::default(),
            AccessibilityNode::from(accessibility),
            QuoinDialogClose,
        ))
        .add_child(close_label)
        .id();
    let spacer = commands
        .spawn((
            Node {
                flex_grow: 1.0,
                ..default()
            },
            Pickable::IGNORE,
        ))
        .id();
    let title_bar = commands
        .spawn((
            Node {
                height: px(DIALOG_TITLE_BAR_PX),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                padding: UiRect::axes(px(10), px(4)),
                column_gap: px(6),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::MASTER_PANEL),
        ))
        .add_children(&[title_label, spacer, close])
        .id();
    let content_host = commands
        .spawn(Node {
            min_width: px(0),
            min_height: px(0),
            flex_grow: 1.0,
            overflow: Overflow::clip(),
            ..default()
        })
        .id();
    let root = commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                border: UiRect::all(px(1)),
                display: Display::None,
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::PANEL),
            bevy::feathers::theme::ThemeBorderColor(tokens::BORDER),
            BorderColor::all(Color::NONE),
            Pickable::default(),
            Name::new("quoin-dialog"),
            QuoinDialogParts {
                title_bar,
                title_label,
                close,
                content_host,
            },
        ))
        .add_children(&[title_bar, content_host])
        .id();
    queue.apply(world);
    world.resource_mut::<QuoinDialog>().root = Some(root);
    root
}

/// Parent `content` under the dialog chrome, replacing any previous content,
/// and apply the authored title and chrome flag. Always succeeds: the chrome
/// is created on demand.
pub fn mount_dialog_content(world: &mut World, content: Entity, title: &str, chrome: bool) {
    let root = ensure_dialog_chrome(world);
    let parts = world.get::<QuoinDialogParts>(root).expect("dialog chrome parts");
    let (host, bar, label) = (parts.content_host, parts.title_bar, parts.title_label);
    let previous = world.resource::<QuoinDialog>().content;
    if let Some(previous) = previous.filter(|previous| *previous != content)
        && world.get_entity(previous).is_ok()
    {
        world.entity_mut(previous).remove::<ChildOf>();
    }
    if world.get::<ChildOf>(content).map(ChildOf::parent) != Some(host) {
        world.entity_mut(host).add_child(content);
    }
    if let Some(mut text) = world.get_mut::<Text>(label)
        && text.0 != title
    {
        text.0 = title.to_owned();
    }
    let display = if chrome { Display::Flex } else { Display::None };
    if let Some(mut node) = world.get_mut::<Node>(bar)
        && node.display != display
    {
        node.display = display;
    }
    world.resource_mut::<QuoinDialog>().content = Some(content);
}

/// Detach `content` if it is the dialog's; the caller despawns it. Hides the
/// dialog: there is nothing left to show.
pub fn unmount_dialog_content(world: &mut World, content: Entity) {
    let mut dialog = world.resource_mut::<QuoinDialog>();
    if dialog.content != Some(content) {
        return;
    }
    dialog.content = None;
    dialog.visible = false;
    if world.get_entity(content).is_ok() {
        world.entity_mut(content).remove::<ChildOf>();
    }
}

fn on_close(
    activated: On<Activate>,
    closes: Query<(), With<QuoinDialogClose>>,
    mut dialog: ResMut<QuoinDialog>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if closes.contains(activated.entity) && dialog.visible {
        dialog.visible = false;
        redraw.write(RequestRedraw);
    }
}

/// Escape hides the dialog while its window holds the keyboard. Keys reach
/// here only from a shell surface; a key for another window (a panel) is
/// not the dialog's. Only a fresh press counts, as for panels.
fn escape_dialog(
    mut ime: MessageReader<Ime>,
    mut keys: MessageReader<KeyboardInput>,
    mut dialog: ResMut<QuoinDialog>,
    focus: Option<Res<InputFocus>>,
    parents: Query<&ChildOf>,
) {
    let focus_inside = || {
        let (Some(root), Some(mut entity)) = (dialog.root, focus.as_ref().and_then(|f| f.get()))
        else {
            return false;
        };
        loop {
            if entity == root {
                return true;
            }
            match parents.get(entity) {
                Ok(parent) => entity = parent.parent(),
                Err(_) => return false,
            }
        }
    };
    let Some(window) = dialog.window.filter(|_| !dialog.window_shared || focus_inside()) else {
        ime.clear();
        keys.clear();
        return;
    };
    let mut preedit = dialog.preedit;
    for event in ime.read() {
        match event {
            Ime::Preedit { window: w, value, .. } if *w == window => preedit = !value.is_empty(),
            Ime::Commit { window: w, .. } | Ime::Disabled { window: w } if *w == window => {
                preedit = false;
            }
            _ => {}
        }
    }
    if dialog.preedit != preedit {
        dialog.preedit = preedit;
    }
    let escaped = keys.read().any(|key| {
        key.window == window
            && key.key_code == KeyCode::Escape
            && key.state == ButtonState::Pressed
            && !key.repeat
    });
    if escaped && dialog.visible && !dialog.preedit {
        dialog.visible = false;
    }
}

/// Keep the chrome root out of layout and picking while no surface shows it.
fn present_dialog(dialog: Res<QuoinDialog>, mut nodes: Query<&mut Node>) {
    let Some(root) = dialog.root else {
        return;
    };
    let display = if dialog.wants_surface() {
        Display::Flex
    } else {
        Display::None
    };
    if let Ok(mut node) = nodes.get_mut(root)
        && node.display != display
    {
        node.display = display;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(scene: &str, owner: &str) -> DialogSeat {
        DialogSeat {
            scene: scene.into(),
            owner: owner.into(),
            accepted_at: 1,
            output: OutputKey::new("DP-1").unwrap(),
            w: 880.0,
            h: 620.0,
            title: Some("Scene Editor".into()),
            chrome: true,
        }
    }

    #[test]
    fn show_and_hide_are_idempotent_and_scoped_to_the_seat() {
        let mut dialog = QuoinDialog::default();
        assert_eq!(dialog.show("editor"), None, "no seat, nothing to show");
        dialog.set_seat(Some(seat("editor", "scenes")));
        assert_eq!(dialog.show("other"), None);
        assert_eq!(dialog.show("editor"), Some(true));
        assert_eq!(dialog.show("editor"), Some(false));
        assert_eq!(dialog.hide("editor"), Some(true));
        assert_eq!(dialog.hide("editor"), Some(false));
        assert!(dialog.seat.is_some(), "hide keeps the seat");
    }

    #[test]
    fn a_new_holder_starts_hidden_and_a_reload_keeps_visibility() {
        let mut dialog = QuoinDialog::default();
        dialog.set_seat(Some(seat("editor", "scenes")));
        dialog.show("editor");
        let mut bigger = seat("editor", "scenes");
        bigger.w = 1000.0;
        dialog.set_seat(Some(bigger));
        assert!(dialog.visible, "same holder reloading stays visible");
        dialog.set_seat(Some(seat("other", "someone")));
        assert!(!dialog.visible, "a pre-empting holder is not shown by inheritance");
        dialog.show("other");
        dialog.set_seat(None);
        assert!(!dialog.visible);
        assert_eq!(dialog.notice(), None);
    }

    /// Live nested gate (1105×560 output): the 880×620 editor must fit.
    #[test]
    fn a_dialog_fits_the_zone_with_a_margin_and_keeps_its_size_when_it_fits() {
        let declared = Vec2::new(880.0, 620.0);
        assert_eq!(fit_dialog_size(declared, Vec2::new(1105.0, 560.0)), Vec2::new(880.0, 512.0));
        assert_eq!(fit_dialog_size(declared, Vec2::new(1920.0, 1080.0)), declared);
        assert_eq!(fit_dialog_size(declared, Vec2::new(928.0, 668.0)), declared, "exactly fits");
        assert_eq!(fit_dialog_size(declared, Vec2::new(927.0, 667.0)), Vec2::new(879.0, 619.0));
        assert_eq!(
            fit_dialog_size(declared, Vec2::new(300.0, 200.0)),
            Vec2::new(252.0, DIALOG_MIN_PX),
            "never below the minimum"
        );
    }

    #[test]
    fn the_fitted_size_is_what_notices_report() {
        let mut app = app();
        let mut wide = seat("editor", "scenes");
        wide.w = 980.0;
        app.world_mut().resource_mut::<QuoinDialog>().set_seat(Some(wide));
        app.update();
        let dialog = app.world().resource::<QuoinDialog>();
        // The 1000×800 test output leaves 952 px of width after the margins.
        assert_eq!(dialog.fitted, Some(Vec2::new(952.0, 620.0)));
        assert_eq!(dialog.size(), Some(Vec2::new(952.0, 620.0)));
        let notice = dialog.notice().unwrap();
        assert_eq!((notice.w, notice.h), (952.0, 620.0));
        app.world_mut().resource_mut::<QuoinDialog>().set_seat(None);
        app.update();
        assert_eq!(app.world().resource::<QuoinDialog>().fitted, None);
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(crate::runtime::ShellRuntimePlugin::new(
                crate::core::ShellModel::new(
                    OutputKey::new("DP-1").unwrap(),
                    crate::core::LogicalSize::new(1000.0, 800.0).unwrap(),
                    std::time::Duration::ZERO,
                    std::time::Duration::from_millis(800),
                    std::time::Duration::from_millis(200),
                )
                .unwrap(),
            ))
            .add_message::<KeyboardInput>()
            .add_message::<RequestRedraw>();
        install(&mut app);
        app
    }

    fn escape(window: Entity, repeat: bool) -> KeyboardInput {
        KeyboardInput {
            key_code: KeyCode::Escape,
            logical_key: bevy::input::keyboard::Key::Escape,
            state: ButtonState::Pressed,
            text: None,
            repeat,
            window,
        }
    }

    #[test]
    fn escape_hides_only_from_the_dialog_window_and_never_in_preedit() {
        let mut app = app();
        let window = app.world_mut().spawn_empty().id();
        let panel = app.world_mut().spawn_empty().id();
        {
            let mut dialog = app.world_mut().resource_mut::<QuoinDialog>();
            dialog.set_seat(Some(seat("editor", "scenes")));
            dialog.show("editor");
            dialog.window = Some(window);
        }
        for key in [escape(panel, false), escape(window, true)] {
            app.world_mut().write_message(key);
            app.update();
            assert!(app.world().resource::<QuoinDialog>().visible);
        }
        app.world_mut().write_message(Ime::Preedit {
            window,
            value: "ni".into(),
            cursor: Some((2, 2)),
        });
        app.world_mut().write_message(escape(window, false));
        app.update();
        assert!(app.world().resource::<QuoinDialog>().preedit);
        assert!(app.world().resource::<QuoinDialog>().visible, "preedit owns Escape");
        app.world_mut().write_message(Ime::Commit {
            window,
            value: "你".into(),
        });
        app.world_mut().write_message(escape(window, false));
        app.update();
        assert!(!app.world().resource::<QuoinDialog>().visible);
    }

    /// Stage R round 2 (Opus): on the embedded host the output window is
    /// shared with every panel, so Escape (and the IME guard) belong to the
    /// dialog only while the input focus is inside the dialog root.
    #[test]
    fn a_shared_window_escape_needs_focus_inside_the_dialog() {
        let mut app = app();
        app.init_resource::<InputFocus>();
        let window = app.world_mut().spawn_empty().id();
        let content = app.world_mut().spawn(Node::default()).id();
        let field = app.world_mut().spawn(Node::default()).id();
        app.world_mut().entity_mut(content).add_child(field);
        mount_dialog_content(app.world_mut(), content, "Scene Editor", true);
        let panel_field = app.world_mut().spawn(Node::default()).id();
        {
            let mut dialog = app.world_mut().resource_mut::<QuoinDialog>();
            dialog.set_seat(Some(seat("editor", "scenes")));
            dialog.show("editor");
            dialog.window = Some(window);
            dialog.window_shared = true;
        }
        for focused in [None, Some(panel_field)] {
            *app.world_mut().resource_mut::<InputFocus>() =
                focused.map_or_else(InputFocus::default, InputFocus::from_entity);
            app.world_mut().write_message(Ime::Preedit {
                window,
                value: "ni".into(),
                cursor: None,
            });
            app.world_mut().write_message(escape(window, false));
            app.update();
            let dialog = app.world().resource::<QuoinDialog>();
            assert!(dialog.visible, "{focused:?}: a panel's Escape is not the dialog's");
            assert!(!dialog.preedit, "{focused:?}: a panel's preedit is not the dialog's");
        }
        *app.world_mut().resource_mut::<InputFocus>() = InputFocus::from_entity(field);
        app.world_mut().write_message(escape(window, false));
        app.update();
        assert!(!app.world().resource::<QuoinDialog>().visible);
    }

    #[test]
    fn close_control_hides_and_mount_replaces_content() {
        let mut app = app();
        let content = app.world_mut().spawn(Node::default()).id();
        mount_dialog_content(app.world_mut(), content, "Scene Editor", true);
        let root = app.world().resource::<QuoinDialog>().root.unwrap();
        let parts = app.world().get::<QuoinDialogParts>(root).unwrap();
        let (close, host, label) = (parts.close, parts.content_host, parts.title_label);
        assert_eq!(app.world().get::<ChildOf>(content).unwrap().parent(), host);
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "Scene Editor");
        {
            let mut dialog = app.world_mut().resource_mut::<QuoinDialog>();
            dialog.set_seat(Some(seat("editor", "scenes")));
            dialog.show("editor");
        }
        app.update();
        assert_eq!(app.world().get::<Node>(root).unwrap().display, Display::Flex);
        app.world_mut().trigger(Activate { entity: close });
        assert!(!app.world().resource::<QuoinDialog>().visible);
        app.update();
        assert_eq!(app.world().get::<Node>(root).unwrap().display, Display::None);

        let replacement = app.world_mut().spawn(Node::default()).id();
        mount_dialog_content(app.world_mut(), replacement, "Other", false);
        assert!(app.world().get::<ChildOf>(content).is_none());
        assert_eq!(app.world().resource::<QuoinDialog>().content, Some(replacement));
        let bar = app.world().get::<QuoinDialogParts>(root).unwrap().title_bar;
        assert_eq!(app.world().get::<Node>(bar).unwrap().display, Display::None);
        unmount_dialog_content(app.world_mut(), replacement);
        assert_eq!(app.world().resource::<QuoinDialog>().content, None);
        assert!(app.world().get::<ChildOf>(replacement).is_none());
    }
}
