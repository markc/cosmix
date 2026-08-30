//! Canonical CTK button: one variant/size surface with live theme styling.
//!
//! CTK deliberately diverges from shadcn's opacity-50 disabled treatment on
//! the desktop: every non-ghost variant uses the same neutral disabled control
//! surface, border and dim foreground, while Ghost remains fully transparent.
//! This preserves the theme's validated `text.dim`-on-control pairing instead
//! of making disabled contrast depend on the enabled variant colour.
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
use bevy::feathers::theme::UiTheme;
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

use crate::style::{
    contrast_safe_lift, lighten, selected_background, InteractionVisualState, DISABLED_LIFT,
};
use crate::theme::{ctk_color, tokens, CtkThemeMetrics, CtkTypography, CtkTypographyOptOut};
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

fn button_colours(
    theme: &UiTheme,
    variant: ButtonVariant,
    state: InteractionVisualState,
) -> (Color, Color, Color) {
    if matches!(state, InteractionVisualState::Disabled) {
        let text = ctk_color(theme, &tokens::TEXT_DIM);
        return if variant == ButtonVariant::Ghost {
            (Color::NONE, Color::NONE, text)
        } else {
            (
                contrast_safe_lift(ctk_color(theme, &tokens::CONTROL), -DISABLED_LIFT, &[text]),
                ctk_color(theme, &tokens::BORDER),
                text,
            )
        };
    }

    let text = if variant == ButtonVariant::Primary {
        ctk_color(theme, &tokens::ROW_SELECTED_TEXT)
    } else {
        ctk_color(theme, &tokens::TEXT)
    };
    let (background, border) = match variant {
        ButtonVariant::Default => (
            lighten(ctk_color(theme, &tokens::CONTROL), state.legacy_lift()),
            ctk_color(theme, &tokens::BORDER),
        ),
        ButtonVariant::Primary => (
            selected_background(theme, state),
            ctk_color(theme, &tokens::BORDER),
        ),
        ButtonVariant::Destructive => (
            lighten(
                ctk_color(theme, &tokens::DANGER_SURFACE),
                state.legacy_lift(),
            ),
            ctk_color(theme, &tokens::METER_RED),
        ),
        ButtonVariant::Ghost => {
            let background = if matches!(state, InteractionVisualState::Resting) {
                Color::NONE
            } else {
                lighten(ctk_color(theme, &tokens::CONTROL), state.legacy_lift())
            };
            (background, Color::NONE)
        }
    };
    (background, border, text)
}

fn has_visible_focus(
    entity: Entity,
    focus: Option<&InputFocus>,
    focus_visible: Option<&InputFocusVisible>,
) -> bool {
    focus_visible.is_some_and(|visible| visible.0)
        && focus.and_then(InputFocus::get) == Some(entity)
}

fn focused_border(
    theme: &UiTheme,
    variant: ButtonVariant,
    resting_border: Color,
    focused: bool,
) -> Color {
    if focused {
        if variant == ButtonVariant::Primary {
            ctk_color(theme, &tokens::ROW_SELECTED_TEXT)
        } else {
            ctk_color(theme, &tokens::ROW_SELECTED)
        }
    } else {
        resting_border
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn update_button_style(
    theme: Res<UiTheme>,
    focus: Option<Res<InputFocus>>,
    focus_visible: Option<Res<InputFocusVisible>>,
    mut buttons: Query<(
        Entity,
        &Hovered,
        Has<Pressed>,
        Has<InteractionDisabled>,
        &CtkButton,
        &mut BackgroundColor,
        &mut BorderColor,
        Option<&Children>,
    )>,
    mut labels: Query<&mut TextColor, With<CtkButtonLabel>>,
) {
    for (entity, hovered, pressed, disabled, button, mut background, mut border, children) in
        &mut buttons
    {
        let state = visual_state(hovered, pressed, disabled);
        let (want_background, resting_border, want_text) =
            button_colours(&theme, button.variant, state);
        let want_border = focused_border(
            &theme,
            button.variant,
            resting_border,
            !disabled && has_visible_focus(entity, focus.as_deref(), focus_visible.as_deref()),
        );

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

fn apply_button_metrics(metrics: &CtkThemeMetrics, button: &CtkButton, node: &mut Node) {
    let min_width = if button.size == ButtonSize::Sm {
        0.0
    } else {
        metrics.button_min_width
    };
    let height = px(metrics.button_height[button.size.index()]);
    let min_width = px(min_width);
    let padding = UiRect::horizontal(px(metrics.button_pad_h));
    let border = UiRect::all(px(metrics.button_border));
    let radius = BorderRadius::all(px(metrics.radius.md));
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
    theme: &'a UiTheme,
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
    let (want_background, resting_border, _) =
        button_colours(context.theme, context.button.variant, state);
    let want_border = focused_border(
        context.theme,
        context.button.variant,
        resting_border,
        !context.disabled
            && has_visible_focus(context.entity, context.focus, context.focus_visible),
    );
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
    theme: Res<UiTheme>,
    metrics: Res<CtkThemeMetrics>,
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
    apply_button_metrics(&metrics, button, &mut node);
    paint_button_root(
        ButtonPaintContext {
            entity: add.entity,
            theme: &theme,
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
    theme: Res<UiTheme>,
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
    let (_, _, want_colour) = button_colours(&theme, button.variant, state);
    if colour.0 != want_colour {
        colour.0 = want_colour;
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn paint_disabled_button(
    add: On<Add, InteractionDisabled>,
    theme: Res<UiTheme>,
    mut buttons: Query<(
        &CtkButton,
        &Hovered,
        Has<Pressed>,
        &mut BackgroundColor,
        &mut BorderColor,
        Option<&Children>,
    )>,
    mut labels: Query<&mut TextColor, With<CtkButtonLabel>>,
    #[cfg(feature = "icons")] mut icons: Query<
        (
            &mut crate::icons::SvgColor,
            &mut crate::icons::ThemeSvgColor,
        ),
        With<CtkButtonLabel>,
    >,
) {
    let Ok((button, hovered, pressed, mut background, mut border, children)) =
        buttons.get_mut(add.entity)
    else {
        return;
    };
    paint_button_root(
        ButtonPaintContext {
            entity: add.entity,
            theme: &theme,
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
    let (_, _, want_text) =
        button_colours(&theme, button.variant, InteractionVisualState::Disabled);
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
        if let Ok((mut svg, mut retained)) = icons.get_mut(*child) {
            if retained.0 != tokens::TEXT_DIM {
                retained.0 = tokens::TEXT_DIM;
            }
            if svg.0 != want_text {
                svg.0 = want_text;
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn paint_enabled_button(
    remove: On<Remove, InteractionDisabled>,
    theme: Res<UiTheme>,
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
    #[cfg(feature = "icons")] mut icons: Query<
        (
            &mut crate::icons::SvgColor,
            &mut crate::icons::ThemeSvgColor,
        ),
        With<CtkButtonLabel>,
    >,
) {
    let Ok((button, hovered, pressed, mut background, mut border, children)) =
        buttons.get_mut(remove.entity)
    else {
        return;
    };
    paint_button_root(
        ButtonPaintContext {
            entity: remove.entity,
            theme: &theme,
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
    let (_, _, want_text) = button_colours(&theme, button.variant, state);
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
        if let Ok((mut svg, mut retained)) = icons.get_mut(*child) {
            let token = foreground_token(button.variant, false);
            if retained.0 != token {
                retained.0 = token.clone();
            }
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

pub(crate) fn update_button_metrics(
    metrics: Res<CtkThemeMetrics>,
    typography: Res<CtkTypography>,
    mut buttons: Query<(Ref<CtkButton>, &mut Node, Option<&Children>)>,
    #[cfg(feature = "icons")] mut icons: ButtonIconNodes,
) {
    let resources_changed = metrics.is_changed() || typography.is_changed();
    for (button, mut node, children) in &mut buttons {
        let geometry_changed = resources_changed || button.is_changed();
        if geometry_changed {
            apply_button_metrics(&metrics, &button, &mut node);
        }

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
    theme: &'a UiTheme,
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
    let (_, _, colour) = button_colours(context.theme, context.button.variant, state);
    let font_size = label_font_size(context.typography, context.button.size);
    match &pending.0 {
        ButtonLabel::Text(_) => unreachable!("text labels spawn immediately"),
        ButtonLabel::Icon(icon, _) => {
            let icon = crate::icons::spawn_icon(
                commands,
                context.icons,
                context.theme,
                *icon,
                font_size,
                foreground_token(context.button.variant, context.disabled),
            );
            commands.entity(icon).insert(CtkButtonLabel(context.entity));
            commands.entity(context.entity).add_child(icon);
        }
        ButtonLabel::IconText(icon, text) => {
            let icon = crate::icons::spawn_icon(
                commands,
                context.icons,
                context.theme,
                *icon,
                font_size,
                foreground_token(context.button.variant, context.disabled),
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
    theme: Res<UiTheme>,
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
            theme: &theme,
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
    theme: Res<UiTheme>,
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
                theme: &theme,
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
fn foreground_token(variant: ButtonVariant, disabled: bool) -> bevy::feathers::theme::ThemeToken {
    if disabled {
        tokens::TEXT_DIM
    } else if variant == ButtonVariant::Primary {
        tokens::ROW_SELECTED_TEXT
    } else {
        tokens::TEXT
    }
}

#[cfg(feature = "icons")]
pub(crate) fn update_button_icon_style(
    theme: Res<UiTheme>,
    buttons: Query<(&CtkButton, Has<InteractionDisabled>, Option<&Children>)>,
    mut icons: Query<
        (
            &mut crate::icons::SvgColor,
            &mut crate::icons::ThemeSvgColor,
        ),
        With<CtkButtonLabel>,
    >,
) {
    for (button, disabled, children) in &buttons {
        let Some(children) = children else {
            continue;
        };
        let token = foreground_token(button.variant, disabled);
        let colour = ctk_color(&theme, &token);
        for child in children.iter() {
            let Ok((mut svg, mut retained)) = icons.get_mut(*child) else {
                continue;
            };
            if retained.0 != token {
                retained.0 = token.clone();
            }
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
    use bevy::input_focus::{FocusedInput, InputDispatchPlugin, InputFocus, InputFocusPlugin};
    use bevy::picking::backend::HitData;
    use bevy::picking::events::{Click, Pointer, Press};
    use bevy::picking::pointer::{Location, PointerButton, PointerId};
    use bevy::prelude::{App, EntityEvent, ResMut, Resource, Vec2};
    use bevy::window::{PrimaryWindow, Window, WindowRef};
    use core::time::Duration;

    use crate::style::{
        contrast_safe_lift, lighten, selected_background_from_pair, InteractionVisualState,
        DISABLED_LIFT, HOVERED_LIFT, PRESSED_LIFT,
    };
    use crate::theme::{
        apply_theme, contrast_ratio, Mode, RadiusScale, Scheme, ThemeSpec, ThemeState, AA_CONTRAST,
    };
    use crate::widgets::{ControlChange, CtkWidgetsPlugin};

    fn test_app(spec: &ThemeSpec) -> App {
        let mut theme = UiTheme::default();
        let mut state = ThemeState::default();
        apply_theme(&mut theme, &mut state, spec);
        let mut typography = CtkTypography::default();
        typography.body_px = spec.typography.body_px;

        let mut app = App::new();
        app.insert_resource(theme)
            .insert_resource(state)
            .insert_resource(typography)
            .insert_resource(spec.metrics.clone())
            .add_plugins(CtkWidgetsPlugin)
            // In production CtkThemePlugin registers this after the PostUpdate
            // typography pass; the harness has no theme plugin, so register it
            // bare to keep the font-reconciliation path under test.
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
            .find(|child| app.world().get::<Text>(*child).is_some())
            .expect("button has a text label")
    }

    fn set_hovered(app: &mut App, button: Entity, hovered: bool) {
        app.world_mut().entity_mut(button).remove::<Hovered>();
        app.world_mut().flush();
        app.world_mut().entity_mut(button).insert(Hovered(hovered));
        app.world_mut().flush();
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

    #[test]
    fn radius_scale_derives_from_base_and_clamps_small_radii() {
        assert_eq!(
            RadiusScale::from_base(6.0),
            RadiusScale {
                sm: 2.0,
                md: 4.0,
                lg: 6.0,
                xl: 10.0,
            }
        );
        assert_eq!(RadiusScale::from_base(2.0).sm, 0.0);
        assert_eq!(RadiusScale::from_base(2.0).md, 0.0);
    }

    #[test]
    fn spawn_contract_is_focusable_accessible_and_has_one_input_path() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let button = spawn(&mut app, ButtonDef::text("Save"));
        let entity = app.world().entity(button);

        assert!(entity.contains::<ActivateOnPress>());
        assert_eq!(entity.get::<TabIndex>(), Some(&TabIndex(0)));
        assert_eq!(entity.get::<Hovered>(), Some(&Hovered::default()));
        let accessible = entity.get::<AccessibilityNode>().unwrap();
        assert_eq!(accessible.role(), Role::Button);
        assert_eq!(accessible.label(), Some("Save"));
        assert!(!entity.contains::<bevy::ui_widgets::Button>());
        assert!(!entity.contains::<bevy::ui_widgets::Checkbox>());
        assert!(!entity.contains::<ActionButton>());
        assert!(!entity.contains::<BusWidget>());
    }

    #[test]
    fn added_observers_apply_live_paint_metrics_and_typography_before_update() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        {
            let mut metrics = app.world_mut().resource_mut::<CtkThemeMetrics>();
            metrics.button_height[ButtonSize::Md.index()] = 37.0;
            metrics.radius = RadiusScale::from_base(9.0);
        }
        app.world_mut().resource_mut::<CtkTypography>().body_px = 17.0;

        let button = spawn(&mut app, ButtonDef::text("Immediate"));
        let label = text_label(&app, button);
        let node = app.world().get::<Node>(button).unwrap();
        assert_eq!(node.height, px(37));
        assert_eq!(node.border_radius, BorderRadius::all(px(7)));
        assert_eq!(
            app.world().get::<BackgroundColor>(button).unwrap().0,
            spec.colors.control
        );
        assert_eq!(
            app.world().get::<BorderColor>(button).unwrap().top,
            spec.colors.border
        );
        assert_eq!(
            app.world().get::<TextColor>(label).unwrap().0,
            spec.colors.text
        );
        assert_eq!(
            app.world().get::<TextFont>(label).unwrap().font_size,
            FontSize::Px(17.0)
        );
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

    #[derive(Resource, Default)]
    struct LabelFontChangeCounts(Vec<usize>);

    #[test]
    fn settled_label_font_is_not_marked_changed_every_frame() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        app.init_resource::<LabelFontChangeCounts>();
        app.add_systems(
            bevy::app::Last,
            |changed: Query<(), (bevy::ecs::query::Changed<TextFont>, With<CtkButtonLabel>)>,
             mut counts: ResMut<LabelFontChangeCounts>| {
                counts.0.push(changed.iter().count());
            },
        );
        spawn(&mut app, ButtonDef::text("Settle"));
        app.update();
        app.update();
        app.update();
        let counts = &app.world().resource::<LabelFontChangeCounts>().0;
        assert_eq!(
            &counts[counts.len() - 2..],
            &[0, 0],
            "settled label fonts must not re-enter change detection: {counts:?}"
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn installed_icon_set_materialises_icon_text_during_spawn_flush() {
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
                .get::<crate::icons::ThemeSvgColor>(children[0])
                .unwrap()
                .0,
            tokens::ROW_SELECTED_TEXT
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn icon_only_button_requires_and_uses_an_explicit_accessibility_label() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        app.insert_resource(crate::icons::IconSet::placeholder_for_test(&[
            crate::icons::Icon::Info,
        ]));
        let button = spawn(
            &mut app,
            ButtonDef::icon(crate::icons::Icon::Info, "Show information"),
        );
        assert_eq!(
            app.world()
                .get::<AccessibilityNode>(button)
                .unwrap()
                .label(),
            Some("Show information")
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn disabled_variant_icon_uses_the_uniform_dim_foreground() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        app.insert_resource(crate::icons::IconSet::placeholder_for_test(&[
            crate::icons::Icon::Info,
        ]));
        let button = spawn(
            &mut app,
            ButtonDef::icon(crate::icons::Icon::Info, "Information")
                .variant(ButtonVariant::Primary)
                .disabled(),
        );
        app.update();
        let icon = app.world().get::<Children>(button).unwrap()[0];
        assert_eq!(
            app.world()
                .get::<crate::icons::ThemeSvgColor>(icon)
                .unwrap()
                .0,
            tokens::TEXT_DIM
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(button).unwrap().0,
            contrast_safe_lift(spec.colors.control, -DISABLED_LIFT, &[spec.colors.text_dim])
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn runtime_disable_and_enable_retint_icon_in_the_lifecycle_observers() {
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
        assert_eq!(
            app.world().get::<crate::icons::SvgColor>(icon).unwrap().0,
            spec.colors.row_selected_text
        );

        app.world_mut()
            .entity_mut(button)
            .insert(InteractionDisabled);
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .get::<crate::icons::ThemeSvgColor>(icon)
                .unwrap()
                .0,
            tokens::TEXT_DIM
        );
        assert_eq!(
            app.world().get::<crate::icons::SvgColor>(icon).unwrap().0,
            spec.colors.text_dim
        );

        app.world_mut()
            .entity_mut(button)
            .remove::<InteractionDisabled>();
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .get::<crate::icons::ThemeSvgColor>(icon)
                .unwrap()
                .0,
            tokens::ROW_SELECTED_TEXT
        );
        assert_eq!(
            app.world().get::<crate::icons::SvgColor>(icon).unwrap().0,
            spec.colors.row_selected_text
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn icon_text_hover_reconciles_its_icon_tint() {
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
        let icon = app.world().get::<Children>(button).unwrap()[0];
        app.world_mut()
            .get_mut::<crate::icons::SvgColor>(icon)
            .unwrap()
            .0 = Color::NONE;
        set_hovered(&mut app, button, true);
        app.update();

        assert_eq!(
            app.world().get::<crate::icons::SvgColor>(icon).unwrap().0,
            spec.colors.row_selected_text
        );
        assert_ne!(
            app.world().get::<BackgroundColor>(button).unwrap().0,
            spec.colors.row_selected
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
        let warned_after_first_pass = app
            .world()
            .resource::<ButtonDiagnostics>()
            .missing_icon_set_warned;
        app.update();
        assert_eq!(
            app.world()
                .resource::<ButtonDiagnostics>()
                .missing_icon_set_warned,
            warned_after_first_pass,
            "the missing-resource warning stays latched on later frames"
        );
    }

    #[cfg(feature = "icons")]
    #[test]
    fn live_size_and_typography_changes_resize_icon_and_icon_text_children() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        app.insert_resource(crate::icons::IconSet::placeholder_for_test(&[
            crate::icons::Icon::Info,
        ]));
        let buttons = [
            spawn(
                &mut app,
                ButtonDef::icon(crate::icons::Icon::Info, "Information").size(ButtonSize::Sm),
            ),
            spawn(
                &mut app,
                ButtonDef::icon_text(crate::icons::Icon::Info, "Details").size(ButtonSize::Sm),
            ),
        ];
        app.update();

        let icon_children = buttons.map(|button| {
            app.world()
                .get::<Children>(button)
                .unwrap()
                .iter()
                .copied()
                .find(|child| app.world().get::<crate::icons::UiSvg>(*child).is_some())
                .unwrap()
        });
        for icon in icon_children {
            let node = app.world().get::<Node>(icon).unwrap();
            assert_eq!(node.width, px(spec.typography.body_px - 2.0));
            assert_eq!(node.min_width, px(spec.typography.body_px - 2.0));
            assert_eq!(node.height, px(spec.typography.body_px - 2.0));
        }

        for button in buttons {
            app.world_mut().get_mut::<CtkButton>(button).unwrap().size = ButtonSize::Lg;
        }
        app.update();
        for icon in icon_children {
            let node = app.world().get::<Node>(icon).unwrap();
            assert_eq!(node.width, px(spec.typography.body_px));
            assert_eq!(node.min_width, px(spec.typography.body_px));
            assert_eq!(node.height, px(spec.typography.body_px));
        }

        app.world_mut().resource_mut::<CtkTypography>().body_px = 20.0;
        app.update();
        for icon in icon_children {
            let node = app.world().get::<Node>(icon).unwrap();
            assert_eq!(node.width, px(20));
            assert_eq!(node.min_width, px(20));
            assert_eq!(node.height, px(20));
        }
    }

    #[test]
    fn every_variant_uses_its_resting_theme_pairing() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let buttons = [
            (
                ButtonVariant::Default,
                spec.colors.control,
                spec.colors.border,
                spec.colors.text,
            ),
            (
                ButtonVariant::Primary,
                spec.colors.row_selected,
                spec.colors.border,
                spec.colors.row_selected_text,
            ),
            (
                ButtonVariant::Destructive,
                spec.colors.danger_surface,
                spec.colors.meter_red,
                spec.colors.text,
            ),
            (
                ButtonVariant::Ghost,
                Color::NONE,
                Color::NONE,
                spec.colors.text,
            ),
        ]
        .map(|(variant, background, border, text)| {
            (
                spawn(
                    &mut app,
                    ButtonDef::text(format!("{variant:?}")).variant(variant),
                ),
                background,
                border,
                text,
            )
        });
        app.update();

        for (button, background, border, text) in buttons {
            assert_eq!(
                app.world().get::<BackgroundColor>(button).unwrap().0,
                background
            );
            assert_eq!(app.world().get::<BorderColor>(button).unwrap().top, border);
            assert_eq!(
                app.world()
                    .get::<TextColor>(text_label(&app, button))
                    .unwrap()
                    .0,
                text
            );
        }
    }

    #[test]
    fn disabled_variants_use_the_uniform_neutral_pairing_and_ghost_stays_clear() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let neutral =
            contrast_safe_lift(spec.colors.control, -DISABLED_LIFT, &[spec.colors.text_dim]);
        for variant in [
            ButtonVariant::Default,
            ButtonVariant::Primary,
            ButtonVariant::Destructive,
            ButtonVariant::Ghost,
        ] {
            let button = spawn(
                &mut app,
                ButtonDef::text(format!("{variant:?}"))
                    .variant(variant)
                    .disabled(),
            );
            let ghost = variant == ButtonVariant::Ghost;
            assert_eq!(
                app.world().get::<BackgroundColor>(button).unwrap().0,
                if ghost { Color::NONE } else { neutral }
            );
            assert_eq!(
                app.world().get::<BorderColor>(button).unwrap().top,
                if ghost {
                    Color::NONE
                } else {
                    spec.colors.border
                }
            );
            assert_eq!(
                app.world()
                    .get::<TextColor>(text_label(&app, button))
                    .unwrap()
                    .0,
                spec.colors.text_dim
            );
        }
    }

    #[test]
    fn neutral_disabled_pairing_clears_aa_in_every_builtin_palette() {
        for scheme in Scheme::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let colors = ThemeSpec::from_scheme(scheme, mode).colors;
                let background =
                    contrast_safe_lift(colors.control, -DISABLED_LIFT, &[colors.text_dim]);
                let measured = contrast_ratio(colors.text_dim, background);
                assert!(
                    measured >= AA_CONTRAST,
                    "{scheme:?}/{mode:?} disabled text measures {measured:.3}:1"
                );
            }
        }
    }

    #[test]
    fn adversarial_disabled_pairing_clamps_before_losing_aa() {
        let mut spec = ThemeSpec::builtin();
        spec.colors.control = Color::srgb(118.0 / 255.0, 118.0 / 255.0, 118.0 / 255.0);
        spec.colors.text_dim = Color::BLACK;
        let raw = lighten(spec.colors.control, -DISABLED_LIFT);
        assert!(contrast_ratio(spec.colors.text_dim, spec.colors.control) >= AA_CONTRAST);
        assert!(contrast_ratio(spec.colors.text_dim, raw) < AA_CONTRAST);

        let mut theme = UiTheme::default();
        let mut state = ThemeState::default();
        apply_theme(&mut theme, &mut state, &spec);
        let (background, _, foreground) = button_colours(
            &theme,
            ButtonVariant::Default,
            InteractionVisualState::Disabled,
        );
        assert_eq!(foreground, spec.colors.text_dim);
        assert!(contrast_ratio(foreground, background) >= AA_CONTRAST);
        assert_ne!(background, raw);
    }

    #[test]
    fn focused_border_differs_from_resting_border_and_background_in_every_palette() {
        for scheme in Scheme::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let spec = ThemeSpec::from_scheme(scheme, mode);
                let mut theme = UiTheme::default();
                let mut state = ThemeState::default();
                apply_theme(&mut theme, &mut state, &spec);
                for variant in [
                    ButtonVariant::Default,
                    ButtonVariant::Primary,
                    ButtonVariant::Destructive,
                    ButtonVariant::Ghost,
                ] {
                    let (background, resting_border, _) =
                        button_colours(&theme, variant, InteractionVisualState::Resting);
                    let focused = focused_border(&theme, variant, resting_border, true);
                    assert_ne!(
                        focused, resting_border,
                        "{scheme:?}/{mode:?}/{variant:?} focus equals resting border"
                    );
                    assert_ne!(
                        focused, background,
                        "{scheme:?}/{mode:?}/{variant:?} focus equals background"
                    );
                    assert_eq!(
                        focused,
                        if variant == ButtonVariant::Primary {
                            spec.colors.row_selected_text
                        } else {
                            spec.colors.row_selected
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn focus_border_hides_when_input_focus_visible_is_false() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let button = spawn(&mut app, ButtonDef::text("Focus"));
        let resting = app.world().get::<BorderColor>(button).unwrap().top;
        *app.world_mut().resource_mut::<InputFocus>() = InputFocus::from_entity(button);
        app.world_mut().resource_mut::<InputFocusVisible>().0 = true;
        app.update();
        assert_ne!(app.world().get::<BorderColor>(button).unwrap().top, resting);

        app.world_mut().resource_mut::<InputFocusVisible>().0 = false;
        app.update();
        assert_eq!(app.world().get::<BorderColor>(button).unwrap().top, resting);
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
            1,
            "adding a second competing marker must not warn again for the entity"
        );
    }

    #[test]
    fn hover_and_press_lift_backgrounds_and_primary_uses_the_safe_pairing() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let default_button = spawn(&mut app, ButtonDef::text("Default"));
        let primary = spawn(
            &mut app,
            ButtonDef::text("Primary").variant(ButtonVariant::Primary),
        );
        app.update();

        set_hovered(&mut app, default_button, true);
        set_hovered(&mut app, primary, true);
        app.update();
        assert_eq!(
            app.world()
                .get::<BackgroundColor>(default_button)
                .unwrap()
                .0,
            lighten(spec.colors.control, HOVERED_LIFT)
        );
        let safe_hover = selected_background_from_pair(
            spec.colors.row_selected,
            spec.colors.row_selected_text,
            spec.colors.row_selected_text_dim,
            InteractionVisualState::Hovered,
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(primary).unwrap().0,
            safe_hover
        );
        assert_ne!(safe_hover, lighten(spec.colors.row_selected, HOVERED_LIFT));

        app.world_mut().entity_mut(default_button).insert(Pressed);
        app.world_mut().entity_mut(primary).insert(Pressed);
        app.update();
        assert_eq!(
            app.world()
                .get::<BackgroundColor>(default_button)
                .unwrap()
                .0,
            lighten(spec.colors.control, PRESSED_LIFT)
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(primary).unwrap().0,
            selected_background_from_pair(
                spec.colors.row_selected,
                spec.colors.row_selected_text,
                spec.colors.row_selected_text_dim,
                InteractionVisualState::Pressed,
            )
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
        let mut theme = UiTheme::default();
        let mut state = ThemeState::default();
        apply_theme(&mut theme, &mut state, spec);
        let mut typography = CtkTypography::default();
        typography.body_px = spec.typography.body_px;

        let mut app = App::new();
        app.insert_resource(theme)
            .insert_resource(state)
            .insert_resource(typography)
            .insert_resource(spec.metrics.clone())
            .init_resource::<bevy::ui::UiScale>()
            .add_plugins((
                InputPlugin,
                InputFocusPlugin,
                InputDispatchPlugin,
                CtkWidgetsPlugin,
            ))
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
    fn disabled_button_is_dimmed_and_swallows_pointer_and_focused_key_activation() {
        let spec = ThemeSpec::builtin();
        let (mut app, window) = activation_app(&spec);
        let button = spawn(
            &mut app,
            ButtonDef::text("Disabled").bus("test.disabled").disabled(),
        );
        app.world_mut()
            .insert_resource(InputFocus::from_entity(button));
        app.update();

        assert_eq!(
            app.world().get::<BackgroundColor>(button).unwrap().0,
            contrast_safe_lift(spec.colors.control, -DISABLED_LIFT, &[spec.colors.text_dim])
        );
        assert_eq!(
            app.world()
                .get::<TextColor>(text_label(&app, button))
                .unwrap()
                .0,
            spec.colors.text_dim
        );

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
    fn enabled_bus_button_publishes_once_for_a_pointer_press_click_sequence() {
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
    fn busless_button_exposes_one_activate_without_control_change() {
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
    fn enabled_focused_key_activation_publishes_exactly_once() {
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
    fn size_metrics_and_radius_are_canonical() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let buttons = [ButtonSize::Sm, ButtonSize::Md, ButtonSize::Lg]
            .map(|size| spawn(&mut app, ButtonDef::text(format!("{size:?}")).size(size)));
        app.update();

        for (button, height) in buttons.into_iter().zip([24.0, 28.0, 32.0]) {
            let node = app.world().get::<Node>(button).unwrap();
            assert_eq!(node.height, px(height));
            assert_eq!(
                node.border_radius,
                BorderRadius::all(px(spec.metrics.radius.md))
            );
        }
    }

    #[test]
    fn live_metrics_and_typography_rewrite_an_existing_button() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let button = spawn(&mut app, ButtonDef::text("Live"));
        app.update();

        {
            let mut metrics = app.world_mut().resource_mut::<CtkThemeMetrics>();
            metrics.button_height[ButtonSize::Md.index()] = 41.0;
            metrics.radius = RadiusScale::from_base(12.0);
        }
        app.world_mut().resource_mut::<CtkTypography>().body_px = 18.0;
        app.update();

        let node = app.world().get::<Node>(button).unwrap();
        assert_eq!(node.height, px(41));
        assert_eq!(node.border_radius, BorderRadius::all(px(10)));
        assert_eq!(
            app.world()
                .get::<TextFont>(text_label(&app, button))
                .unwrap()
                .font_size,
            FontSize::Px(18.0)
        );
    }

    #[test]
    fn ghost_is_transparent_only_at_rest() {
        let spec = ThemeSpec::builtin();
        let mut app = test_app(&spec);
        let ghost = spawn(
            &mut app,
            ButtonDef::text("Ghost").variant(ButtonVariant::Ghost),
        );
        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(ghost).unwrap().0,
            Color::NONE
        );

        set_hovered(&mut app, ghost, true);
        app.update();
        assert_ne!(
            app.world().get::<BackgroundColor>(ghost).unwrap().0,
            Color::NONE
        );
    }
}
