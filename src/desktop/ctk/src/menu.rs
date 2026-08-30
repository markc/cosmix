//! A CTK menu bar: titles across the top, dropdown panels, click-away close.
//!
//! Bevy 0.19 does ship menu machinery — headless
//! `bevy_ui_widgets::{MenuButton, MenuPopup, MenuItem}` plus a styled
//! `bevy_feathers::controls::menu` — but the feathers layer is built on the
//! experimental `bsn!` scene macro, which doesn't compose with CTK's
//! imperative spawn style, and the headless layer's focus-loss dismissal
//! machinery is still settling. This module is the CTK-idiom equivalent: an
//! app declares its menus as data ([`MenuDef`]), spawns the bar once, and
//! observes [`MenuActivated`] for item ids. Migrating onto the feathers menu
//! when bsn stabilises only replaces the internals — the `MenuDef` →
//! `MenuActivated` contract stays.

use std::borrow::Cow;
use std::collections::BTreeMap;

use bevy::app::{App, Plugin, Update};
use bevy::color::Alpha;
#[cfg(any(feature = "actions", feature = "icons"))]
use bevy::ecs::change_detection::DetectChanges;
use bevy::ecs::entity::Entity;
use bevy::ecs::event::EntityEvent;
use bevy::ecs::hierarchy::ChildOf;
#[cfg(feature = "actions")]
use bevy::ecs::message::{Message, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With};
use bevy::ecs::system::SystemParam;
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme};
use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::input::ButtonState;
use bevy::input_focus::{FocusCause, FocusedInput, InputFocus};
use bevy::math::Vec2;
use bevy::picking::events::{Click, Pointer, Press};
use bevy::picking::hover::Hovered;
#[cfg(any(feature = "actions", feature = "icons"))]
use bevy::prelude::AlignItems;
#[cfg(feature = "actions")]
use bevy::prelude::Ref;
use bevy::prelude::{
    default, BackgroundColor, BorderColor, Color, Component, Display, GlobalZIndex, Node,
    PositionType, Resource, Text, TextFont,
};
use bevy::ui::{percent, px, FlexDirection, FocusPolicy, UiRect};

#[cfg(feature = "actions")]
use cosmix_actions::{ActionArgs, ActionId, ActionIdError, ActionRegistry, Keymap};

#[cfg(feature = "icons")]
use crate::icons::{spawn_icon, Icon, IconSet, SvgColor, ThemeSvgColor};

use crate::theme::{ctk_color, tokens};
use bevy::feathers::theme::ThemeBorderColor;

/// One activatable entry in a menu.
pub struct MenuItemDef {
    /// Stable action id delivered by [`MenuActivated`].
    ///
    /// The runtime shape remains `&'static str` for consumers that do not
    /// enable `actions`; with that feature, [`MenuItemDef::action_id`] exposes
    /// the same value as a [`cosmix_actions::ActionId`].
    pub id: &'static str,
    /// Human-readable entry label. Accelerators are never stored here.
    ///
    /// `"Label".into()` remains valid in struct literals with every feature
    /// combination; icon state is private inside [`MenuItemLabel`].
    pub label: MenuItemLabel,
}

impl MenuItemDef {
    /// Construct a literal-backed item without presentation state or an icon.
    ///
    /// This is `const` so applications can declare static menu tables. Dynamic
    /// labels can use [`MenuItemDef::with_dynamic_label`].
    pub const fn new(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label: MenuItemLabel::new(label),
        }
    }

    /// Construct an item whose label is owned at runtime.
    pub fn with_dynamic_label(id: &'static str, label: impl Into<String>) -> Self {
        Self {
            id,
            label: MenuItemLabel::from(label.into()),
        }
    }

    /// Validate and return this menu id in the shared action-id vocabulary.
    ///
    /// Invalid literals return an error rather than panicking menu construction.
    #[cfg(feature = "actions")]
    pub fn action_id(&self) -> Result<ActionId, ActionIdError> {
        validated_action_id(self.id)
    }

    /// Attach a catalogue icon.
    #[cfg(feature = "icons")]
    pub fn with_icon(mut self, icon: Icon) -> Self {
        self.label.icon = Some(icon);
        self
    }
}

/// Menu label storage with feature-private decoration data.
///
/// Keeping the optional icon inside this wrapper means existing
/// `MenuItemDef { id, label: "...".into() }` literals continue to compile when
/// the `icons` feature is enabled.
pub struct MenuItemLabel {
    text: Cow<'static, str>,
    #[cfg(feature = "icons")]
    icon: Option<Icon>,
}

impl MenuItemLabel {
    const fn new(text: &'static str) -> Self {
        Self {
            text: Cow::Borrowed(text),
            #[cfg(feature = "icons")]
            icon: None,
        }
    }

    /// Borrow the visible label text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    #[cfg(feature = "icons")]
    const fn icon(&self) -> Option<Icon> {
        self.icon
    }
}

impl From<&'static str> for MenuItemLabel {
    fn from(text: &'static str) -> Self {
        Self::new(text)
    }
}

impl From<String> for MenuItemLabel {
    fn from(text: String) -> Self {
        Self {
            text: Cow::Owned(text),
            #[cfg(feature = "icons")]
            icon: None,
        }
    }
}

/// One titled menu of entries.
pub struct MenuDef {
    /// Title shown in the bar.
    pub label: String,
    /// Entries shown in its dropdown.
    pub items: Vec<MenuItemDef>,
}

/// Checked/radio decoration derived from current application state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuItemMarker {
    /// No leading state marker.
    #[default]
    None,
    /// A checked toggle item.
    Checked,
    /// The selected member of a radio group.
    Radio,
}

impl MenuItemMarker {
    #[cfg(feature = "actions")]
    const fn display(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Checked => "✓",
            Self::Radio => "●",
        }
    }
}

/// Bevy resource exposing the app-local action registry to CTK adapters.
///
/// Menus, accelerator presentation and the optional Bus action port share this
/// one registry, so enabled predicates and metadata cannot drift by ingress.
#[cfg(feature = "actions")]
#[derive(Resource, Default)]
pub struct ActionRegistryResource {
    registry: ActionRegistry,
    enabled_revision: u64,
}

#[cfg(feature = "actions")]
impl ActionRegistryResource {
    /// Wrap an app-local action registry for CTK dispatch adapters.
    pub const fn new(registry: ActionRegistry) -> Self {
        Self {
            registry,
            enabled_revision: 0,
        }
    }

    /// Borrow the underlying registry.
    pub const fn registry(&self) -> &ActionRegistry {
        &self.registry
    }

    /// Mutably borrow the underlying registry.
    pub fn registry_mut(&mut self) -> &mut ActionRegistry {
        &mut self.registry
    }

    /// Mark app-owned state read by enabled predicates as changed.
    ///
    /// Query adapters cache predicate results by this revision. Apps must call
    /// this after changing any state captured by an enabled predicate.
    pub fn mark_enabled_changed(&mut self) {
        self.enabled_revision = self.enabled_revision.saturating_add(1);
    }

    /// Revision of app-owned enabled-predicate inputs.
    pub const fn enabled_revision(&self) -> u64 {
        self.enabled_revision
    }

    fn is_enabled(&self, id: &'static str) -> bool {
        validated_action_id(id)
            .ok()
            .and_then(|action| self.registry.is_enabled(action))
            .unwrap_or(false)
    }
}

/// Compatibility name for the action registry resource introduced with menu
/// bridging. New code should use [`ActionRegistryResource`].
#[cfg(feature = "actions")]
pub type MenuActionRegistry = ActionRegistryResource;

#[cfg(feature = "actions")]
#[derive(SystemParam)]
struct MenuActionAuthority<'w, 's> {
    registry: Option<Res<'w, MenuActionRegistry>>,
    bridged_bars: Query<'w, 's, Ref<'static, ActionBridgeBar>>,
}

#[cfg(feature = "actions")]
impl MenuActionAuthority<'_, '_> {
    fn is_bridged(&self, bar: Entity) -> bool {
        self.bridged_bars.contains(bar)
    }

    fn bridge_changed(&self, bar: Entity) -> bool {
        self.bridged_bars
            .get(bar)
            .is_ok_and(|marker| marker.is_changed())
    }

    fn registry_enabled(&self, id: &'static str) -> bool {
        self.registry
            .as_deref()
            .is_some_and(|registry| registry.is_enabled(id))
    }

    fn registry_changed(&self) -> bool {
        self.registry
            .as_ref()
            .is_some_and(|registry| registry.is_changed())
    }

    fn registry_present(&self) -> bool {
        self.registry.is_some()
    }
}

#[derive(SystemParam)]
struct MenuDispatchAuthority<'w, 's> {
    presentation: Res<'w, MenuPresentation>,
    #[cfg(feature = "actions")]
    actions: MenuActionAuthority<'w, 's>,
    #[cfg(not(feature = "actions"))]
    _legacy: Query<'w, 's, ()>,
}

impl MenuDispatchAuthority<'_, '_> {
    fn enabled(&self, entry: &MenuEntry) -> bool {
        let presentation_enabled = self.presentation.item(entry.id).enabled;
        #[cfg(feature = "actions")]
        {
            presentation_enabled
                && (!self.actions.is_bridged(entry.bar) || self.actions.registry_enabled(entry.id))
        }
        #[cfg(not(feature = "actions"))]
        {
            presentation_enabled
        }
    }
}

/// One mismatch reported by [`validate_menu_against_registry`].
#[cfg(feature = "actions")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuValidationIssue {
    /// The declared id is outside the shared action-id grammar.
    InvalidActionId {
        /// Declared menu id.
        id: &'static str,
        /// Grammar or length failure.
        error: ActionIdError,
    },
    /// The id is valid but has no app-local registry entry.
    Unregistered {
        /// Declared menu id.
        id: &'static str,
    },
    /// The action requires arguments but Phase 2 menu rows invoke with none.
    RequiresArguments {
        /// Declared menu id.
        id: &'static str,
    },
}

/// Validate every menu item against the app-local registry without interning.
///
/// Phase 2 menu rows are nullary invocations: they always emit an empty
/// [`ActionArgs`] bag. This returns all invalid, unregistered, and required-arg
/// actions so applications can reject an unusable definition during start-up.
#[cfg(feature = "actions")]
pub fn validate_menu_against_registry(
    menus: &[MenuDef],
    registry: &ActionRegistry,
) -> Result<(), Vec<MenuValidationIssue>> {
    let mut issues = Vec::new();
    for item in menus.iter().flat_map(|menu| &menu.items) {
        match item.action_id() {
            Ok(action) => match registry.metadata(action) {
                None => issues.push(MenuValidationIssue::Unregistered { id: item.id }),
                Some(meta) if meta.args_schema.fields.iter().any(|field| field.required) => {
                    issues.push(MenuValidationIssue::RequiresArguments { id: item.id });
                }
                Some(_) => {}
            },
            Err(error) => issues.push(MenuValidationIssue::InvalidActionId { id: item.id, error }),
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

#[cfg(feature = "actions")]
fn validated_action_id(id: &'static str) -> Result<ActionId, ActionIdError> {
    ActionId::validate_str(id)?;
    Ok(ActionId::from_static(id))
}

/// Reactive presentation for one menu action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuItemPresentation {
    /// Whether the item should look and behave enabled.
    ///
    /// Without `actions` this is necessarily a best-effort dispatch gate. With
    /// `actions` and [`MenuActionRegistry`], the live predicate is authoritative.
    pub enabled: bool,
    /// Current checked/radio decoration.
    pub marker: MenuItemMarker,
}

impl Default for MenuItemPresentation {
    fn default() -> Self {
        Self {
            enabled: true,
            marker: MenuItemMarker::None,
        }
    }
}

/// Revision-keyed enabled and checked/radio menu presentation.
///
/// Apps publish a fresh revision when action predicates or checked state may
/// have changed. Menu rows compare that revision and update in place; the bar
/// is never respawned.
#[derive(Resource, Default)]
pub struct MenuPresentation {
    revision: u64,
    items: BTreeMap<&'static str, MenuItemPresentation>,
}

impl MenuPresentation {
    /// Construct one published presentation snapshot.
    pub fn new(
        revision: u64,
        items: impl IntoIterator<Item = (&'static str, MenuItemPresentation)>,
    ) -> Self {
        Self {
            revision,
            items: items.into_iter().collect(),
        }
    }

    /// Replace all states and publish the supplied application/theme revision.
    pub fn replace(
        &mut self,
        revision: u64,
        items: impl IntoIterator<Item = (&'static str, MenuItemPresentation)>,
    ) {
        self.revision = revision;
        self.items = items.into_iter().collect();
    }

    /// Current publication revision.
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Presentation for an id; omitted ids remain enabled and unmarked.
    pub fn item(&self, id: &'static str) -> MenuItemPresentation {
        self.items.get(id).copied().unwrap_or_default()
    }

    /// Evaluate registry enabled predicates into one reactive snapshot.
    ///
    /// `markers` supplies app-owned checked/radio state, normally keyed by the
    /// same theme/app-state revision passed here.
    #[cfg(feature = "actions")]
    pub fn from_registry(
        revision: u64,
        registry: &ActionRegistry,
        markers: impl IntoIterator<Item = (ActionId, MenuItemMarker)>,
    ) -> Self {
        let markers: BTreeMap<_, _> = markers.into_iter().collect();
        let items = registry.iter_metadata().map(|meta| {
            (
                meta.id.as_str(),
                MenuItemPresentation {
                    enabled: registry.is_enabled(meta.id).unwrap_or(false),
                    marker: markers.get(&meta.id).copied().unwrap_or_default(),
                },
            )
        });
        Self::new(revision, items)
    }
}

/// Revision-keyed keymap used for reactive menu accelerator hints.
#[cfg(feature = "actions")]
#[derive(Resource)]
pub struct MenuKeymap {
    revision: u64,
    keymap: Keymap,
}

#[cfg(feature = "actions")]
impl MenuKeymap {
    /// Publish a keymap at an app-owned hot-reload revision.
    pub const fn new(revision: u64, keymap: Keymap) -> Self {
        Self { revision, keymap }
    }

    /// Replace the keymap and revision without respawning menus.
    pub fn replace(&mut self, revision: u64, keymap: Keymap) {
        self.revision = revision;
        self.keymap = keymap;
    }

    /// Current hot-reload revision.
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Current layered keymap.
    pub const fn keymap(&self) -> &Keymap {
        &self.keymap
    }
}

/// An entry was chosen. `id` is the [`MenuItemDef::id`] the app declared.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuActivated {
    /// The menu bar the entry belongs to.
    #[event_target]
    pub bar: Entity,
    pub id: &'static str,
    /// Focus held before the menu's first pointer press cleared it. Consumers
    /// that open a modal can restore this entity after validating liveness.
    pub invocation_focus: Option<Entity>,
    /// How this activation was produced.
    #[cfg(feature = "actions")]
    pub origin: MenuActivationOrigin,
}

/// Provenance attached to [`MenuActivated`] before optional action bridging.
#[cfg(feature = "actions")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuActivationOrigin {
    /// A pointer activated an actual menu row.
    Pointer,
    /// Code activated a menu action and supplies its honest ingress source.
    Programmatic(Source),
}

/// Origin of a generic [`ActionRequest`].
#[cfg(feature = "actions")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Engine-normalised keyboard input.
    Key,
    /// Pointer or other direct manipulation outside a menu.
    Mouse,
    /// CTK menu activation.
    Menu,
    /// Bus application ingress.
    Bus,
    /// MIDI command ingress.
    Midi,
    /// OSC command ingress.
    Osc,
}

/// Generic Bevy action-bus request emitted by CTK menu activation.
///
/// Other input adapters may write the same message with a different
/// [`Source`]. Menu requests preserve the focus captured before menu chrome
/// cleared it, so modal-opening handlers can restore focus on close.
#[cfg(feature = "actions")]
#[derive(Message, Clone, Debug, PartialEq)]
pub struct ActionRequest {
    /// Shared action identifier.
    pub action: ActionId,
    /// Ingress provenance.
    pub source: Source,
    /// Typed invocation arguments. Menu rows publish an empty bag.
    pub args: ActionArgs,
    /// Focus captured before the menu opened.
    pub invocation_focus: Option<Entity>,
}

/// Marker: the bar root.
#[derive(Component)]
pub struct MenuBar;

/// Opt one menu bar into registry-authoritative action bridging.
///
/// Insert this on the entity returned by [`spawn_menu_bar`]. Unmarked bars
/// continue emitting legacy [`MenuActivated`] events and are ignored by
/// [`ActionBridgePlugin`], allowing bar-by-bar migration.
#[cfg(feature = "actions")]
#[derive(Component, Default)]
pub struct ActionBridgeBar;

/// A menu title button; `dropdown` is the panel it toggles.
#[derive(Component)]
struct MenuTitle {
    dropdown: Entity,
    bar: Entity,
}

/// Invocation context retained while one bar's dropdowns are being browsed.
/// The first title press captures focus; moving between titles must not replace
/// it with the `None` produced by the menu's own non-focusable chrome.
#[derive(Component, Default)]
struct MenuInvocationFocus {
    focus: Option<Entity>,
    active: bool,
}

/// Marker: a dropdown panel.
#[derive(Component)]
struct MenuDropdown;

/// One pointer-positioned context menu.
#[derive(Component)]
pub struct ContextMenu {
    entries: Vec<Entity>,
    invocation_focus: Option<Entity>,
}

#[derive(Component)]
struct ContextMenuEntry {
    menu: Entity,
}

#[derive(Resource, Default)]
struct ContextMenuState {
    open: Option<Entity>,
}

#[derive(SystemParam)]
struct ContextMenuKeyboard<'w, 's> {
    context_entries: Query<'w, 's, &'static ContextMenuEntry>,
    contexts: Query<'w, 's, &'static ContextMenu>,
    entries: Query<'w, 's, &'static MenuEntry>,
    authority: MenuDispatchAuthority<'w, 's>,
    state: ResMut<'w, ContextMenuState>,
    focus: ResMut<'w, InputFocus>,
    commands: Commands<'w, 's>,
}

type MenuPointerTarget = (
    Option<&'static MenuTitle>,
    Option<&'static MenuEntry>,
    Has<MenuDropdown>,
    Has<ContextMenu>,
);

#[derive(SystemParam)]
struct MenuPointerDispatch<'w, 's> {
    targets: Query<'w, 's, MenuPointerTarget>,
    parents: Query<'w, 's, &'static ChildOf>,
    dropdowns: Query<'w, 's, &'static mut Node, With<MenuDropdown>>,
    invocations: Query<'w, 's, (Entity, &'static mut MenuInvocationFocus)>,
    contexts: Query<'w, 's, (), With<ContextMenu>>,
    context_state: ResMut<'w, ContextMenuState>,
    commands: Commands<'w, 's>,
    authority: MenuDispatchAuthority<'w, 's>,
}

/// An entry row; carries its id + owning bar for the [`MenuActivated`] event.
#[derive(Component)]
struct MenuEntry {
    id: &'static str,
    bar: Entity,
    label: Entity,
    #[cfg(feature = "actions")]
    marker: Entity,
    #[cfg(feature = "icons")]
    icon: Option<Entity>,
    #[cfg(feature = "actions")]
    accelerator: Entity,
    /// Cached styling/hover state only; dispatch reads `MenuPresentation` live.
    enabled: bool,
    presentation_revision: Option<u64>,
}

/// Non-predicate inputs that invalidate one row's cached presentation.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MenuPresentationContext {
    #[cfg(feature = "actions")]
    bridged: bool,
    #[cfg(feature = "actions")]
    registry_present: bool,
    #[cfg(feature = "icons")]
    theme_present: bool,
}

/// Revision last rendered by one accelerator text entity.
#[cfg(feature = "actions")]
#[derive(Component)]
struct MenuAccelerator {
    action: Option<ActionId>,
    revision: Option<u64>,
}

const FONT_SIZE: f32 = 13.0;
/// Dropdowns float above every view.
const DROPDOWN_Z: i32 = 100;
/// Context menus sit above ordinary app chrome and menu-bar dropdowns, while
/// remaining below CTK file-requester and interaction modal layers (1,000+).
pub const CONTEXT_MENU_Z: i32 = 200;

/// Spawn a menu bar across its container's width. Observe [`MenuActivated`]
/// (globally or on the returned entity) for item choices.
pub fn spawn_menu_bar(commands: &mut Commands, menus: &[MenuDef]) -> Entity {
    #[cfg(feature = "icons")]
    {
        spawn_menu_bar_inner(commands, menus, None)
    }
    #[cfg(not(feature = "icons"))]
    {
        spawn_menu_bar_inner(commands, menus)
    }
}

/// Spawn a menu bar with catalogue icons coloured from [`UiTheme`].
///
/// Items whose `icon` is `None` retain an aligned empty icon column. This is
/// separate from [`spawn_menu_bar`] so existing non-icon call sites keep their
/// signature when the `icons` feature is enabled elsewhere in an app.
#[cfg(feature = "icons")]
pub fn spawn_menu_bar_with_icons(
    commands: &mut Commands,
    menus: &[MenuDef],
    icons: &IconSet,
    theme: &UiTheme,
) -> Entity {
    spawn_menu_bar_inner(commands, menus, Some((icons, theme)))
}

/// Spawn an action-menu at a pointer position.
///
/// [`MenuBarPlugin`] keeps at most one context menu open, dismisses it on
/// outside click or Escape, and dispatches rows through the same
/// [`MenuActivated`] / action-registry path as menu-bar entries.
pub fn spawn_context_menu(
    commands: &mut Commands,
    items: &[MenuItemDef],
    position: Vec2,
    invocation_focus: Option<Entity>,
) -> Entity {
    spawn_context_menu_inner(
        commands,
        items,
        position,
        invocation_focus,
        #[cfg(feature = "icons")]
        None,
    )
}

/// Spawn a pointer-positioned context menu with catalogue icons.
#[cfg(feature = "icons")]
pub fn spawn_context_menu_with_icons(
    commands: &mut Commands,
    items: &[MenuItemDef],
    position: Vec2,
    invocation_focus: Option<Entity>,
    icons: &IconSet,
    theme: &UiTheme,
) -> Entity {
    spawn_context_menu_inner(
        commands,
        items,
        position,
        invocation_focus,
        Some((icons, theme)),
    )
}

fn spawn_context_menu_inner(
    commands: &mut Commands,
    items: &[MenuItemDef],
    position: Vec2,
    invocation_focus: Option<Entity>,
    #[cfg(feature = "icons")] icon_resources: Option<(&IconSet, &UiTheme)>,
) -> Entity {
    let menu = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(position.x),
                top: px(position.y),
                width: px(210),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(5)),
                border: UiRect::all(px(1)),
                ..default()
            },
            ThemeBackgroundColor(tokens::MASTER_PANEL),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::BORDER),
            GlobalZIndex(CONTEXT_MENU_Z),
            FocusPolicy::Block,
            MenuInvocationFocus {
                focus: invocation_focus,
                active: true,
            },
        ))
        .id();
    #[cfg(feature = "actions")]
    commands.entity(menu).insert(ActionBridgeBar);
    let entries = spawn_menu_entries(
        commands,
        items,
        menu,
        menu,
        #[cfg(feature = "icons")]
        icon_resources,
        true,
    );
    commands.entity(menu).insert(ContextMenu {
        entries,
        invocation_focus,
    });
    menu
}

fn spawn_menu_bar_inner(
    commands: &mut Commands,
    menus: &[MenuDef],
    #[cfg(feature = "icons")] icon_resources: Option<(&IconSet, &UiTheme)>,
) -> Entity {
    let bar = commands
        .spawn((
            Node {
                width: percent(100),
                flex_direction: FlexDirection::Row,
                column_gap: px(2),
                padding: UiRect::axes(px(4), px(2)),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            MenuBar,
            MenuInvocationFocus::default(),
        ))
        .id();

    for menu in menus {
        // Anchor: relative so the absolute dropdown hangs off this title.
        let anchor = commands
            .spawn((Node {
                position_type: PositionType::Relative,
                ..default()
            },))
            .id();
        let dropdown = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    top: percent(100),
                    left: px(0),
                    min_width: px(190),
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::all(px(4)),
                    display: Display::None,
                    ..default()
                },
                ThemeBackgroundColor(tokens::MASTER_PANEL),
                GlobalZIndex(DROPDOWN_Z),
                MenuDropdown,
            ))
            .id();
        let title = commands
            .spawn((
                Node {
                    padding: UiRect::axes(px(10), px(5)),
                    ..default()
                },
                BackgroundColor(Color::NONE),
                Hovered::default(),
                MenuTitle { dropdown, bar },
            ))
            .id();
        let title_text = commands
            .spawn((
                Text::new(menu.label.clone()),
                TextFont::from_font_size(FONT_SIZE),
                ThemeTextColor(tokens::TEXT),
            ))
            .id();
        commands.entity(title).add_children(&[title_text]);

        spawn_menu_entries(
            commands,
            &menu.items,
            bar,
            dropdown,
            #[cfg(feature = "icons")]
            icon_resources,
            false,
        );

        commands.entity(anchor).add_children(&[title, dropdown]);
        commands.entity(bar).add_children(&[anchor]);
    }
    bar
}

fn spawn_menu_entries(
    commands: &mut Commands,
    items: &[MenuItemDef],
    bar: Entity,
    parent: Entity,
    #[cfg(feature = "icons")] icon_resources: Option<(&IconSet, &UiTheme)>,
    context: bool,
) -> Vec<Entity> {
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let entry = commands
            .spawn((
                Node {
                    padding: UiRect::axes(px(10), px(6)),
                    width: percent(100),
                    #[cfg(any(feature = "actions", feature = "icons"))]
                    column_gap: px(6),
                    #[cfg(any(feature = "actions", feature = "icons"))]
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(Color::NONE),
                Hovered::default(),
            ))
            .id();
        #[cfg(feature = "actions")]
        let marker = commands
            .spawn((
                Node {
                    min_width: px(14),
                    ..default()
                },
                Text::new(""),
                TextFont::from_font_size(FONT_SIZE),
                ThemeTextColor(tokens::TEXT),
            ))
            .id();
        #[cfg(feature = "icons")]
        let (icon_slot, menu_icon) = {
            if let Some((icons, theme)) = icon_resources {
                let slot = commands
                    .spawn((Node {
                        min_width: px(14),
                        height: px(14),
                        align_items: AlignItems::Center,
                        ..default()
                    },))
                    .id();
                let menu_icon = item.label.icon().map(|icon| {
                    let icon = spawn_icon(commands, icons, theme, icon, 14.0, tokens::TEXT);
                    commands.entity(slot).add_children(&[icon]);
                    icon
                });
                (Some(slot), menu_icon)
            } else {
                (None, None)
            }
        };
        let label = commands
            .spawn((
                Text::new(item.label.as_str().to_owned()),
                TextFont::from_font_size(FONT_SIZE),
                ThemeTextColor(tokens::TEXT),
            ))
            .id();
        #[cfg(feature = "actions")]
        let spacer = commands
            .spawn((Node {
                flex_grow: 1.0,
                ..default()
            },))
            .id();
        #[cfg(feature = "actions")]
        let accelerator = commands
            .spawn((
                Text::new(""),
                TextFont::from_font_size(FONT_SIZE),
                ThemeTextColor(tokens::TEXT_DIM),
                MenuAccelerator {
                    action: match item.action_id() {
                        Ok(action) => Some(action),
                        Err(error) => {
                            bevy::log::warn!(
                                "menu item `{}` has an invalid action id and is inert: {error}",
                                item.id
                            );
                            None
                        }
                    },
                    revision: None,
                },
            ))
            .id();
        commands.entity(entry).insert(MenuEntry {
            id: item.id,
            bar,
            label,
            #[cfg(feature = "actions")]
            marker,
            #[cfg(feature = "icons")]
            icon: menu_icon,
            #[cfg(feature = "actions")]
            accelerator,
            enabled: true,
            presentation_revision: None,
        });
        if context {
            commands
                .entity(entry)
                .insert(ContextMenuEntry { menu: bar });
        }
        let children: Vec<_> = [
            #[cfg(feature = "actions")]
            Some(marker),
            #[cfg(feature = "icons")]
            icon_slot,
            Some(label),
            #[cfg(feature = "actions")]
            Some(spacer),
            #[cfg(feature = "actions")]
            Some(accelerator),
        ]
        .into_iter()
        .flatten()
        .collect();
        commands.entity(entry).add_children(&children);
        commands.entity(parent).add_children(&[entry]);
        entries.push(entry);
    }
    entries
}

/// Menu behaviour: open/close, activation, click-away, hover highlight.
pub struct MenuBarPlugin;

impl Plugin for MenuBarPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MenuPresentation>()
            .init_resource::<ContextMenuState>()
            .add_observer(on_menu_pointer_press)
            .add_observer(on_menu_pointer_click)
            .add_observer(on_context_menu_key_input)
            .add_systems(
                Update,
                (
                    enforce_single_context_menu,
                    sync_menu_presentation,
                    hover_highlight,
                ),
            );
        #[cfg(feature = "actions")]
        app.add_systems(Update, sync_menu_accelerators);
    }
}

/// Opt-in adapter from [`MenuActivated`] to the generic [`ActionRequest`] bus.
///
/// Enabling this plugin is a migration boundary: applications must stop
/// observing `MenuActivated` for action dispatch on bars marked
/// [`ActionBridgeBar`]. Unmarked bars remain legacy. A marked bar requires
/// [`MenuActionRegistry`] and emits only presentation-enabled, registered, and
/// currently predicate-enabled actions.
#[cfg(feature = "actions")]
pub struct ActionBridgePlugin;

#[cfg(feature = "actions")]
impl Plugin for ActionBridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MenuPresentation>()
            .add_message::<ActionRequest>()
            .add_observer(bridge_menu_activation);
    }
}

fn enforce_single_context_menu(
    added: Query<(Entity, &ContextMenu), Added<ContextMenu>>,
    entries: Query<&MenuEntry>,
    authority: MenuDispatchAuthority,
    mut state: ResMut<ContextMenuState>,
    mut focus: ResMut<InputFocus>,
    mut dropdowns: Query<&mut Node, With<MenuDropdown>>,
    mut commands: Commands,
) {
    for (menu, context) in &added {
        if let Some(previous) = state.open.replace(menu) {
            if previous != menu && commands.get_entity(previous).is_ok() {
                commands.entity(previous).despawn();
            }
        }
        close_all(&mut dropdowns);
        if let Some(first) = context.entries.iter().copied().find(|entry| {
            entries
                .get(*entry)
                .is_ok_and(|entry| authority.enabled(entry))
        }) {
            focus.set(first, FocusCause::Navigated);
        }
    }
}

fn on_context_menu_key_input(
    mut event: On<FocusedInput<KeyboardInput>>,
    mut context: ContextMenuKeyboard,
) {
    if event.input.state != ButtonState::Pressed {
        return;
    }
    let Some(open_menu) = context.state.open else {
        return;
    };
    let Ok(menu) = context.contexts.get(open_menu) else {
        return;
    };
    if event.input.key_code == KeyCode::Escape {
        event.propagate(false);
        let restore = menu.invocation_focus;
        dismiss_context_menu(&mut context.commands, &mut context.state);
        if let Some(entity) = restore.filter(|entity| context.commands.get_entity(*entity).is_ok())
        {
            context.focus.set(entity, FocusCause::Navigated);
        } else {
            context.focus.clear();
        }
        return;
    }
    let Ok(context_entry) = context.context_entries.get(event.focused_entity) else {
        return;
    };
    if context_entry.menu != open_menu {
        return;
    }
    event.propagate(false);
    match event.input.key_code {
        KeyCode::Escape => unreachable!("Escape is handled before entry routing"),
        KeyCode::ArrowUp | KeyCode::ArrowDown => {
            let Some(current) = menu
                .entries
                .iter()
                .position(|entry| *entry == event.focused_entity)
            else {
                return;
            };
            let len = menu.entries.len();
            for offset in 1..=menu.entries.len() {
                let index = if event.input.key_code == KeyCode::ArrowDown {
                    (current + offset) % len
                } else {
                    (current + len - (offset % len)) % len
                };
                let candidate = menu.entries[index];
                if context
                    .entries
                    .get(candidate)
                    .is_ok_and(|entry| context.authority.enabled(entry))
                {
                    context.focus.set(candidate, FocusCause::Navigated);
                    break;
                }
            }
        }
        KeyCode::Enter if !event.input.repeat => {
            let Ok(entry) = context.entries.get(event.focused_entity) else {
                return;
            };
            if !context.authority.enabled(entry) {
                return;
            }
            context.commands.trigger(MenuActivated {
                bar: entry.bar,
                id: entry.id,
                invocation_focus: menu.invocation_focus,
                #[cfg(feature = "actions")]
                origin: MenuActivationOrigin::Programmatic(Source::Key),
            });
            dismiss_context_menu(&mut context.commands, &mut context.state);
        }
        _ => {}
    }
}

fn dismiss_context_menu(commands: &mut Commands, state: &mut ContextMenuState) {
    if let Some(menu) = state.open.take() {
        if commands.get_entity(menu).is_ok() {
            commands.entity(menu).despawn();
        }
    }
}

fn sync_menu_presentation(
    presentation: Res<MenuPresentation>,
    mut entries: Query<(Entity, &mut MenuEntry, Option<&MenuPresentationContext>)>,
    #[cfg(feature = "actions")] mut texts: Query<&mut Text>,
    mut commands: Commands,
    #[cfg(feature = "actions")] authority: MenuActionAuthority,
    #[cfg(feature = "icons")] theme: Option<Res<UiTheme>>,
    #[cfg(feature = "icons")] mut icons: Query<(&mut SvgColor, &mut ThemeSvgColor)>,
) {
    #[cfg(feature = "actions")]
    let registry_changed = authority.registry_changed();
    #[cfg(feature = "icons")]
    let theme_changed = theme.as_ref().is_some_and(|theme| theme.is_changed());
    for (entity, mut entry, previous_context) in &mut entries {
        let context = MenuPresentationContext {
            #[cfg(feature = "actions")]
            bridged: authority.is_bridged(entry.bar),
            #[cfg(feature = "actions")]
            registry_present: authority.registry_present(),
            #[cfg(feature = "icons")]
            theme_present: theme.is_some(),
        };
        #[cfg(feature = "actions")]
        let action_inputs_changed = registry_changed || authority.bridge_changed(entry.bar);
        #[cfg(not(feature = "actions"))]
        let action_inputs_changed = false;
        #[cfg(feature = "icons")]
        let theme_inputs_changed = theme_changed;
        #[cfg(not(feature = "icons"))]
        let theme_inputs_changed = false;
        let invalidated = entry.presentation_revision != Some(presentation.revision())
            || previous_context != Some(&context)
            || action_inputs_changed
            || theme_inputs_changed;
        if !invalidated {
            continue;
        }

        #[cfg(feature = "actions")]
        let mut state = presentation.item(entry.id);
        #[cfg(not(feature = "actions"))]
        let state = presentation.item(entry.id);
        #[cfg(feature = "actions")]
        if authority.is_bridged(entry.bar) {
            state.enabled &= authority.registry_enabled(entry.id);
        }
        entry.enabled = state.enabled;
        entry.presentation_revision = Some(presentation.revision());
        commands.entity(entity).insert(context);
        let token = if state.enabled {
            tokens::TEXT
        } else {
            tokens::TEXT_DIM
        };
        #[cfg(feature = "actions")]
        {
            if let Ok(mut marker) = texts.get_mut(entry.marker) {
                marker.0 = state.marker.display().to_owned();
            }
            commands
                .entity(entry.marker)
                .insert(ThemeTextColor(token.clone()));
        }
        commands
            .entity(entry.label)
            .insert(ThemeTextColor(token.clone()));
        #[cfg(feature = "icons")]
        if let (Some(icon), Some(theme)) = (entry.icon, theme.as_deref()) {
            if let Ok((mut colour, mut theme_colour)) = icons.get_mut(icon) {
                theme_colour.0 = token.clone();
                colour.0 = ctk_color(theme, &token);
            }
        }
        #[cfg(feature = "actions")]
        commands
            .entity(entry.accelerator)
            .insert(ThemeTextColor(if state.enabled {
                tokens::TEXT_DIM
            } else {
                token
            }));
    }
}

#[cfg(feature = "actions")]
fn sync_menu_accelerators(
    keymap: Option<Res<MenuKeymap>>,
    mut accelerators: Query<(&mut MenuAccelerator, &mut Text)>,
) {
    for (mut accelerator, mut text) in &mut accelerators {
        let revision = keymap.as_ref().map(|keymap| keymap.revision());
        if accelerator.revision == revision {
            continue;
        }
        text.0 = keymap
            .as_ref()
            .and_then(|keymap| {
                accelerator
                    .action
                    .and_then(|action| keymap.keymap().binding_for(action))
            })
            .unwrap_or_default();
        accelerator.revision = revision;
    }
}

#[cfg(feature = "actions")]
fn bridge_menu_activation(
    activation: On<MenuActivated>,
    registry: Option<Res<MenuActionRegistry>>,
    presentation: Res<MenuPresentation>,
    bridged_bars: Query<(), With<ActionBridgeBar>>,
    mut requests: MessageWriter<ActionRequest>,
) {
    if !bridged_bars.contains(activation.bar) {
        return;
    }
    let Ok(action) = validated_action_id(activation.id) else {
        bevy::log::warn!(
            "ignored menu activation with invalid action id `{}`",
            activation.id
        );
        return;
    };
    let Some(registry) = registry else {
        bevy::log::warn!("ignored menu activation `{action}`: no MenuActionRegistry resource");
        return;
    };
    let Some(meta) = registry.registry().metadata(action) else {
        bevy::log::warn!("ignored unregistered menu action `{action}`");
        return;
    };
    if meta.args_schema.fields.iter().any(|field| field.required) {
        bevy::log::warn!(
            "ignored menu action `{action}`: Phase 2 menu items cannot supply required arguments"
        );
        return;
    }
    if !presentation.item(activation.id).enabled
        || registry.registry().is_enabled(action) != Some(true)
    {
        bevy::log::warn!(
            "ignored presentation-disabled, predicate-disabled, or unregistered menu action `{action}`"
        );
        return;
    }
    requests.write(ActionRequest {
        action,
        source: match activation.origin {
            MenuActivationOrigin::Pointer => Source::Menu,
            MenuActivationOrigin::Programmatic(source) => source,
        },
        args: ActionArgs::new(),
        invocation_focus: activation.invocation_focus,
    });
}

/// Snapshot the invoker on the initial title press. Bevy's click-to-focus
/// observer queues focus clearing from this same `Press`; reading here sees
/// the pre-menu focus before that deferred change is applied.
fn on_menu_pointer_press(
    press: On<Pointer<Press>>,
    titles: Query<&MenuTitle>,
    parents: Query<&ChildOf>,
    focus: Res<InputFocus>,
    mut invocations: Query<&mut MenuInvocationFocus>,
) {
    if press.entity != press.original_event_target() {
        return;
    }
    let mut entity = press.original_event_target();
    loop {
        if let Ok(title) = titles.get(entity) {
            if let Ok(mut invocation) = invocations.get_mut(title.bar) {
                if !invocation.active {
                    invocation.focus = focus.get();
                    invocation.active = true;
                }
            }
            return;
        }
        match parents.get(entity) {
            Ok(parent) => entity = parent.parent(),
            Err(_) => return,
        }
    }
}

/// One decision per pointer click (taken on the first bubble hop): a title
/// toggles its dropdown, an entry activates + closes, anything else closes
/// every open dropdown (click-away).
fn on_menu_pointer_click(click: On<Pointer<Click>>, mut dispatch: MenuPointerDispatch) {
    // Bubbling re-triggers global observers per hop; act exactly once.
    if click.entity != click.original_event_target() {
        return;
    }
    // Walk up from the hit entity to find what was clicked.
    let mut entity = click.original_event_target();
    loop {
        if let Ok((Some(title), _, _, _)) = dispatch.targets.get(entity) {
            dismiss_context_menu(&mut dispatch.commands, &mut dispatch.context_state);
            let was_open = dispatch
                .dropdowns
                .get(title.dropdown)
                .map(|node| node.display != Display::None)
                .unwrap_or(false);
            close_all(&mut dispatch.dropdowns);
            reset_invocations(&mut dispatch.invocations, Some(title.bar));
            if !was_open {
                if let Ok(mut node) = dispatch.dropdowns.get_mut(title.dropdown) {
                    node.display = Display::Flex;
                }
            } else if let Ok((_, mut invocation)) = dispatch.invocations.get_mut(title.bar) {
                *invocation = default();
            }
            return;
        }
        if let Ok((_, Some(entry), _, _)) = dispatch.targets.get(entity) {
            if !dispatch.authority.enabled(entry) {
                return;
            }
            close_all(&mut dispatch.dropdowns);
            let invocation_focus =
                dispatch
                    .invocations
                    .get_mut(entry.bar)
                    .ok()
                    .and_then(|(_, invocation)| {
                        invocation.active.then_some(invocation.focus).flatten()
                    });
            reset_invocations(&mut dispatch.invocations, None);
            dispatch.commands.trigger(MenuActivated {
                bar: entry.bar,
                id: entry.id,
                invocation_focus,
                #[cfg(feature = "actions")]
                origin: MenuActivationOrigin::Pointer,
            });
            if dispatch.contexts.contains(entry.bar) {
                dismiss_context_menu(&mut dispatch.commands, &mut dispatch.context_state);
            }
            return;
        }
        if dispatch
            .targets
            .get(entity)
            .is_ok_and(|(_, _, dropdown, context)| dropdown || context)
        {
            return; // dropdown chrome (padding) — keep it open
        }
        match dispatch.parents.get(entity) {
            Ok(parent) => entity = parent.parent(),
            Err(_) => break,
        }
    }
    close_all(&mut dispatch.dropdowns);
    reset_invocations(&mut dispatch.invocations, None);
    dismiss_context_menu(&mut dispatch.commands, &mut dispatch.context_state);
}

fn close_all(dropdowns: &mut Query<&mut Node, With<MenuDropdown>>) {
    for mut node in dropdowns.iter_mut() {
        node.display = Display::None;
    }
}

/// Clear every bar's invocation state except an optional bar that remains
/// active while the pointer moves between titles in that same bar.
fn reset_invocations(
    invocations: &mut Query<(Entity, &mut MenuInvocationFocus)>,
    keep: Option<Entity>,
) {
    for (bar, mut invocation) in invocations.iter_mut() {
        if keep != Some(bar) {
            *invocation = default();
        }
    }
}

/// Highlight hovered titles/entries (bevy_picking maintains `Hovered`).
#[allow(clippy::type_complexity)]
fn hover_highlight(
    theme: Res<UiTheme>,
    focus: Res<InputFocus>,
    mut hoverables: Query<
        (Entity, &Hovered, &mut BackgroundColor, Option<&MenuEntry>),
        bevy::ecs::query::Or<(With<MenuTitle>, With<MenuEntry>)>,
    >,
) {
    // A translucent accent wash over the (themed) menu panel — reads as a
    // highlight in both light and dark while the panel showing through keeps
    // the label readable, so no per-mode text flip is needed. Re-read each
    // frame so a live theme swap recolours the hover too.
    let hover = ctk_color(&theme, &tokens::CONTROL_ACTIVE).with_alpha(0.22);
    for (entity, hovered, mut bg, entry) in hoverables.iter_mut() {
        let active = hovered.get() || focus.get() == Some(entity);
        let want = if active && entry.is_none_or(|entry| entry.enabled) {
            hover
        } else {
            Color::NONE
        };
        if bg.0 != want {
            bg.0 = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::time::Duration;

    use bevy::camera::NormalizedRenderTarget;
    #[cfg(feature = "actions")]
    use bevy::ecs::message::MessageReader;
    use bevy::input_focus::{FocusCause, InputFocus};
    use bevy::math::Vec2;
    use bevy::picking::backend::HitData;
    use bevy::picking::events::{Click, Pointer, Press};
    use bevy::picking::pointer::{Location, PointerButton, PointerId};
    use bevy::prelude::{ResMut, Resource};
    use bevy::window::WindowRef;

    #[derive(Resource, Default)]
    struct SeenActivations(Vec<MenuActivated>);

    fn record_activation(activation: On<MenuActivated>, mut seen: ResMut<SeenActivations>) {
        seen.0.push(*activation);
    }

    #[cfg(feature = "actions")]
    #[derive(Resource, Default)]
    struct SeenRequests(Vec<ActionRequest>);

    #[cfg(feature = "actions")]
    fn record_requests(mut requests: MessageReader<ActionRequest>, mut seen: ResMut<SeenRequests>) {
        seen.0.extend(requests.read().cloned());
    }

    #[cfg(feature = "actions")]
    fn test_registry(id: &'static str, enabled: bool) -> MenuActionRegistry {
        test_registry_with_schema(id, enabled, cosmix_actions::ArgsSchema::default())
    }

    #[cfg(feature = "actions")]
    fn test_registry_with_schema(
        id: &'static str,
        enabled: bool,
        args_schema: cosmix_actions::ArgsSchema,
    ) -> MenuActionRegistry {
        let mut registry = ActionRegistry::new();
        registry
            .register(
                cosmix_actions::ActionMeta {
                    id: ActionId::from_static(id),
                    label: id.to_owned(),
                    args_schema,
                    category: None,
                    icon_name: None,
                    description: None,
                    interactive: None,
                    allowed_sources: cosmix_actions::ActionSources::default(),
                },
                std::sync::Arc::new(|_| Ok(())),
                std::sync::Arc::new(move || enabled),
            )
            .unwrap();
        MenuActionRegistry::new(registry)
    }

    fn pointer_location(target: Entity) -> Location {
        Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(target).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        }
    }

    #[cfg(feature = "actions")]
    #[test]
    fn presentation_revision_updates_enabled_and_marker_without_respawn() {
        let mut app = App::new();
        app.init_resource::<MenuPresentation>()
            .add_systems(Update, sync_menu_presentation);
        let marker = app.world_mut().spawn(Text::new("")).id();
        let label = app.world_mut().spawn(Text::new("Theme")).id();
        let accelerator = app.world_mut().spawn(Text::new("")).id();
        let entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "theme-ocean",
                bar: Entity::PLACEHOLDER,
                label,
                marker,
                #[cfg(feature = "icons")]
                icon: None,
                #[cfg(feature = "actions")]
                accelerator,
                enabled: true,
                presentation_revision: None,
            })
            .id();
        app.world_mut().resource_mut::<MenuPresentation>().replace(
            7,
            [(
                "theme-ocean",
                MenuItemPresentation {
                    enabled: false,
                    marker: MenuItemMarker::Checked,
                },
            )],
        );
        app.update();

        let world = app.world();
        assert!(!world.entity(entry).get::<MenuEntry>().unwrap().enabled);
        assert_eq!(world.entity(marker).get::<Text>().unwrap().0, "✓");
        assert!(world.entity(label).get::<ThemeTextColor>().unwrap().0 == tokens::TEXT_DIM);

        app.world_mut().resource_mut::<MenuPresentation>().replace(
            8,
            [(
                "theme-ocean",
                MenuItemPresentation {
                    enabled: true,
                    marker: MenuItemMarker::Radio,
                },
            )],
        );
        app.update();
        let world = app.world();
        assert!(world.entity(entry).get::<MenuEntry>().unwrap().enabled);
        assert_eq!(world.entity(marker).get::<Text>().unwrap().0, "●");
        assert!(world.entity(label).get::<ThemeTextColor>().unwrap().0 == tokens::TEXT);
    }

    #[cfg(feature = "actions")]
    #[test]
    fn presentation_sync_tracks_bridge_marker_removal_without_revision_change() {
        let mut app = App::new();
        app.init_resource::<MenuPresentation>()
            .add_systems(Update, sync_menu_presentation)
            .insert_resource(test_registry("settings", false));
        let bar = app.world_mut().spawn(ActionBridgeBar).id();
        let marker = app.world_mut().spawn(Text::new("")).id();
        let label = app.world_mut().spawn(Text::new("Settings")).id();
        let accelerator = app.world_mut().spawn(Text::new("")).id();
        let entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "settings",
                bar,
                label,
                marker,
                #[cfg(feature = "icons")]
                icon: None,
                accelerator,
                enabled: true,
                presentation_revision: Some(0),
            })
            .id();

        app.update();
        assert!(
            !app.world()
                .entity(entry)
                .get::<MenuEntry>()
                .unwrap()
                .enabled
        );

        app.world_mut().entity_mut(bar).remove::<ActionBridgeBar>();
        app.update();
        assert!(
            app.world()
                .entity(entry)
                .get::<MenuEntry>()
                .unwrap()
                .enabled
        );
    }

    #[cfg(feature = "actions")]
    #[test]
    fn presentation_sync_tracks_registry_insertion_without_revision_change() {
        let mut app = App::new();
        app.init_resource::<MenuPresentation>()
            .add_systems(Update, sync_menu_presentation);
        let bar = app.world_mut().spawn(ActionBridgeBar).id();
        let marker = app.world_mut().spawn(Text::new("")).id();
        let label = app.world_mut().spawn(Text::new("Settings")).id();
        let accelerator = app.world_mut().spawn(Text::new("")).id();
        let entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "settings",
                bar,
                label,
                marker,
                #[cfg(feature = "icons")]
                icon: None,
                accelerator,
                enabled: true,
                presentation_revision: Some(0),
            })
            .id();

        app.update();
        assert!(
            !app.world()
                .entity(entry)
                .get::<MenuEntry>()
                .unwrap()
                .enabled
        );

        app.insert_resource(test_registry("settings", true));
        app.update();
        assert!(
            app.world()
                .entity(entry)
                .get::<MenuEntry>()
                .unwrap()
                .enabled
        );
    }

    #[cfg(feature = "actions")]
    #[test]
    fn presentation_sync_does_not_poll_predicates_without_invalidation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let predicate_calls = Arc::clone(&calls);
        let mut registry = ActionRegistry::new();
        registry
            .register(
                cosmix_actions::ActionMeta {
                    id: ActionId::from_static("settings"),
                    label: "Settings".into(),
                    args_schema: cosmix_actions::ArgsSchema::default(),
                    category: None,
                    icon_name: None,
                    description: None,
                    interactive: None,
                    allowed_sources: cosmix_actions::ActionSources::default(),
                },
                Arc::new(|_| Ok(())),
                Arc::new(move || {
                    predicate_calls.fetch_add(1, Ordering::Relaxed);
                    true
                }),
            )
            .unwrap();

        let mut app = App::new();
        app.init_resource::<MenuPresentation>()
            .add_systems(Update, sync_menu_presentation)
            .insert_resource(MenuActionRegistry::new(registry));
        let bar = app.world_mut().spawn(ActionBridgeBar).id();
        let marker = app.world_mut().spawn(Text::new("")).id();
        let label = app.world_mut().spawn(Text::new("Settings")).id();
        let accelerator = app.world_mut().spawn(Text::new("")).id();
        app.world_mut().spawn(MenuEntry {
            id: "settings",
            bar,
            label,
            marker,
            #[cfg(feature = "icons")]
            icon: None,
            accelerator,
            enabled: true,
            presentation_revision: None,
        });

        app.update();
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        app.update();
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        app.world_mut()
            .resource_mut::<MenuPresentation>()
            .replace(1, []);
        app.update();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[cfg(feature = "actions")]
    #[test]
    fn accelerator_hint_tracks_keymap_revision_without_respawn() {
        let mut app = App::new();
        app.add_systems(Update, sync_menu_accelerators);
        let action = ActionId::from_static("song-save");
        let hint = app
            .world_mut()
            .spawn((
                Text::new(""),
                MenuAccelerator {
                    action: Some(action),
                    revision: None,
                },
            ))
            .id();
        let ctrl_s = cosmix_actions::parse_keymap(
            "{ version: 1, chord_timeout_ms: 1000, defaults: [{ action: \"song-save\", chord: [\"Ctrl+S\"] }], custom: [] }",
        )
        .unwrap();
        app.insert_resource(MenuKeymap::new(1, ctrl_s));
        app.update();
        assert_eq!(app.world().entity(hint).get::<Text>().unwrap().0, "Ctrl+S");

        let ctrl_shift_s = cosmix_actions::parse_keymap(
            "{ version: 1, chord_timeout_ms: 1000, defaults: [{ action: \"song-save\", chord: [\"Ctrl+Shift+S\"] }], custom: [] }",
        )
        .unwrap();
        app.world_mut()
            .resource_mut::<MenuKeymap>()
            .replace(1, ctrl_shift_s.clone());
        app.update();
        assert_eq!(app.world().entity(hint).get::<Text>().unwrap().0, "Ctrl+S");

        app.world_mut()
            .resource_mut::<MenuKeymap>()
            .replace(2, ctrl_shift_s);
        app.update();
        assert_eq!(
            app.world().entity(hint).get::<Text>().unwrap().0,
            "Ctrl+Shift+S"
        );

        app.world_mut()
            .resource_mut::<MenuKeymap>()
            .replace(3, Keymap::default());
        app.update();
        assert!(app.world().entity(hint).get::<Text>().unwrap().0.is_empty());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn menu_activation_bridges_to_action_request_with_focus() {
        let mut app = App::new();
        app.add_plugins(ActionBridgePlugin)
            .init_resource::<SeenRequests>()
            .add_systems(Update, record_requests);
        app.insert_resource(test_registry("settings", true));
        let bar = app.world_mut().spawn(ActionBridgeBar).id();
        let invocation_focus = app.world_mut().spawn_empty().id();

        app.world_mut().trigger(MenuActivated {
            bar,
            id: "settings",
            invocation_focus: Some(invocation_focus),
            origin: MenuActivationOrigin::Pointer,
        });
        app.update();

        assert_eq!(
            app.world().resource::<SeenRequests>().0,
            [ActionRequest {
                action: ActionId::from_static("settings"),
                source: Source::Menu,
                args: ActionArgs::new(),
                invocation_focus: Some(invocation_focus),
            }]
        );
    }

    #[cfg(feature = "actions")]
    #[test]
    fn programmatic_menu_activation_preserves_honest_source() {
        let mut app = App::new();
        app.add_plugins(ActionBridgePlugin)
            .init_resource::<SeenRequests>()
            .add_systems(Update, record_requests)
            .insert_resource(test_registry("settings", true));
        let bar = app.world_mut().spawn(ActionBridgeBar).id();

        app.world_mut().trigger(MenuActivated {
            bar,
            id: "settings",
            invocation_focus: None,
            origin: MenuActivationOrigin::Programmatic(Source::Key),
        });
        app.update();

        assert_eq!(
            app.world().resource::<SeenRequests>().0[0].source,
            Source::Key
        );
    }

    #[cfg(feature = "actions")]
    #[test]
    fn bridge_rechecks_disabled_predicate_for_direct_activation() {
        let mut app = App::new();
        app.add_plugins(ActionBridgePlugin)
            .init_resource::<SeenRequests>()
            .add_systems(Update, record_requests)
            .insert_resource(test_registry("settings", false));
        let bar = app.world_mut().spawn(ActionBridgeBar).id();

        app.world_mut().trigger(MenuActivated {
            bar,
            id: "settings",
            invocation_focus: None,
            origin: MenuActivationOrigin::Programmatic(Source::Key),
        });
        app.update();

        assert!(app.world().resource::<SeenRequests>().0.is_empty());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn bridge_requires_presentation_and_registry_to_enable_action() {
        let mut app = App::new();
        app.add_plugins(ActionBridgePlugin)
            .init_resource::<SeenRequests>()
            .add_systems(Update, record_requests)
            .insert_resource(test_registry("settings", true));
        app.world_mut().resource_mut::<MenuPresentation>().replace(
            1,
            [(
                "settings",
                MenuItemPresentation {
                    enabled: false,
                    marker: MenuItemMarker::None,
                },
            )],
        );
        let bar = app.world_mut().spawn(ActionBridgeBar).id();

        app.world_mut().trigger(MenuActivated {
            bar,
            id: "settings",
            invocation_focus: None,
            origin: MenuActivationOrigin::Programmatic(Source::Key),
        });
        app.update();

        assert!(app.world().resource::<SeenRequests>().0.is_empty());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn bridge_refuses_actions_with_required_arguments() {
        let mut app = App::new();
        app.add_plugins(ActionBridgePlugin)
            .init_resource::<SeenRequests>()
            .add_systems(Update, record_requests)
            .insert_resource(test_registry_with_schema(
                "mixer.gain",
                true,
                cosmix_actions::ArgsSchema {
                    fields: vec![cosmix_actions::ActionArg {
                        name: "value".into(),
                        kind: cosmix_actions::ActionArgKind::Number,
                        required: true,
                        description: None,
                    }],
                    allow_extra: false,
                },
            ));
        let bar = app.world_mut().spawn(ActionBridgeBar).id();

        app.world_mut().trigger(MenuActivated {
            bar,
            id: "mixer.gain",
            invocation_focus: None,
            origin: MenuActivationOrigin::Programmatic(Source::Key),
        });
        app.update();

        assert!(app.world().resource::<SeenRequests>().0.is_empty());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn bridge_is_not_installed_by_menu_bar_plugin() {
        use bevy::ecs::message::Messages;

        let mut app = App::new();
        app.add_plugins(MenuBarPlugin);
        assert!(!app.world().contains_resource::<Messages<ActionRequest>>());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn menu_validation_reports_invalid_and_unregistered_ids() {
        let menus = [MenuDef {
            label: "File".into(),
            items: vec![
                MenuItemDef::new("settings", "Settings"),
                MenuItemDef::new("not registered", "Invalid"),
                MenuItemDef::new("missing", "Missing"),
            ],
        }];
        let registry = test_registry("settings", true);

        assert_eq!(
            validate_menu_against_registry(&menus, registry.registry()),
            Err(vec![
                MenuValidationIssue::InvalidActionId {
                    id: "not registered",
                    error: ActionIdError::InvalidCharacter,
                },
                MenuValidationIssue::Unregistered { id: "missing" },
            ])
        );
    }

    #[cfg(feature = "actions")]
    #[test]
    fn menu_validation_rejects_actions_with_required_arguments() {
        let menus = [MenuDef {
            label: "Mixer".into(),
            items: vec![MenuItemDef::new("mixer.gain", "Set gain")],
        }];
        let registry = test_registry_with_schema(
            "mixer.gain",
            true,
            cosmix_actions::ArgsSchema {
                fields: vec![cosmix_actions::ActionArg {
                    name: "value".into(),
                    kind: cosmix_actions::ActionArgKind::Number,
                    required: true,
                    description: None,
                }],
                allow_extra: false,
            },
        );

        assert_eq!(
            validate_menu_against_registry(&menus, registry.registry()),
            Err(vec![MenuValidationIssue::RequiresArguments {
                id: "mixer.gain"
            }])
        );
    }

    #[cfg(feature = "actions")]
    #[test]
    fn presentation_snapshot_evaluates_registry_predicates() {
        let mut registry = ActionRegistry::new();
        registry
            .register(
                cosmix_actions::ActionMeta {
                    id: ActionId::from_static("theme-ocean"),
                    label: "Ocean".into(),
                    args_schema: cosmix_actions::ArgsSchema::default(),
                    category: Some("theme".into()),
                    icon_name: None,
                    description: None,
                    interactive: None,
                    allowed_sources: cosmix_actions::ActionSources::default(),
                },
                std::sync::Arc::new(|_| Ok(())),
                std::sync::Arc::new(|| false),
            )
            .unwrap();
        let presentation = MenuPresentation::from_registry(
            11,
            &registry,
            [(ActionId::from_static("theme-ocean"), MenuItemMarker::Radio)],
        );
        assert_eq!(presentation.revision(), 11);
        assert_eq!(
            presentation.item("theme-ocean"),
            MenuItemPresentation {
                enabled: false,
                marker: MenuItemMarker::Radio,
            }
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn icon_feature_preserves_literals_and_const_constructor() {
        const CONST_ITEM: MenuItemDef = MenuItemDef::new("song-save", "Save");
        let literal_item = MenuItemDef {
            id: "song-open",
            label: "Open".into(),
        };
        let item = MenuItemDef::new("song-open", "Open").with_icon(Icon::FolderOpen);
        assert_eq!(CONST_ITEM.label.as_str(), "Save");
        assert_eq!(literal_item.label.icon(), None);
        assert_eq!(item.label.icon(), Some(Icon::FolderOpen));
    }

    #[cfg(feature = "icons")]
    #[test]
    fn plain_spawn_omits_icon_slot_even_when_icons_feature_is_enabled() {
        use bevy::ecs::hierarchy::Children;
        use bevy::ecs::system::RunSystemOnce;

        fn spawn(mut commands: Commands) {
            spawn_menu_bar(
                &mut commands,
                &[MenuDef {
                    label: "File".into(),
                    items: vec![MenuItemDef::new("open", "Open").with_icon(Icon::FolderOpen)],
                }],
            );
        }

        let mut world = bevy::ecs::world::World::new();
        world.run_system_once(spawn).unwrap();
        world.flush();
        let mut entries = world.query_filtered::<&Children, With<MenuEntry>>();
        let children = entries.single(&world).unwrap();
        let expected = if cfg!(feature = "actions") { 4 } else { 1 };
        assert_eq!(children.len(), expected);
    }

    #[cfg(feature = "icons")]
    #[test]
    fn presentation_sync_retints_icon_when_theme_changes() {
        let mut app = App::new();
        app.init_resource::<MenuPresentation>()
            .add_systems(Update, sync_menu_presentation);
        let mut theme = UiTheme::default();
        let mut theme_state = crate::theme::ThemeState::default();
        crate::theme::apply_theme(
            &mut theme,
            &mut theme_state,
            &crate::theme::ThemeSpec::builtin(),
        );
        app.insert_resource(theme);
        let label = app.world_mut().spawn(Text::new("Open")).id();
        #[cfg(feature = "actions")]
        let marker = app.world_mut().spawn(Text::new("")).id();
        #[cfg(feature = "actions")]
        let accelerator = app.world_mut().spawn(Text::new("")).id();
        let icon = app
            .world_mut()
            .spawn((
                SvgColor(Color::NONE),
                ThemeSvgColor(crate::theme::tokens::TEXT),
            ))
            .id();
        app.world_mut().spawn(MenuEntry {
            id: "open",
            bar: Entity::PLACEHOLDER,
            label,
            #[cfg(feature = "actions")]
            marker,
            icon: Some(icon),
            #[cfg(feature = "actions")]
            accelerator,
            enabled: true,
            presentation_revision: Some(0),
        });

        app.update();
        let changed_colour = Color::srgb(0.12, 0.34, 0.56);
        app.world_mut()
            .resource_mut::<UiTheme>()
            .set_color("ctk.text", changed_colour);
        app.update();

        assert_eq!(
            app.world().entity(icon).get::<SvgColor>().unwrap().0,
            changed_colour
        );
    }

    #[test]
    fn menu_activation_carries_focus_from_before_title_press() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<SeenActivations>()
            .add_plugins(MenuBarPlugin)
            .add_observer(record_activation);

        let board_control = app.world_mut().spawn_empty().id();
        let bar = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default()))
            .id();
        let dropdown = app.world_mut().spawn((Node::default(), MenuDropdown)).id();
        let title = app.world_mut().spawn(MenuTitle { dropdown, bar }).id();
        let entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "settings",
                bar,
                label: Entity::PLACEHOLDER,
                #[cfg(feature = "actions")]
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                #[cfg(feature = "actions")]
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: None,
            })
            .id();

        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(board_control, FocusCause::Navigated);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            pointer_location(title),
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                count: 1,
            },
            title,
        ));
        // This is what Bevy's non-focusable menu chrome does after Press.
        app.world_mut().resource_mut::<InputFocus>().clear();
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            pointer_location(entry),
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            entry,
        ));
        app.world_mut().flush();

        assert_eq!(
            app.world().resource::<SeenActivations>().0,
            [MenuActivated {
                bar,
                id: "settings",
                invocation_focus: Some(board_control),
                #[cfg(feature = "actions")]
                origin: MenuActivationOrigin::Pointer,
            }]
        );
    }

    #[test]
    fn current_presentation_disablement_blocks_click_before_style_sync() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<SeenActivations>()
            .add_plugins(MenuBarPlugin)
            .add_observer(record_activation);
        app.world_mut().resource_mut::<MenuPresentation>().replace(
            1,
            [(
                "disabled",
                MenuItemPresentation {
                    enabled: false,
                    marker: MenuItemMarker::None,
                },
            )],
        );
        let bar = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default()))
            .id();
        let entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "disabled",
                bar,
                label: Entity::PLACEHOLDER,
                #[cfg(feature = "actions")]
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                #[cfg(feature = "actions")]
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: Some(0),
            })
            .id();

        click(&mut app, entry);
        app.world_mut().flush();
        assert!(app.world().resource::<SeenActivations>().0.is_empty());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn click_rechecks_registry_instead_of_stale_enabled_presentation() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<SeenActivations>()
            .add_plugins(MenuBarPlugin)
            .add_observer(record_activation)
            .insert_resource(test_registry("disabled", false));
        let bar = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default(), ActionBridgeBar))
            .id();
        let entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "disabled",
                bar,
                label: Entity::PLACEHOLDER,
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: Some(1),
            })
            .id();

        click(&mut app, entry);
        app.world_mut().flush();
        assert!(app.world().resource::<SeenActivations>().0.is_empty());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn bridged_click_does_not_override_disabled_presentation() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<SeenActivations>()
            .add_plugins(MenuBarPlugin)
            .add_observer(record_activation)
            .insert_resource(test_registry("settings", true));
        app.world_mut().resource_mut::<MenuPresentation>().replace(
            1,
            [(
                "settings",
                MenuItemPresentation {
                    enabled: false,
                    marker: MenuItemMarker::None,
                },
            )],
        );
        let bar = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default(), ActionBridgeBar))
            .id();
        let entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "settings",
                bar,
                label: Entity::PLACEHOLDER,
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: Some(0),
            })
            .id();

        click(&mut app, entry);
        app.world_mut().flush();
        assert!(app.world().resource::<SeenActivations>().0.is_empty());
    }

    #[cfg(feature = "actions")]
    #[test]
    fn registry_authority_is_scoped_to_bridged_bars() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<SeenActivations>()
            .add_plugins(MenuBarPlugin)
            .add_observer(record_activation)
            .insert_resource(test_registry("registered", true));
        let legacy_bar = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default()))
            .id();
        let bridged_bar = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default(), ActionBridgeBar))
            .id();
        let legacy_entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "missing",
                bar: legacy_bar,
                label: Entity::PLACEHOLDER,
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: Some(1),
            })
            .id();
        let bridged_entry = app
            .world_mut()
            .spawn(MenuEntry {
                id: "missing",
                bar: bridged_bar,
                label: Entity::PLACEHOLDER,
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: Some(1),
            })
            .id();

        click(&mut app, legacy_entry);
        click(&mut app, bridged_entry);
        app.world_mut().flush();
        assert_eq!(
            app.world().resource::<SeenActivations>().0,
            [MenuActivated {
                bar: legacy_bar,
                id: "missing",
                invocation_focus: None,
                origin: MenuActivationOrigin::Pointer,
            }]
        );
    }

    #[cfg(not(any(feature = "actions", feature = "icons")))]
    #[test]
    fn featureless_entry_keeps_original_single_label_layout() {
        use bevy::ecs::hierarchy::Children;
        use bevy::ecs::system::RunSystemOnce;

        fn spawn(mut commands: Commands) {
            spawn_menu_bar(
                &mut commands,
                &[MenuDef {
                    label: "File".into(),
                    items: vec![MenuItemDef::new("open", "Open")],
                }],
            );
        }

        let mut world = bevy::ecs::world::World::new();
        world.run_system_once(spawn).unwrap();
        world.flush();
        let mut entries = world.query_filtered::<(&Children, &Node), With<MenuEntry>>();
        let (children, node) = entries.single(&world).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(node.column_gap, px(0));
        assert_eq!(node.align_items, default());
    }

    #[test]
    fn opening_and_activating_second_bar_resets_first_bars_invocation() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<SeenActivations>()
            .add_plugins(MenuBarPlugin)
            .add_observer(record_activation);

        let focus_a = app.world_mut().spawn_empty().id();
        let focus_b = app.world_mut().spawn_empty().id();
        let focus_a_reopened = app.world_mut().spawn_empty().id();
        let bar_a = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default()))
            .id();
        let bar_b = app
            .world_mut()
            .spawn((MenuBar, MenuInvocationFocus::default()))
            .id();
        let dropdown_a = app
            .world_mut()
            .spawn((
                Node {
                    display: Display::None,
                    ..default()
                },
                MenuDropdown,
            ))
            .id();
        let dropdown_b = app
            .world_mut()
            .spawn((
                Node {
                    display: Display::None,
                    ..default()
                },
                MenuDropdown,
            ))
            .id();
        let title_a = app
            .world_mut()
            .spawn(MenuTitle {
                dropdown: dropdown_a,
                bar: bar_a,
            })
            .id();
        let title_b = app
            .world_mut()
            .spawn(MenuTitle {
                dropdown: dropdown_b,
                bar: bar_b,
            })
            .id();
        let entry_a = app
            .world_mut()
            .spawn(MenuEntry {
                id: "settings-a",
                bar: bar_a,
                label: Entity::PLACEHOLDER,
                #[cfg(feature = "actions")]
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                #[cfg(feature = "actions")]
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: None,
            })
            .id();
        let entry_b = app
            .world_mut()
            .spawn(MenuEntry {
                id: "settings-b",
                bar: bar_b,
                label: Entity::PLACEHOLDER,
                #[cfg(feature = "actions")]
                marker: Entity::PLACEHOLDER,
                #[cfg(feature = "icons")]
                icon: None,
                #[cfg(feature = "actions")]
                accelerator: Entity::PLACEHOLDER,
                enabled: true,
                presentation_revision: None,
            })
            .id();

        press_title(&mut app, title_a, focus_a);
        click(&mut app, title_a);
        press_title(&mut app, title_b, focus_b);
        click(&mut app, title_b);

        let invocation_a = app
            .world()
            .entity(bar_a)
            .get::<MenuInvocationFocus>()
            .unwrap();
        assert!(!invocation_a.active);
        assert_eq!(invocation_a.focus, None);

        click(&mut app, entry_b);
        for bar in [bar_a, bar_b] {
            let invocation = app
                .world()
                .entity(bar)
                .get::<MenuInvocationFocus>()
                .unwrap();
            assert!(!invocation.active);
            assert_eq!(invocation.focus, None);
        }

        press_title(&mut app, title_a, focus_a_reopened);
        click(&mut app, title_a);
        click(&mut app, entry_a);
        app.world_mut().flush();

        assert_eq!(
            app.world().resource::<SeenActivations>().0,
            [
                MenuActivated {
                    bar: bar_b,
                    id: "settings-b",
                    invocation_focus: Some(focus_b),
                    #[cfg(feature = "actions")]
                    origin: MenuActivationOrigin::Pointer,
                },
                MenuActivated {
                    bar: bar_a,
                    id: "settings-a",
                    invocation_focus: Some(focus_a_reopened),
                    #[cfg(feature = "actions")]
                    origin: MenuActivationOrigin::Pointer,
                },
            ]
        );
    }

    fn press_title(app: &mut App, title: Entity, focus: Entity) {
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(focus, FocusCause::Navigated);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            pointer_location(title),
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                count: 1,
            },
            title,
        ));
        app.world_mut().resource_mut::<InputFocus>().clear();
    }

    fn click(app: &mut App, target: Entity) {
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            pointer_location(target),
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            target,
        ));
    }

    #[cfg(feature = "actions")]
    fn spawn_test_context(
        app: &mut App,
        items: &[MenuItemDef],
        invocation_focus: Option<Entity>,
    ) -> Entity {
        let mut queue = bevy::ecs::world::CommandQueue::default();
        let menu = {
            let world = app.world_mut();
            let mut commands = Commands::new(&mut queue, world);
            spawn_context_menu(
                &mut commands,
                items,
                Vec2::new(20.0, 30.0),
                invocation_focus,
            )
        };
        queue.apply(app.world_mut());
        menu
    }

    #[cfg(feature = "actions")]
    #[test]
    fn context_menu_arrow_navigation_enter_dispatch_and_single_open() {
        use bevy::input::keyboard::{Key as LogicalKey, NativeKey};
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::window::PrimaryWindow;

        let mut app = App::new();
        app.add_plugins(bevy::input::InputPlugin)
            .add_plugins((InputFocusPlugin, InputDispatchPlugin))
            .init_resource::<UiTheme>()
            .init_resource::<SeenActivations>()
            .add_plugins(MenuBarPlugin)
            .add_observer(record_activation)
            .insert_resource(test_registry("open", true));
        let window = app
            .world_mut()
            .spawn((bevy::window::Window::default(), PrimaryWindow))
            .id();
        let first = spawn_test_context(
            &mut app,
            &[
                MenuItemDef::new("open", "Open"),
                MenuItemDef::new("open", "Open Again"),
            ],
            None,
        );
        app.update();
        let first_entries = app
            .world()
            .entity(first)
            .get::<ContextMenu>()
            .unwrap()
            .entries
            .clone();
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(first_entries[0])
        );
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::ArrowDown,
            logical_key: LogicalKey::Unidentified(NativeKey::Unidentified),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(first_entries[1])
        );

        let second = spawn_test_context(&mut app, &[MenuItemDef::new("open", "Replacement")], None);
        app.update();
        assert!(app.world().get_entity(first).is_err());
        let second_entry = app
            .world()
            .entity(second)
            .get::<ContextMenu>()
            .unwrap()
            .entries[0];
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(second_entry)
        );

        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Enter,
            logical_key: LogicalKey::Unidentified(NativeKey::Unidentified),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();
        app.world_mut().flush();

        assert!(app.world().get_entity(second).is_err());
        assert_eq!(
            app.world().resource::<SeenActivations>().0,
            [MenuActivated {
                bar: second,
                id: "open",
                invocation_focus: None,
                origin: MenuActivationOrigin::Programmatic(Source::Key),
            }]
        );
    }

    #[cfg(feature = "actions")]
    #[test]
    fn context_menu_outside_click_and_escape_dismiss() {
        use bevy::input::keyboard::{Key as LogicalKey, NativeKey};
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::window::PrimaryWindow;

        let mut app = App::new();
        app.add_plugins(bevy::input::InputPlugin)
            .add_plugins((InputFocusPlugin, InputDispatchPlugin))
            .init_resource::<UiTheme>()
            .add_plugins(MenuBarPlugin)
            .insert_resource(test_registry("open", true));
        let window = app
            .world_mut()
            .spawn((bevy::window::Window::default(), PrimaryWindow))
            .id();
        let previous = app.world_mut().spawn_empty().id();
        let first = spawn_test_context(
            &mut app,
            &[MenuItemDef::new("open", "Open")],
            Some(previous),
        );
        app.update();
        let outside = app.world_mut().spawn_empty().id();
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(window).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            },
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            outside,
        ));
        app.world_mut().flush();
        assert!(app.world().get_entity(first).is_err());

        let second = spawn_test_context(
            &mut app,
            &[MenuItemDef::new("open", "Open")],
            Some(previous),
        );
        app.update();
        // Escape owns the open context menu even if another system has moved
        // focus away from its rows (or every row is disabled).
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(previous, FocusCause::Navigated);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Escape,
            logical_key: LogicalKey::Unidentified(NativeKey::Unidentified),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();
        app.world_mut().flush();
        assert!(app.world().get_entity(second).is_err());
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(previous));
    }
}
