//! Canonical CTK button: one variant/size surface resolved from `CtkDesign`.
//!
//! Apps which use icon labels must install [`crate::icons::IconSet`]. If it is
//! missing, CTK warns once and keeps the label pending so a legitimately late
//! resource can still materialise it. Phase 1 deliberately has no text
//! fallback: installing the icon set is part of the app-initialisation
//! contract for icon buttons.

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::ecs::change_detection::DetectChangesMut;
use bevy::ecs::hierarchy::Children;
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::observer::On;
#[cfg(feature = "icons")]
use bevy::ecs::query::Without;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{InputFocus, InputFocusVisible};
use bevy::log::warn;
use bevy::picking::hover::Hovered;
use bevy::prelude::{
    default, BackgroundColor, BorderColor, Color, Component, DetectChanges, Entity, Node, Ref,
    Resource, Text, TextColor, TextFont, UiRect,
};
use bevy::text::{FontSize, FontSource};
use bevy::ui::{px, AlignItems, BorderRadius, InteractionDisabled, JustifyContent, Pressed};
use bevy::ui_widgets::ActivateOnPress;

use cosmix_design::{ButtonCellKey, InteractionState, ResolvedButtonCell};

use crate::design::{bevy_color, CtkDesign};
use crate::style::InteractionVisualState;
use crate::theme::{CtkTypography, CtkTypographyOptOut};
use crate::widgets::{ActionButton, BusWidget};

pub use cosmix_design::{ButtonSize, ButtonVariant};

/// Content rendered inside a button.
#[derive(Clone, Debug, PartialEq)]
pub enum ButtonLabel {
    Text(String),
    #[cfg(feature = "icons")]
    Icon(crate::icons::Icon, String),
    #[cfg(feature = "icons")]
    IconText(crate::icons::Icon, String),
}

impl ButtonLabel {
    fn accessibility_label(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            #[cfg(feature = "icons")]
            Self::Icon(_, accessible_label) => accessible_label.clone(),
            #[cfg(feature = "icons")]
            Self::IconText(_, text) => text.clone(),
        }
    }
}

/// Declarative button definition, following shadcn's variant/size model.
#[derive(Clone, Debug, PartialEq)]
pub struct ButtonDef {
    pub label: ButtonLabel,
    pub variant: ButtonVariant,
    pub size: ButtonSize,
    pub bus_id: Option<String>,
    pub disabled: bool,
}

impl ButtonDef {
    pub fn text(label: impl Into<String>) -> Self {
        Self {
            label: ButtonLabel::Text(label.into()),
            variant: ButtonVariant::default(),
            size: ButtonSize::default(),
            bus_id: None,
            disabled: false,
        }
    }

    #[cfg(feature = "icons")]
    pub fn icon(icon: crate::icons::Icon, accessible_label: impl Into<String>) -> Self {
        Self {
            label: ButtonLabel::Icon(icon, accessible_label.into()),
            ..Self::text("")
        }
    }

    #[cfg(feature = "icons")]
    pub fn icon_text(icon: crate::icons::Icon, label: impl Into<String>) -> Self {
        Self {
            label: ButtonLabel::IconText(icon, label.into()),
            ..Self::text("")
        }
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    pub fn bus(mut self, id: impl Into<String>) -> Self {
        self.bus_id = Some(id.into());
        self
    }

    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }
}

/// Canonical button presentation state. Mutating either field restyles live.
///
/// This marker is exclusive with Bevy's [`bevy::ui_widgets::Button`] and CTK's
/// [`crate::widgets::ToggleButton`]. Combining either marker with `CtkButton`
/// creates competing input contracts and is diagnosed once per entity.
/// [`ActionButton`] is the sanctioned exception used by Bus-bound buttons.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct CtkButton {
    pub variant: ButtonVariant,
    pub size: ButtonSize,
}

/// Button-owned label content. The button reconciles a text label's
/// [`TextFont`] every frame; callers must not style that component directly.
#[derive(Component)]
pub(crate) struct CtkButtonLabel(Entity);

#[cfg(feature = "icons")]
#[derive(Component)]
pub(crate) struct PendingButtonLabel(ButtonLabel);

#[derive(Resource, Default)]
pub(crate) struct ButtonDiagnostics {
    #[cfg(feature = "icons")]
    missing_icon_set_warned: bool,
    marker_collisions_warned: std::collections::HashSet<Entity>,
}

/// Spawn one canonical CTK button and its label child.
///
/// Requires both `CtkWidgetsPlugin` (input + repaint systems) and
/// [`crate::theme::CtkThemePlugin`] (the real palette, and live label-font
/// reconciliation scheduled against its typography pass).
///
/// Do not add Bevy's [`bevy::ui_widgets::Button`] or CTK's
/// [`crate::widgets::ToggleButton`]: `CtkWidgetsPlugin` owns this entity's one
/// pointer/keyboard activation path and warns once if either marker collides.
pub fn spawn_button(commands: &mut Commands, def: ButtonDef) -> Entity {
    let mut accessible = accesskit::Node::new(Role::Button);
    accessible.set_label(def.label.accessibility_label());

    let entity = commands
        .spawn((
            CtkButton {
                variant: def.variant,
                size: def.size,
            },
            ActivateOnPress,
            TabIndex(0),
            Hovered::default(),
            AccessibilityNode::from(accessible),
            Node {
                column_gap: px(6),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor::DEFAULT,
            BorderColor::DEFAULT,
        ))
        .id();

    if let Some(id) = def.bus_id {
        commands
            .entity(entity)
            .insert((BusWidget::writable(id), ActionButton));
    }
    if def.disabled {
        commands.entity(entity).insert(InteractionDisabled);
    }

    match def.label {
        ButtonLabel::Text(text) => {
            let label = spawn_text_label(commands, entity, text, TextFont::default(), Color::NONE);
            commands.entity(entity).add_child(label);
        }
        #[cfg(feature = "icons")]
        label @ (ButtonLabel::Icon(_, _) | ButtonLabel::IconText(_, _)) => {
            commands.entity(entity).insert(PendingButtonLabel(label));
        }
    }

    entity
}

fn spawn_text_label(
    commands: &mut Commands,
    button: Entity,
    text: String,
    font: TextFont,
    colour: Color,
) -> Entity {
    commands
        .spawn((
            Text::new(text),
            font,
            TextColor(colour),
            CtkTypographyOptOut,
            CtkButtonLabel(button),
        ))
        .id()
}

fn visual_state(hovered: &Hovered, pressed: bool, disabled: bool) -> InteractionVisualState {
    if disabled {
        InteractionVisualState::Disabled
    } else if pressed {
        InteractionVisualState::Pressed
    } else if hovered.get() {
        InteractionVisualState::Hovered
    } else {
        InteractionVisualState::Resting
    }
}

fn button_cell_key(
    button: &CtkButton,
    state: InteractionVisualState,
    focus_visible: bool,
) -> ButtonCellKey {
    let interaction = match state {
        InteractionVisualState::Resting => InteractionState::Resting,
        InteractionVisualState::Hovered => InteractionState::Hovered,
        InteractionVisualState::Pressed => InteractionState::Pressed,
        InteractionVisualState::Disabled => InteractionState::Disabled,
    };
    ButtonCellKey {
        variant: button.variant,
        size: button.size,
        interaction,
        focus_visible,
    }
}

fn has_visible_focus(
    entity: Entity,
    focus: Option<&InputFocus>,
    focus_visible: Option<&InputFocusVisible>,
) -> bool {
    focus_visible.is_some_and(|visible| visible.0)
        && focus.and_then(InputFocus::get) == Some(entity)
}

#[allow(clippy::type_complexity)]
pub(crate) fn update_button_style(
    design: Res<CtkDesign>,
    focus: Option<Res<InputFocus>>,
    focus_visible: Option<Res<InputFocusVisible>>,
    mut buttons: Query<(
        Entity,
        &Hovered,
        Has<Pressed>,
        Has<InteractionDisabled>,
        &CtkButton,
        &mut Node,
        &mut BackgroundColor,
        &mut BorderColor,
        Option<&Children>,
    )>,
    mut labels: Query<&mut TextColor, With<CtkButtonLabel>>,
) {
    for (
        entity,
        hovered,
        pressed,
        disabled,
        button,
        mut node,
        mut background,
        mut border,
        children,
    ) in &mut buttons
    {
        let state = visual_state(hovered, pressed, disabled);
        let focused =
            !disabled && has_visible_focus(entity, focus.as_deref(), focus_visible.as_deref());
        let Some(cell) = design.button_cell(button_cell_key(button, state, focused)) else {
            continue;
        };
        let want_background = bevy_color(cell.pair.surface);
        let want_border = cell.ring.or(cell.border).map_or(Color::NONE, bevy_color);
        let want_text = bevy_color(cell.pair.foreground);
        sync_node_to_cell(cell, &mut node);

        // Every write is guarded: unconditional writes would mark every button
        // changed on every frame and repaint the whole button population.
        if background.0 != want_background {
            background.0 = want_background;
        }
        let want_border = BorderColor::all(want_border);
        if *border != want_border {
            *border = want_border;
        }
        let Some(children) = children else {
            continue;
        };
        for child in children.iter() {
            let Ok(mut colour) = labels.get_mut(*child) else {
                continue;
            };
            if colour.0 != want_text {
                colour.0 = want_text;
            }
        }
    }
}

fn label_font_size(typography: &CtkTypography, size: ButtonSize) -> f32 {
    if size == ButtonSize::Sm {
        typography.body_px - 2.0
    } else {
        typography.body_px
    }
}

fn sync_node_to_cell(cell: &ResolvedButtonCell, node: &mut Node) {
    let height = px(cell.height as f32);
    let min_width = px(cell.min_width as f32);
    let padding = UiRect::horizontal(px(cell.padding_x as f32));
    let border = UiRect::all(px(cell.border_width as f32));
    let radius = BorderRadius::all(px(cell.radius as f32));
    if node.height != height
        || node.min_width != min_width
        || node.padding != padding
        || node.border != border
        || node.border_radius != radius
    {
        node.height = height;
        node.min_width = min_width;
        node.padding = padding;
        node.border = border;
        node.border_radius = radius;
    }
}

/// Returns true only when the font was actually rewritten, so callers holding
/// a `Mut<TextFont>` can keep Bevy's change tick honest.
fn apply_label_font(typography: &CtkTypography, size: ButtonSize, font: &mut TextFont) -> bool {
    let want_size = FontSize::Px(label_font_size(typography, size));
    let source_drift =
        typography.effective_family.is_some() && !matches!(font.font, FontSource::SansSerif);
    if font.font_size != want_size || source_drift {
        if typography.effective_family.is_some() {
            // Match apply_ctk_typography's source ownership: once CTK has a
            // resolved generic mapping, stamp SansSerif. Never revert it when
            // that mapping is later lost; doing so can turn a working face
            // into Bevy's ASCII-only embedded fallback.
            font.font = FontSource::SansSerif;
        }
        font.font_size = want_size;
        return true;
    }
    false
}

struct ButtonPaintContext<'a> {
    entity: Entity,
    design: &'a CtkDesign,
    focus: Option<&'a InputFocus>,
    focus_visible: Option<&'a InputFocusVisible>,
    button: &'a CtkButton,
    hovered: &'a Hovered,
    pressed: bool,
    disabled: bool,
}

fn paint_button_root(
    context: ButtonPaintContext<'_>,
    background: &mut BackgroundColor,
    border: &mut BorderColor,
) {
    let state = visual_state(context.hovered, context.pressed, context.disabled);
    let focused = !context.disabled
        && has_visible_focus(context.entity, context.focus, context.focus_visible);
    let Some(cell) = context
        .design
        .button_cell(button_cell_key(context.button, state, focused))
    else {
        return;
    };
    let want_background = bevy_color(cell.pair.surface);
    let want_border = cell.ring.or(cell.border).map_or(Color::NONE, bevy_color);
    if background.0 != want_background {
        background.0 = want_background;
    }
    let want_border = BorderColor::all(want_border);
    if *border != want_border {
        *border = want_border;
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn paint_added_button(
    add: On<Add, CtkButton>,
    design: Res<CtkDesign>,
    focus: Option<Res<InputFocus>>,
    focus_visible: Option<Res<InputFocusVisible>>,
    mut buttons: Query<(
        &CtkButton,
        &Hovered,
        Has<Pressed>,
        Has<InteractionDisabled>,
        &mut Node,
        &mut BackgroundColor,
        &mut BorderColor,
    )>,
) {
    let Ok((button, hovered, pressed, disabled, mut node, mut background, mut border)) =
        buttons.get_mut(add.entity)
    else {
        return;
    };
    let state = visual_state(hovered, pressed, disabled);
    let focused =
        !disabled && has_visible_focus(add.entity, focus.as_deref(), focus_visible.as_deref());
    if let Some(cell) = design.button_cell(button_cell_key(button, state, focused)) {
        sync_node_to_cell(cell, &mut node);
    }
    paint_button_root(
        ButtonPaintContext {
            entity: add.entity,
            design: &design,
            focus: focus.as_deref(),
            focus_visible: focus_visible.as_deref(),
            button,
            hovered,
            pressed,
            disabled,
        },
        &mut background,
        &mut border,
    );
}

pub(crate) fn paint_added_button_label(
    add: On<Add, CtkButtonLabel>,
    design: Res<CtkDesign>,
    typography: Res<CtkTypography>,
    buttons: Query<(&CtkButton, Has<InteractionDisabled>)>,
    mut labels: Query<(&CtkButtonLabel, &mut TextFont, &mut TextColor)>,
) {
    let Ok((managed, mut font, mut colour)) = labels.get_mut(add.entity) else {
        return;
    };
    let Ok((button, disabled)) = buttons.get(managed.0) else {
        return;
    };
    apply_label_font(&typography, button.size, &mut font);
    let state = if disabled {
        InteractionVisualState::Disabled
    } else {
        InteractionVisualState::Resting
    };
    let Some(cell) = design.button_cell(button_cell_key(button, state, false)) else {
        return;
    };
    let want_colour = bevy_color(cell.pair.foreground);
    if colour.0 != want_colour {
        colour.0 = want_colour;
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn paint_disabled_button(
    add: On<Add, InteractionDisabled>,
    design: Res<CtkDesign>,
    mut buttons: Query<(
        &CtkButton,
        &Hovered,
        Has<Pressed>,
        &mut BackgroundColor,
        &mut BorderColor,
        Option<&Children>,
    )>,
    mut labels: Query<&mut TextColor, With<CtkButtonLabel>>,
    #[cfg(feature = "icons")] mut icons: Query<&mut crate::icons::SvgColor, With<CtkButtonLabel>>,
) {
    let Ok((button, hovered, pressed, mut background, mut border, children)) =
        buttons.get_mut(add.entity)
    else {
        return;
    };
    paint_button_root(
        ButtonPaintContext {
            entity: add.entity,
            design: &design,
            focus: None,
            focus_visible: None,
            button,
            hovered,
            pressed,
            disabled: true,
        },
        &mut background,
        &mut border,
    );
    let Some(cell) = design.button_cell(button_cell_key(
        button,
        InteractionVisualState::Disabled,
        false,
    )) else {
        return;
    };
    let want_text = bevy_color(cell.pair.foreground);
    let Some(children) = children else {
        return;
    };
    for child in children.iter() {
        if let Ok(mut colour) = labels.get_mut(*child) {
            if colour.0 != want_text {
                colour.0 = want_text;
            }
        }
        #[cfg(feature = "icons")]
        if let Ok(mut svg) = icons.get_mut(*child) {
            if svg.0 != want_text {
                svg.0 = want_text;
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn paint_enabled_button(
    remove: On<Remove, InteractionDisabled>,
    design: Res<CtkDesign>,
    focus: Option<Res<InputFocus>>,
    focus_visible: Option<Res<InputFocusVisible>>,
    mut buttons: Query<(
        &CtkButton,
        &Hovered,
        Has<Pressed>,
        &mut BackgroundColor,
        &mut BorderColor,
        Option<&Children>,
    )>,
    mut labels: Query<&mut TextColor, With<CtkButtonLabel>>,
    #[cfg(feature = "icons")] mut icons: Query<&mut crate::icons::SvgColor, With<CtkButtonLabel>>,
) {
    let Ok((button, hovered, pressed, mut background, mut border, children)) =
        buttons.get_mut(remove.entity)
    else {
        return;
    };
    paint_button_root(
        ButtonPaintContext {
            entity: remove.entity,
            design: &design,
            focus: focus.as_deref(),
            focus_visible: focus_visible.as_deref(),
            button,
            hovered,
            pressed,
            disabled: false,
        },
        &mut background,
        &mut border,
    );
    let state = visual_state(hovered, pressed, false);
    let focused = has_visible_focus(remove.entity, focus.as_deref(), focus_visible.as_deref());
    let Some(cell) = design.button_cell(button_cell_key(button, state, focused)) else {
        return;
    };
    let want_text = bevy_color(cell.pair.foreground);
    let Some(children) = children else {
        return;
    };
    for child in children.iter() {
        if let Ok(mut colour) = labels.get_mut(*child) {
            if colour.0 != want_text {
                colour.0 = want_text;
            }
        }
        #[cfg(feature = "icons")]
        if let Ok(mut svg) = icons.get_mut(*child) {
            if svg.0 != want_text {
                svg.0 = want_text;
            }
        }
    }
}

#[cfg(feature = "icons")]
fn apply_icon_metrics(size: f32, node: &mut Node) {
    let size = px(size);
    if node.width != size || node.min_width != size || node.height != size {
        node.width = size;
        node.min_width = size;
        node.height = size;
    }
}

#[cfg(feature = "icons")]
type ButtonIconNodes<'w, 's> = Query<
    'w,
    's,
    &'static mut Node,
    (
        With<CtkButtonLabel>,
        With<crate::icons::UiSvg>,
        Without<CtkButton>,
    ),
>;

pub(crate) fn update_button_icon_metrics(
    typography: Res<CtkTypography>,
    buttons: Query<(Ref<CtkButton>, Option<&Children>)>,
    #[cfg(feature = "icons")] mut icons: ButtonIconNodes,
) {
    let resources_changed = typography.is_changed();
    for (button, children) in &buttons {
        let geometry_changed = resources_changed || button.is_changed();
        #[cfg(not(feature = "icons"))]
        let _ = geometry_changed;
        let Some(children) = children else {
            continue;
        };
        #[cfg(not(feature = "icons"))]
        let _ = children;
        #[cfg(feature = "icons")]
        for child in children.iter() {
            if geometry_changed {
                if let Ok(mut node) = icons.get_mut(*child) {
                    apply_icon_metrics(label_font_size(&typography, button.size), &mut node);
                }
            }
        }
    }
}

/// Runs in `PostUpdate` after `apply_ctk_typography` has resolved the font
/// family, so a `Startup`-spawned label never renders a frame with the
/// fallback source. Deliberately unconditional but guarded: the button owns
/// its label font and self-heals outside writes. `CtkThemePlugin` schedules
/// this relative to its typography pass — like the real palette, live font
/// reconciliation is part of that plugin's contract.
pub(crate) fn reconcile_button_label_fonts(
    typography: Res<CtkTypography>,
    buttons: Query<&CtkButton>,
    mut labels: Query<(&CtkButtonLabel, &mut TextFont)>,
) {
    for (label, mut font) in &mut labels {
        let Ok(button) = buttons.get(label.0) else {
            continue;
        };
        // Bypass first: a bare `DerefMut` would mark every settled label
        // changed every frame and feed rerender detection needless work.
        if apply_label_font(&typography, button.size, font.bypass_change_detection()) {
            font.set_changed();
        }
    }
}

#[cfg(feature = "icons")]
struct ButtonLabelMaterialiseContext<'a> {
    icons: &'a crate::icons::IconSet,
    design: &'a CtkDesign,
    typography: &'a CtkTypography,
    entity: Entity,
    button: &'a CtkButton,
    disabled: bool,
}

#[cfg(feature = "icons")]
fn materialise_button_label(
    commands: &mut Commands,
    pending: &PendingButtonLabel,
    context: ButtonLabelMaterialiseContext<'_>,
) {
    let state = if context.disabled {
        InteractionVisualState::Disabled
    } else {
        InteractionVisualState::Resting
    };
    let Some(cell) = context
        .design
        .button_cell(button_cell_key(context.button, state, false))
    else {
        return;
    };
    let colour = bevy_color(cell.pair.foreground);
    let font_size = label_font_size(context.typography, context.button.size);
    match &pending.0 {
        ButtonLabel::Text(_) => unreachable!("text labels spawn immediately"),
        ButtonLabel::Icon(icon, _) => {
            let icon = crate::icons::spawn_icon_coloured(
                commands,
                context.icons,
                *icon,
                font_size,
                colour,
            );
            commands.entity(icon).insert(CtkButtonLabel(context.entity));
            commands.entity(context.entity).add_child(icon);
        }
        ButtonLabel::IconText(icon, text) => {
            let icon = crate::icons::spawn_icon_coloured(
                commands,
                context.icons,
                *icon,
                font_size,
                colour,
            );
            commands.entity(icon).insert(CtkButtonLabel(context.entity));
            commands.entity(context.entity).add_child(icon);
            let text = spawn_text_label(
                commands,
                context.entity,
                text.clone(),
                TextFont::from_font_size(font_size),
                colour,
            );
            commands.entity(context.entity).add_child(text);
        }
    }
}

#[cfg(feature = "icons")]
pub(crate) fn materialise_added_button_label(
    add: On<Add, PendingButtonLabel>,
    mut commands: Commands,
    icons: Option<Res<crate::icons::IconSet>>,
    design: Res<CtkDesign>,
    typography: Res<CtkTypography>,
    pending: Query<(&PendingButtonLabel, &CtkButton, Has<InteractionDisabled>)>,
) {
    let Some(icons) = icons else {
        return;
    };
    let Ok((pending, button, disabled)) = pending.get(add.entity) else {
        return;
    };
    materialise_button_label(
        &mut commands,
        pending,
        ButtonLabelMaterialiseContext {
            icons: &icons,
            design: &design,
            typography: &typography,
            entity: add.entity,
            button,
            disabled,
        },
    );
    commands.entity(add.entity).remove::<PendingButtonLabel>();
}

#[cfg(feature = "icons")]
pub(crate) fn spawn_pending_button_labels(
    mut commands: Commands,
    icons: Option<Res<crate::icons::IconSet>>,
    design: Res<CtkDesign>,
    typography: Res<CtkTypography>,
    mut diagnostics: ResMut<ButtonDiagnostics>,
    pending: Query<(
        Entity,
        &PendingButtonLabel,
        &CtkButton,
        Has<InteractionDisabled>,
    )>,
) {
    if pending.is_empty() {
        return;
    }
    let Some(icons) = icons else {
        if !diagnostics.missing_icon_set_warned {
            warn!("icon ButtonDef spawned but IconSet resource not installed");
            diagnostics.missing_icon_set_warned = true;
        }
        return;
    };
    for (entity, pending, button, disabled) in &pending {
        materialise_button_label(
            &mut commands,
            pending,
            ButtonLabelMaterialiseContext {
                icons: &icons,
                design: &design,
                typography: &typography,
                entity,
                button,
                disabled,
            },
        );
        commands.entity(entity).remove::<PendingButtonLabel>();
    }
}

#[cfg(feature = "icons")]
#[allow(clippy::type_complexity)]
pub(crate) fn update_button_icon_style(
    design: Res<CtkDesign>,
    focus: Option<Res<InputFocus>>,
    focus_visible: Option<Res<InputFocusVisible>>,
    buttons: Query<(
        Entity,
        &CtkButton,
        &Hovered,
        Has<Pressed>,
        Has<InteractionDisabled>,
        Option<&Children>,
    )>,
    mut icons: Query<&mut crate::icons::SvgColor, With<CtkButtonLabel>>,
) {
    for (entity, button, hovered, pressed, disabled, children) in &buttons {
        let Some(children) = children else {
            continue;
        };
        let state = visual_state(hovered, pressed, disabled);
        let focused =
            !disabled && has_visible_focus(entity, focus.as_deref(), focus_visible.as_deref());
        let Some(cell) = design.button_cell(button_cell_key(button, state, focused)) else {
            continue;
        };
        let colour = bevy_color(cell.pair.foreground);
        for child in children.iter() {
            let Ok(mut svg) = icons.get_mut(*child) else {
                continue;
            };
            if svg.0 != colour {
                svg.0 = colour;
            }
        }
    }
}

pub(crate) fn warn_button_marker_collisions(
    mut diagnostics: ResMut<ButtonDiagnostics>,
    buttons: Query<
        (
            Entity,
            Has<bevy::ui_widgets::Button>,
            Has<crate::widgets::ToggleButton>,
        ),
        With<CtkButton>,
    >,
) {
    for (entity, bevy_button, toggle) in &buttons {
        if !bevy_button && !toggle {
            continue;
        }
        if diagnostics.marker_collisions_warned.insert(entity) {
            warn!(
                "CtkButton {entity} has an incompatible marker collision: Bevy Button={bevy_button}, CTK ToggleButton={toggle}; remove the competing marker"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::camera::NormalizedRenderTarget;
    use bevy::ecs::observer::On;
    use bevy::input::keyboard::{Key, KeyCode, KeyboardInput};
    use bevy::input::{ButtonState, InputPlugin};
    use bevy::input_focus::FocusCause;
    use bevy::input_focus::{FocusedInput, InputDispatchPlugin, InputFocusPlugin};
    use bevy::picking::backend::HitData;
    use bevy::picking::events::{Click, Pointer, Press};
    use bevy::picking::pointer::{Location, PointerButton, PointerId};
    use bevy::prelude::{App, Children, EntityEvent, Vec2};
    #[cfg(feature = "theme")]
    use bevy::prelude::{IntoScheduleConfigs, Update};
    use bevy::window::{PrimaryWindow, Window, WindowRef};
    use core::time::Duration;
    use cosmix_design::{
        parse_design_source, DesignCompileOutcome, ResolvedButtonCell, SourceIdentity,
        EMBEDDED_DEFAULT_SOURCE,
    };

    use crate::design::{design_resources_for_source, CtkDesignStatus};
    #[cfg(feature = "theme")]
    use crate::theme::{
        apply_theme_requests, reload_theme_files, theme_file_watcher, ApplyTheme,
        ThemeReloadSignal, ThemeRuntimeConfig, THEME_FILE,
    };
    use crate::theme::{Mode, Scheme, ThemeSpec, ThemeState};
    use crate::widgets::{ControlChange, CtkWidgetsPlugin};

    fn test_app(spec: &ThemeSpec) -> App {
        let (design, design_status) = design_resources_for_source(
            "test:embedded",
            EMBEDDED_DEFAULT_SOURCE,
            spec.scheme,
            spec.mode,
        );
        let mut typography = CtkTypography::default();
        typography.body_px = spec.typography.body_px;
        let mut state = ThemeState::default();
        state.scheme = spec.scheme;
        state.mode = spec.mode;

        let mut app = App::new();
        app.insert_resource(state)
            .insert_resource(typography)
            .insert_resource(design)
            .insert_resource(design_status)
            .add_plugins(CtkWidgetsPlugin)
            .add_systems(bevy::app::PostUpdate, reconcile_button_label_fonts);
        app
    }

    fn spawn(app: &mut App, def: ButtonDef) -> Entity {
        let button = spawn_button(&mut app.world_mut().commands(), def);
        app.world_mut().flush();
        button
    }

    fn text_label(app: &App, button: Entity) -> Entity {
        app.world()
            .get::<Children>(button)
            .expect("button has label children")
            .iter()
            .copied()
            .find(|entity| app.world().get::<Text>(*entity).is_some())
            .expect("button has a text label")
    }

    fn trigger_pointer_press(app: &mut App, button: Entity) {
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(button).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(button, 0.0, None, None),
                count: 1,
            },
            button,
        ));
    }

    fn trigger_pointer_click(app: &mut App, button: Entity) {
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(button).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(button, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            button,
        ));
    }

    fn cell(
        app: &App,
        button: Entity,
        state: InteractionVisualState,
        focused: bool,
    ) -> ResolvedButtonCell {
        let button = app.world().get::<CtkButton>(button).unwrap();
        app.world()
            .resource::<CtkDesign>()
            .button_cell(button_cell_key(button, state, focused))
            .unwrap()
            .clone()
    }

    fn assert_root_matches_cell(app: &App, entity: Entity, cell: &ResolvedButtonCell) {
        let world = app.world();
        let node = world.get::<Node>(entity).unwrap();
        assert_eq!(node.height, px(cell.height as f32));
        assert_eq!(node.min_width, px(cell.min_width as f32));
        assert_eq!(node.padding, UiRect::horizontal(px(cell.padding_x as f32)));
        assert_eq!(node.border, UiRect::all(px(cell.border_width as f32)));
        assert_eq!(
            node.border_radius,
            BorderRadius::all(px(cell.radius as f32))
        );
        assert_eq!(
            world.get::<BackgroundColor>(entity).unwrap().0,
            bevy_color(cell.pair.surface)
        );
        assert_eq!(
            *world.get::<BorderColor>(entity).unwrap(),
            BorderColor::all(cell.ring.or(cell.border).map_or(Color::NONE, bevy_color))
        );
        let label = text_label(app, entity);
        assert_eq!(
            world.get::<TextColor>(label).unwrap().0,
            bevy_color(cell.pair.foreground)
        );
    }

    #[test]
    fn interaction_adapter_is_total_and_exact() {
        let button = CtkButton {
            variant: ButtonVariant::Destructive,
            size: ButtonSize::Lg,
        };
        for (visual, interaction) in [
            (InteractionVisualState::Resting, InteractionState::Resting),
            (InteractionVisualState::Hovered, InteractionState::Hovered),
            (InteractionVisualState::Pressed, InteractionState::Pressed),
            (InteractionVisualState::Disabled, InteractionState::Disabled),
        ] {
            assert_eq!(
                button_cell_key(&button, visual, true),
                ButtonCellKey {
                    variant: ButtonVariant::Destructive,
                    size: ButtonSize::Lg,
                    interaction,
                    focus_visible: true,
                }
            );
        }
    }

    #[test]
    fn spawn_contract_is_focusable_accessible_and_has_one_input_path() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let entity = spawn(
            &mut app,
            ButtonDef::text("Save")
                .bus("save")
                .variant(ButtonVariant::Primary),
        );

        let world = app.world();
        assert_eq!(world.get::<TabIndex>(entity), Some(&TabIndex(0)));
        assert!(world.get::<ActivateOnPress>(entity).is_some());
        assert!(world.get::<bevy::ui_widgets::Button>(entity).is_none());
        assert_eq!(
            world.get::<AccessibilityNode>(entity).unwrap().0.role(),
            Role::Button
        );
        assert_eq!(world.get::<BusWidget>(entity).unwrap().id, "save");
    }

    #[test]
    fn added_observers_apply_the_compiled_cell_before_update() {
        let spec = ThemeSpec::from_scheme(Scheme::Forest, Mode::Dark);
        let mut app = test_app(&spec);
        let entity = spawn(
            &mut app,
            ButtonDef::text("Open")
                .variant(ButtonVariant::Primary)
                .size(ButtonSize::Lg),
        );
        let expected = cell(&app, entity, InteractionVisualState::Resting, false);

        assert_root_matches_cell(&app, entity, &expected);
    }

    #[test]
    fn every_variant_size_and_interaction_paints_from_the_compiled_cell() {
        for variant in ButtonVariant::ALL {
            for size in ButtonSize::ALL {
                let spec = ThemeSpec::from_scheme(Scheme::Sunset, Mode::Light);
                let mut app = test_app(&spec);
                let entity = spawn(
                    &mut app,
                    ButtonDef::text("State").variant(variant).size(size),
                );

                for state in [
                    InteractionVisualState::Resting,
                    InteractionVisualState::Hovered,
                    InteractionVisualState::Pressed,
                    InteractionVisualState::Disabled,
                ] {
                    app.world_mut().entity_mut(entity).remove::<Hovered>();
                    app.world_mut()
                        .entity_mut(entity)
                        .insert(Hovered(matches!(state, InteractionVisualState::Hovered)));
                    if matches!(state, InteractionVisualState::Pressed) {
                        app.world_mut().entity_mut(entity).insert(Pressed);
                    } else {
                        app.world_mut().entity_mut(entity).remove::<Pressed>();
                    }
                    if matches!(state, InteractionVisualState::Disabled) {
                        app.world_mut()
                            .entity_mut(entity)
                            .insert(InteractionDisabled);
                    } else {
                        app.world_mut()
                            .entity_mut(entity)
                            .remove::<InteractionDisabled>();
                    }
                    app.update();

                    let expected = cell(&app, entity, state, false);
                    assert_root_matches_cell(&app, entity, &expected);
                }
            }
        }
    }

    #[test]
    fn visible_focus_uses_ring_then_border() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let entity = spawn(&mut app, ButtonDef::text("Focus"));
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(entity, FocusCause::Navigated);
        app.world_mut().resource_mut::<InputFocusVisible>().0 = true;
        app.update();

        let focused = cell(&app, entity, InteractionVisualState::Resting, true);
        assert_eq!(
            *app.world().get::<BorderColor>(entity).unwrap(),
            BorderColor::all(
                focused
                    .ring
                    .or(focused.border)
                    .map_or(Color::NONE, bevy_color)
            )
        );

        app.world_mut().resource_mut::<InputFocusVisible>().0 = false;
        app.update();
        let resting = cell(&app, entity, InteractionVisualState::Resting, false);
        assert_eq!(
            *app.world().get::<BorderColor>(entity).unwrap(),
            BorderColor::all(
                resting
                    .ring
                    .or(resting.border)
                    .map_or(Color::NONE, bevy_color)
            )
        );
    }

    #[test]
    fn button_definition_mutation_rekeys_colours_and_metrics_live() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let entity = spawn(&mut app, ButtonDef::text("Resize"));
        app.world_mut().entity_mut(entity).insert(CtkButton {
            variant: ButtonVariant::Destructive,
            size: ButtonSize::Lg,
        });
        app.update();

        let expected = cell(&app, entity, InteractionVisualState::Resting, false);
        assert_root_matches_cell(&app, entity, &expected);
    }

    #[allow(deprecated)]
    #[test]
    fn deprecated_button_metrics_are_inert_for_existing_buttons() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let entity = spawn(&mut app, ButtonDef::text("Stable geometry"));
        app.update();
        let expected = cell(&app, entity, InteractionVisualState::Resting, false);

        {
            let mut metrics = app
                .world_mut()
                .resource_mut::<crate::theme::CtkThemeMetrics>();
            metrics.button_height = [80.0, 90.0, 100.0];
            metrics.button_min_width = 500.0;
            metrics.button_pad_h = 70.0;
            metrics.button_border = 20.0;
        }
        app.update();

        assert_root_matches_cell(&app, entity, &expected);
    }

    #[test]
    fn button_owned_typography_stamps_resolved_family_and_never_reverts_it() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("resolved-test-family".to_string());
            typography.body_px = 16.0;
        }
        let button = spawn(&mut app, ButtonDef::text("Themed face"));
        let label = text_label(&app, button);
        let font = app.world().get::<TextFont>(label).unwrap();
        assert_eq!(font.font, FontSource::SansSerif);
        assert_eq!(font.font_size, FontSize::Px(16.0));

        *app.world_mut().get_mut::<TextFont>(label).unwrap() = TextFont::from_font_size(99.0);
        app.update();
        let font = app.world().get::<TextFont>(label).unwrap();
        assert_eq!(font.font, FontSource::SansSerif);
        assert_eq!(font.font_size, FontSize::Px(16.0));

        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = None;
            typography.body_px = 18.0;
        }
        app.update();
        let font = app.world().get::<TextFont>(label).unwrap();
        assert_eq!(font.font, FontSource::SansSerif);
        assert_eq!(font.font_size, FontSize::Px(18.0));
    }

    #[test]
    fn in_memory_source_replacement_restyles_an_existing_button() {
        let mut app = test_app(&ThemeSpec::builtin());
        let entity = spawn(
            &mut app,
            ButtonDef::text("Delete").variant(ButtonVariant::Destructive),
        );
        app.update();
        let old_background = app.world().get::<BackgroundColor>(entity).unwrap().0;
        let label = text_label(&app, entity);
        let old_foreground = app.world().get::<TextColor>(label).unwrap().0;
        let old_node = app.world().get::<Node>(entity).unwrap();
        let old_height = old_node.height;
        let old_min_width = old_node.min_width;
        let old_padding = old_node.padding;
        let old_border = old_node.border;
        let old_radius = old_node.border_radius;
        let old_revision = app.world().resource::<CtkDesign>().revision().unwrap();

        let source = EMBEDDED_DEFAULT_SOURCE
            .replacen("meter_red: \"#c21725\"", "meter_red: \"#006818\"", 1)
            .replacen(
                "danger_surface: \"#c21725\"",
                "danger_surface: \"#006818\"",
                1,
            )
            .replacen(
                "\"status.danger\": { color_space: \"oklch\", l: 0.52, c: 0.20, h: 25.0 }",
                "\"status.danger\": { color_space: \"oklch\", l: 0.45, c: 0.15, h: 145.0 }",
                1,
            )
            .replacen(
                "destructive: { surface: \"status.danger\", foreground: \"palette.background.1\" }",
                "destructive: { surface: \"status.danger\", foreground: \"palette.background.3\" }",
                1,
            )
            .replacen(
                "\"button.height.md\": { kind: \"px\", value: 28.0 }",
                "\"button.height.md\": { kind: \"px\", value: 30.0 }",
                1,
            )
            .replacen(
                "\"button.min_width.standard\": { kind: \"px\", value: 72.0 }",
                "\"button.min_width.standard\": { kind: \"px\", value: 80.0 }",
                1,
            )
            .replacen(
                "\"button.padding_x\": { kind: \"step\", scale: \"spacing\", value: 5 }",
                "\"button.padding_x\": { kind: \"step\", scale: \"spacing\", value: 6 }",
                1,
            )
            .replacen(
                "\"button.border_width\": { kind: \"px\", value: 1.0 }",
                "\"button.border_width\": { kind: \"px\", value: 2.0 }",
                1,
            )
            .replacen(
                "\"radius\": { kind: \"px\", value: 6.0 }",
                "\"radius\": { kind: \"px\", value: 10.0 }",
                1,
            );
        app.world_mut()
            .resource_mut::<CtkDesignStatus>()
            .replace_source("memory:reload", source);
        app.update();

        let expected = cell(&app, entity, InteractionVisualState::Resting, false);
        assert_root_matches_cell(&app, entity, &expected);
        assert_ne!(
            app.world().get::<BackgroundColor>(entity).unwrap().0,
            old_background
        );
        assert_ne!(
            app.world().get::<TextColor>(label).unwrap().0,
            old_foreground
        );
        let node = app.world().get::<Node>(entity).unwrap();
        assert_ne!(node.height, old_height);
        assert_ne!(node.min_width, old_min_width);
        assert_ne!(node.padding, old_padding);
        assert_ne!(node.border, old_border);
        assert_ne!(node.border_radius, old_radius);
        assert_eq!(node.height, px(30));
        assert_eq!(node.min_width, px(80));
        assert_eq!(node.padding, UiRect::horizontal(px(12)));
        assert_eq!(node.border, UiRect::all(px(2)));
        assert_eq!(node.border_radius, BorderRadius::all(px(8)));
        assert_eq!(
            app.world()
                .resource::<CtkDesign>()
                .revision()
                .unwrap()
                .get(),
            old_revision.get() + 1
        );
        assert!(app
            .world()
            .resource::<CtkDesignStatus>()
            .last_error()
            .is_none());
    }

    #[cfg(feature = "theme")]
    #[test]
    fn disk_edit_restyles_an_existing_buttons_colour_and_height_once() {
        let temp = tempfile::TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        std::fs::write(&shared_path, EMBEDDED_DEFAULT_SOURCE).unwrap();
        let reload = ThemeReloadSignal::default();
        reload.request_reload();
        let mut app = test_app(&ThemeSpec::builtin());
        app.insert_resource(ThemeRuntimeConfig {
            shared_path: shared_path.clone(),
            app_config_dir: None,
        })
        .init_resource::<crate::theme::ThemeLayerLastGood>()
        .insert_resource(reload)
        .add_message::<ApplyTheme>()
        .add_systems(Update, (reload_theme_files, apply_theme_requests).chain());
        let entity = spawn(
            &mut app,
            ButtonDef::text("Live").variant(ButtonVariant::Destructive),
        );
        app.update();
        let before_revision = app.world().resource::<CtkDesign>().revision().unwrap();
        let before_background = app.world().get::<BackgroundColor>(entity).unwrap().0;
        let before_height = app.world().get::<Node>(entity).unwrap().height;
        let (woke_tx, woke_rx) = std::sync::mpsc::sync_channel(1);
        let wake = std::sync::Arc::new(move || {
            let _ = woke_tx.try_send(());
        });
        let _watcher = theme_file_watcher(
            vec![shared_path.clone()],
            app.world().resource::<ThemeReloadSignal>().clone(),
            wake,
        )
        .unwrap();

        let edited = EMBEDDED_DEFAULT_SOURCE
            .replacen("meter_red: \"#c21725\"", "meter_red: \"#006818\"", 1)
            .replacen(
                "danger_surface: \"#c21725\"",
                "danger_surface: \"#006818\"",
                1,
            )
            .replacen(
                "\"status.danger\": { color_space: \"oklch\", l: 0.52, c: 0.20, h: 25.0 }",
                "\"status.danger\": { color_space: \"oklch\", l: 0.45, c: 0.15, h: 145.0 }",
                1,
            )
            .replacen(
                "\"button.height.md\": { kind: \"px\", value: 28.0 }",
                "\"button.height.md\": { kind: \"px\", value: 30.0 }",
                1,
            );
        std::fs::write(&shared_path, edited).unwrap();
        woke_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("directory watcher did not request the disk reload");
        app.update();

        assert_ne!(
            app.world().get::<BackgroundColor>(entity).unwrap().0,
            before_background
        );
        assert_ne!(
            app.world().get::<Node>(entity).unwrap().height,
            before_height
        );
        assert_eq!(app.world().get::<Node>(entity).unwrap().height, px(30));
        assert_eq!(
            app.world()
                .resource::<CtkDesign>()
                .revision()
                .unwrap()
                .get(),
            before_revision.get() + 1
        );
    }

    #[test]
    fn invalid_in_memory_source_keeps_existing_button_on_last_good_cell() {
        let mut app = App::new();
        app.add_plugins((crate::theme::CtkThemePlugin::default(), CtkWidgetsPlugin));
        let entity = spawn(&mut app, ButtonDef::text("Safe"));
        app.update();
        let before_revision = app.world().resource::<CtkDesign>().revision();
        let before_background = app.world().get::<BackgroundColor>(entity).unwrap().0;

        app.world_mut()
            .resource_mut::<CtkDesignStatus>()
            .replace_source("memory:bad", "design: nope");
        app.update();

        assert_eq!(
            app.world().resource::<CtkDesign>().revision(),
            before_revision
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(entity).unwrap().0,
            before_background
        );
        assert!(app
            .world()
            .resource::<CtkDesignStatus>()
            .last_error()
            .is_some());
    }

    #[test]
    fn fatal_compilation_keeps_existing_button_on_last_good_cell() {
        let mut app = test_app(&ThemeSpec::builtin());
        let entity = spawn(&mut app, ButtonDef::text("Safe"));
        app.update();
        let label = text_label(&app, entity);
        let before_revision = app.world().resource::<CtkDesign>().revision();
        let before_background = app.world().get::<BackgroundColor>(entity).unwrap().0;
        let before_border = *app.world().get::<BorderColor>(entity).unwrap();
        let before_foreground = app.world().get::<TextColor>(label).unwrap().0;
        let before_node = app.world().get::<Node>(entity).unwrap();
        let before_metrics = (
            before_node.height,
            before_node.min_width,
            before_node.padding,
            before_node.border,
            before_node.border_radius,
        );

        let source = EMBEDDED_DEFAULT_SOURCE.replacen(
            "pair: { kind: \"pair\", value: \"secondary\" }",
            "pair: { kind: \"pair\", value: \"undefined-pair\" }",
            1,
        );
        assert_ne!(source, EMBEDDED_DEFAULT_SOURCE);
        assert!(
            parse_design_source(SourceIdentity::new("memory:fatal"), &source).is_ok(),
            "the fixture must reach compilation rather than the parse-error arm"
        );
        app.world_mut()
            .resource_mut::<CtkDesignStatus>()
            .replace_source("memory:fatal", source);
        app.update();

        assert_eq!(
            app.world().resource::<CtkDesign>().revision(),
            before_revision
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(entity).unwrap().0,
            before_background
        );
        assert_eq!(
            app.world().get::<BorderColor>(entity).unwrap(),
            &before_border
        );
        assert_eq!(
            app.world().get::<TextColor>(label).unwrap().0,
            before_foreground
        );
        let node = app.world().get::<Node>(entity).unwrap();
        assert_eq!(
            (
                node.height,
                node.min_width,
                node.padding,
                node.border,
                node.border_radius,
            ),
            before_metrics
        );
        let status = app.world().resource::<CtkDesignStatus>();
        assert_eq!(
            status.last_compile().map(|compile| compile.outcome),
            Some(DesignCompileOutcome::Fatal)
        );
        assert!(status.last_error().is_some());
    }

    #[cfg(feature = "icons")]
    #[test]
    fn icon_foreground_comes_from_the_full_compiled_cell_key() {
        let spec = ThemeSpec::from_scheme(Scheme::Crimson, Mode::Dark);
        let mut app = test_app(&spec);
        app.insert_resource(crate::icons::IconSet::placeholder_for_test(&[
            crate::icons::Icon::Info,
        ]));
        let entity = spawn(
            &mut app,
            ButtonDef::icon(crate::icons::Icon::Info, "Info")
                .variant(ButtonVariant::Primary)
                .size(ButtonSize::Lg),
        );
        app.update();
        let icon = app
            .world()
            .get::<Children>(entity)
            .unwrap()
            .iter()
            .copied()
            .find(|child| app.world().get::<crate::icons::UiSvg>(*child).is_some())
            .unwrap();

        app.world_mut().entity_mut(entity).remove::<Hovered>();
        app.world_mut().entity_mut(entity).insert(Hovered(true));
        app.update();

        let expected = cell(&app, entity, InteractionVisualState::Hovered, false);
        assert_eq!(
            app.world().get::<crate::icons::SvgColor>(icon).unwrap().0,
            bevy_color(expected.pair.foreground)
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn installed_icon_set_materialises_icon_text_with_accessibility() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        app.insert_resource(crate::icons::IconSet::placeholder_for_test(&[
            crate::icons::Icon::Info,
        ]));
        let button = spawn(
            &mut app,
            ButtonDef::icon_text(crate::icons::Icon::Info, "Details")
                .variant(ButtonVariant::Primary),
        );
        let children: Vec<_> = app
            .world()
            .get::<Children>(button)
            .unwrap()
            .iter()
            .copied()
            .collect();
        assert_eq!(children.len(), 2);
        assert!(app.world().get::<PendingButtonLabel>(button).is_none());
        assert!(app
            .world()
            .get::<crate::icons::UiSvg>(children[0])
            .is_some());
        assert_eq!(
            app.world().get::<Text>(children[1]).unwrap().as_str(),
            "Details"
        );
        assert_eq!(
            app.world()
                .get::<AccessibilityNode>(button)
                .unwrap()
                .label(),
            Some("Details")
        );
        let expected = cell(&app, button, InteractionVisualState::Resting, false);
        assert_eq!(
            app.world()
                .get::<crate::icons::SvgColor>(children[0])
                .unwrap()
                .0,
            bevy_color(expected.pair.foreground)
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn runtime_disable_and_enable_rekeys_icon_foreground() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        app.insert_resource(crate::icons::IconSet::placeholder_for_test(&[
            crate::icons::Icon::Info,
        ]));
        let button = spawn(
            &mut app,
            ButtonDef::icon(crate::icons::Icon::Info, "Information")
                .variant(ButtonVariant::Primary),
        );
        let icon = app.world().get::<Children>(button).unwrap()[0];

        app.world_mut()
            .entity_mut(button)
            .insert(InteractionDisabled);
        app.world_mut().flush();
        let disabled = cell(&app, button, InteractionVisualState::Disabled, false);
        assert_eq!(
            app.world().get::<crate::icons::SvgColor>(icon).unwrap().0,
            bevy_color(disabled.pair.foreground)
        );

        app.world_mut()
            .entity_mut(button)
            .remove::<InteractionDisabled>();
        app.world_mut().flush();
        let resting = cell(&app, button, InteractionVisualState::Resting, false);
        assert_eq!(
            app.world().get::<crate::icons::SvgColor>(icon).unwrap().0,
            bevy_color(resting.pair.foreground)
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn missing_icon_set_warning_is_latched() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        spawn(
            &mut app,
            ButtonDef::icon(crate::icons::Icon::Info, "Information"),
        );
        app.update();
        assert!(
            app.world()
                .resource::<ButtonDiagnostics>()
                .missing_icon_set_warned
        );
        app.update();
        assert!(
            app.world()
                .resource::<ButtonDiagnostics>()
                .missing_icon_set_warned
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn live_size_and_typography_changes_resize_icon_children() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        app.insert_resource(crate::icons::IconSet::placeholder_for_test(&[
            crate::icons::Icon::Info,
        ]));
        let button = spawn(
            &mut app,
            ButtonDef::icon(crate::icons::Icon::Info, "Information").size(ButtonSize::Sm),
        );
        app.update();
        let icon = app.world().get::<Children>(button).unwrap()[0];
        assert_eq!(
            app.world().get::<Node>(icon).unwrap().width,
            px(spec.typography.body_px - 2.0)
        );

        app.world_mut().get_mut::<CtkButton>(button).unwrap().size = ButtonSize::Lg;
        app.update();
        assert_eq!(
            app.world().get::<Node>(icon).unwrap().width,
            px(spec.typography.body_px)
        );

        app.world_mut().resource_mut::<CtkTypography>().body_px = 20.0;
        app.update();
        let node = app.world().get::<Node>(icon).unwrap();
        assert_eq!(node.width, px(20));
        assert_eq!(node.min_width, px(20));
        assert_eq!(node.height, px(20));
    }

    #[test]
    fn incompatible_marker_collision_is_diagnosed_once_per_entity() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let collided = spawn(&mut app, ButtonDef::text("Collision"));
        app.world_mut()
            .entity_mut(collided)
            .insert(bevy::ui_widgets::Button);
        let sanctioned = spawn(
            &mut app,
            ButtonDef::text("Bus action").bus("test.sanctioned"),
        );
        app.update();

        let diagnostics = app.world().resource::<ButtonDiagnostics>();
        assert!(diagnostics.marker_collisions_warned.contains(&collided));
        assert!(!diagnostics.marker_collisions_warned.contains(&sanctioned));
        assert_eq!(diagnostics.marker_collisions_warned.len(), 1);

        app.world_mut()
            .entity_mut(collided)
            .insert(crate::widgets::ToggleButton);
        app.update();
        assert_eq!(
            app.world()
                .resource::<ButtonDiagnostics>()
                .marker_collisions_warned
                .len(),
            1
        );
    }

    #[derive(Resource, Default)]
    struct ActivateCount(usize);

    #[derive(Resource, Default)]
    struct ChangeCount(Vec<bool>);

    fn count_activate(_: On<bevy::ui_widgets::Activate>, mut count: ResMut<ActivateCount>) {
        count.0 += 1;
    }

    fn count_change(change: On<ControlChange>, mut count: ResMut<ChangeCount>) {
        count.0.push(change.is_final);
    }

    fn activation_app(spec: &ThemeSpec) -> (App, Entity) {
        let mut app = test_app(spec);
        app.init_resource::<bevy::ui::UiScale>()
            .add_plugins((InputPlugin, InputFocusPlugin, InputDispatchPlugin))
            .init_resource::<ActivateCount>()
            .init_resource::<ChangeCount>()
            .add_observer(count_activate)
            .add_observer(count_change);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        (app, window)
    }

    #[test]
    fn disabled_button_uses_its_cell_and_swallows_pointer_and_key_activation() {
        let spec = ThemeSpec::builtin();
        let (mut app, window) = activation_app(&spec);
        let button = spawn(
            &mut app,
            ButtonDef::text("Disabled").bus("test.disabled").disabled(),
        );
        app.world_mut()
            .insert_resource(InputFocus::from_entity(button));
        app.update();
        let expected = cell(&app, button, InteractionVisualState::Disabled, false);
        assert_root_matches_cell(&app, button, &expected);

        trigger_pointer_press(&mut app, button);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Space,
            logical_key: Key::Space,
            state: ButtonState::Pressed,
            text: Some(" ".into()),
            repeat: false,
            window,
        });
        app.update();

        assert_eq!(app.world().resource::<ActivateCount>().0, 0);
        assert!(app.world().resource::<ChangeCount>().0.is_empty());
    }

    #[test]
    fn enabled_bus_button_publishes_once_for_a_pointer_sequence() {
        let spec = ThemeSpec::builtin();
        let (mut app, _) = activation_app(&spec);
        let button = spawn(&mut app, ButtonDef::text("Save").bus("test.save"));
        trigger_pointer_press(&mut app, button);
        trigger_pointer_click(&mut app, button);
        app.update();

        assert_eq!(app.world().resource::<ActivateCount>().0, 1);
        assert_eq!(app.world().resource::<ChangeCount>().0, [true]);
    }

    #[test]
    fn busless_button_exposes_activate_without_control_change() {
        let spec = ThemeSpec::builtin();
        let (mut app, _) = activation_app(&spec);
        let button = spawn(&mut app, ButtonDef::text("Local action"));
        trigger_pointer_press(&mut app, button);
        trigger_pointer_click(&mut app, button);
        app.update();

        assert_eq!(app.world().resource::<ActivateCount>().0, 1);
        assert!(app.world().resource::<ChangeCount>().0.is_empty());
    }

    #[test]
    fn focused_key_activation_publishes_exactly_once() {
        let spec = ThemeSpec::builtin();
        let (mut app, window) = activation_app(&spec);
        let button = spawn(
            &mut app,
            ButtonDef::text("Keyboard save").bus("test.keyboard-save"),
        );
        *app.world_mut().resource_mut::<InputFocus>() = InputFocus::from_entity(button);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Enter,
            logical_key: Key::Enter,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();

        assert_eq!(app.world().resource::<ActivateCount>().0, 1);
        assert_eq!(app.world().resource::<ChangeCount>().0, [true]);
    }

    #[test]
    fn disabled_and_repeated_activation_keys_bubble_to_the_parent() {
        #[derive(Component)]
        struct Parent;
        #[derive(Resource, Default)]
        struct Bubbled(usize);
        fn record(
            input: On<FocusedInput<KeyboardInput>>,
            parents: Query<(), With<Parent>>,
            mut bubbled: ResMut<Bubbled>,
        ) {
            if parents.contains(input.event_target()) {
                bubbled.0 += 1;
            }
        }

        let spec = ThemeSpec::builtin();
        let (mut app, window) = activation_app(&spec);
        app.init_resource::<Bubbled>().add_observer(record);
        let parent = app.world_mut().spawn(Parent).id();
        let button = spawn(&mut app, ButtonDef::text("Bubble").disabled());
        app.world_mut().entity_mut(parent).add_child(button);
        *app.world_mut().resource_mut::<InputFocus>() = InputFocus::from_entity(button);

        for (disabled, repeat) in [(true, false), (false, true)] {
            if !disabled {
                app.world_mut()
                    .entity_mut(button)
                    .remove::<InteractionDisabled>();
            }
            app.world_mut().write_message(KeyboardInput {
                key_code: KeyCode::Space,
                logical_key: Key::Space,
                state: ButtonState::Pressed,
                text: Some(" ".into()),
                repeat,
                window,
            });
            app.update();
        }

        assert_eq!(app.world().resource::<Bubbled>().0, 2);
        assert_eq!(app.world().resource::<ActivateCount>().0, 0);
        assert!(app.world().resource::<ChangeCount>().0.is_empty());
    }

    #[test]
    fn settled_label_font_is_not_marked_changed_every_frame() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let entity = spawn(&mut app, ButtonDef::text("Stable"));
        app.update();
        app.update();

        let label = text_label(&app, entity);
        let settled = app
            .world()
            .entity(label)
            .get_ref::<TextFont>()
            .unwrap()
            .last_changed();
        app.update();
        let after = app
            .world()
            .entity(label)
            .get_ref::<TextFont>()
            .unwrap()
            .last_changed();
        assert_eq!(settled, after);
    }
}
