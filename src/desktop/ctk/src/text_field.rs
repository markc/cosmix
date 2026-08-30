//! Shared plain and secret single-line text fields.
//!
//! Secret-field trust boundary. `CtkSecretField` owns a zeroise-on-drop
//! [`SecretValue`], masks its rendered glyphs, strips clipboard copy/cut, and
//! redacts its a11y value — so the secret never reaches the wire (B-1: secrets
//! are never sent as Bus bodies, so `noded.tap` sees nothing), the display, the
//! clipboard, or the accessibility tree. It does NOT defend against a malicious
//! in-process Bevy system: the field is driven by a transparent Bevy
//! `EditableText` engine whose internal buffer holds the typed characters in
//! plaintext, and that buffer is Bevy-owned — it is not zeroised on drop, and
//! any system with `Query<&EditableText>` access can read it. That is the same
//! trust class as raw `KeyboardInput` events, which are likewise in-process
//! plaintext. Secret capture is in-process only by design; process isolation,
//! not this field, is the boundary against a hostile co-resident system.

use std::fmt;
use std::sync::Arc;

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::ecs::entity::Entities;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, ThemeToken, UiTheme};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::text::{EditableText, EditableTextSystems, TextCursorStyle, TextEdit};
use bevy::ui::widget::TextScroll;
use bevy::ui::Overflow;
use bevy::ui_widgets::SelectAllOnFocus;
use zeroize::Zeroize;

use crate::theme::tokens;

/// Theme-aware border paint for a text input.
///
/// The resting token is retained per field because different input sites use
/// different resting borders. The visible border may live on a wrapper (secret
/// fields), so `focus_target` can differ from the painted entity.
///
/// `BorderColor` is required: the painter queries it, so without it the field
/// would be silently skipped and never show a focus indicator.
#[derive(Component, Clone, Debug)]
#[require(BorderColor)]
pub struct CtkTextInputFocusBorder {
    resting: ThemeToken,
    focus_target: Option<Entity>,
}

impl CtkTextInputFocusBorder {
    /// Paint this entity from `resting`, using itself as the focus target.
    pub fn new(resting: ThemeToken) -> Self {
        Self {
            resting,
            focus_target: None,
        }
    }

    /// Paint this entity while tracking focus on a separate editable entity.
    pub fn for_target(resting: ThemeToken, focus_target: Entity) -> Self {
        Self {
            resting,
            focus_target: Some(focus_target),
        }
    }
}

/// Normalises a submitted value or returns an inline validation error.
type ValidatorFn = dyn Fn(&str) -> Result<String, String> + Send + Sync + 'static;

#[derive(Clone)]
pub struct TextValidator(Arc<ValidatorFn>);

impl TextValidator {
    pub fn new(validator: impl Fn(&str) -> Result<String, String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(validator))
    }

    pub fn validate(&self, value: &str) -> Result<String, String> {
        (self.0)(value)
    }
}

impl fmt::Debug for TextValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TextValidator(..)")
    }
}

/// Validate and trim a single file name.
pub fn validate_filename(value: &str) -> Result<String, String> {
    let name = value.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        Err("Enter a single valid file name".into())
    } else {
        Ok(name.into())
    }
}

/// Behaviour attached to the actual editable entity.
#[derive(Component, Clone, Debug)]
pub struct CtkTextField {
    validator: Option<TextValidator>,
}

impl CtkTextField {
    pub fn validate(&self, value: &str) -> Result<String, String> {
        self.validator.as_ref().map_or_else(
            || Ok(value.to_owned()),
            |validator| validator.validate(value),
        )
    }
}

/// Construction properties for a [`CtkTextField`].
#[derive(Clone, Debug)]
pub struct CtkTextFieldProps {
    pub initial: String,
    pub accessible_label: String,
    pub max_length: usize,
    pub select_all: bool,
    pub validator: Option<TextValidator>,
}

impl CtkTextFieldProps {
    pub fn new(initial: impl Into<String>, accessible_label: impl Into<String>) -> Self {
        Self {
            initial: initial.into(),
            accessible_label: accessible_label.into(),
            max_length: 4_096,
            select_all: false,
            validator: None,
        }
    }

    pub fn max_length(mut self, max_length: usize) -> Self {
        self.max_length = max_length;
        self
    }

    pub fn select_all(mut self, select_all: bool) -> Self {
        self.select_all = select_all;
        self
    }

    pub fn validator(mut self, validator: TextValidator) -> Self {
        self.validator = Some(validator);
        self
    }
}

/// The container, editable node and inline-error label of a text field.
#[derive(Clone, Copy, Debug)]
pub struct CtkTextFieldEntities {
    pub root: Entity,
    pub input: Entity,
    pub error: Entity,
}

pub fn spawn_text_field(commands: &mut Commands, props: CtkTextFieldProps) -> CtkTextFieldEntities {
    let mut editable = EditableText::new(&props.initial);
    editable.max_characters = Some(props.max_length);
    let mut input = commands.spawn((
        Node {
            width: percent(100),
            min_width: px(100),
            min_height: px(30),
            padding: UiRect::axes(px(7), px(4)),
            border: UiRect::all(px(1)),
            overflow: Overflow::clip(),
            ..default()
        },
        editable,
        TextLayout::no_wrap(),
        TextFont::from_font_size(13.0),
        ThemeTextColor(tokens::TEXT),
        TextCursorStyle::default(),
        TextScroll::default(),
        ThemeBackgroundColor(tokens::SURFACE),
        BorderColor::all(Color::NONE),
        TabIndex(0),
        text_accessibility(&props.accessible_label, false),
        CtkTextField {
            validator: props.validator,
        },
    ));
    if props.select_all {
        input.insert(SelectAllOnFocus);
    }
    let input = input.id();
    commands
        .entity(input)
        .insert(CtkTextInputFocusBorder::new(tokens::CONTROL));
    let error = commands
        .spawn((
            Text::new(""),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::METER_RED),
            Visibility::Hidden,
        ))
        .id();
    let root = commands
        .spawn(Node {
            width: percent(100),
            flex_grow: 1.0,
            min_width: px(100),
            flex_direction: FlexDirection::Column,
            row_gap: px(3),
            ..default()
        })
        .add_children(&[input, error])
        .id();
    CtkTextFieldEntities { root, input, error }
}

/// Update the field's inline error label.
pub fn set_text_field_error(
    error_entity: Entity,
    error: Option<&str>,
    texts: &mut Query<&mut Text>,
    visibility: &mut Query<&mut Visibility>,
) {
    if let Ok(mut text) = texts.get_mut(error_entity) {
        text.0 = error.unwrap_or_default().to_owned();
    }
    if let Ok(mut visible) = visibility.get_mut(error_entity) {
        *visible = if error.is_some() {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Secret storage. Its buffer is erased on drop and debug output is redacted.
///
/// It deliberately implements neither `Clone` nor serde traits.
#[derive(Default, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn into_inner(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([REDACTED])")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Secret-field state attached to a transparent `EditableText` engine.
#[derive(Component, Debug)]
pub struct CtkSecretField {
    value: SecretValue,
    mask: Entity,
    validator: Option<TextValidator>,
}

impl CtkSecretField {
    pub fn value(&self) -> &SecretValue {
        &self.value
    }

    pub fn take_value(&mut self) -> SecretValue {
        std::mem::take(&mut self.value)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.validator
            .as_ref()
            .map(|validator| validator.validate(self.value.expose_secret()).map(drop))
            .unwrap_or(Ok(()))
    }
}

/// Construction properties for a [`CtkSecretField`].
#[derive(Clone, Debug)]
pub struct CtkSecretFieldProps {
    pub initial: String,
    pub accessible_label: String,
    pub max_length: usize,
    pub validator: Option<TextValidator>,
}

impl CtkSecretFieldProps {
    pub fn new(initial: impl Into<String>, accessible_label: impl Into<String>) -> Self {
        Self {
            initial: initial.into(),
            accessible_label: accessible_label.into(),
            max_length: 4_096,
            validator: None,
        }
    }

    pub fn max_length(mut self, max_length: usize) -> Self {
        self.max_length = max_length;
        self
    }

    pub fn validator(mut self, validator: TextValidator) -> Self {
        self.validator = Some(validator);
        self
    }
}

pub fn spawn_secret_field(
    commands: &mut Commands,
    props: CtkSecretFieldProps,
) -> CtkTextFieldEntities {
    let mask = commands
        .spawn((
            Text::new("•".repeat(props.initial.chars().count())),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT),
            Pickable::IGNORE,
        ))
        .id();
    let mut editable = EditableText::new(&props.initial);
    editable.max_characters = Some(props.max_length);
    let input = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                top: px(0),
                bottom: px(0),
                padding: UiRect::axes(px(7), px(4)),
                overflow: Overflow::clip(),
                ..default()
            },
            editable,
            TextLayout::no_wrap(),
            TextFont::from_font_size(13.0),
            TextColor(Color::NONE),
            TextCursorStyle {
                color: Color::NONE,
                ..default()
            },
            TextScroll::default(),
            TabIndex(0),
            text_accessibility(&props.accessible_label, true),
            CtkSecretField {
                value: SecretValue::new(&props.initial),
                mask,
                validator: props.validator,
            },
        ))
        .id();
    let field = commands
        .spawn((
            Node {
                width: percent(100),
                min_width: px(100),
                min_height: px(30),
                padding: UiRect::axes(px(7), px(4)),
                border: UiRect::all(px(1)),
                align_items: AlignItems::Center,
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
            BorderColor::all(Color::NONE),
            CtkTextInputFocusBorder::for_target(tokens::CONTROL, input),
        ))
        .add_children(&[mask, input])
        .id();
    let error = commands
        .spawn((
            Text::new(""),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::METER_RED),
            Visibility::Hidden,
        ))
        .id();
    let root = commands
        .spawn(Node {
            width: percent(100),
            flex_grow: 1.0,
            min_width: px(100),
            flex_direction: FlexDirection::Column,
            row_gap: px(3),
            ..default()
        })
        .add_children(&[field, error])
        .id();
    CtkTextFieldEntities { root, input, error }
}

fn text_accessibility(label: &str, secret: bool) -> AccessibilityNode {
    let mut node = accesskit::Node::new(Role::TextInput);
    node.set_label(label);
    if secret {
        node.set_value("[REDACTED]");
    }
    AccessibilityNode::from(node)
}

/// Installs the secret-field edit guard and mask synchronisation.
pub struct CtkTextFieldPlugin;

impl Plugin for CtkTextFieldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            // `PostUpdate`, not `Update`. The painter has no ordering relationship
            // with `apply_theme_requests` or with whatever spawns and focuses a
            // modal's fields through deferred `Commands`, so in `Update` it could
            // repaint with the previous frame's theme, or miss a field that is not
            // query-visible yet and leave it transparent for a frame. Running after
            // the whole of `Update` gets both for free: every command is flushed and
            // the theme is this frame's. Nothing here affects layout — `BorderColor`
            // is read at render extract — so no `UiSystems` constraint is needed.
            //
            // Within `PostUpdate` there is one writer of `InputFocus` that must go
            // first: `sanitize_modal_focus` pulls focus off a field underneath an
            // open modal and onto the modal's default. Unordered, the painter can
            // light the underlying field and have the sanitiser move focus after,
            // extracting a stale border for that frame.
            .add_systems(
                PostUpdate,
                paint_text_input_focus_borders.after(crate::interaction::sanitize_modal_focus),
            )
            .add_systems(
                PostUpdate,
                strip_secret_clipboard_edits.before(EditableTextSystems),
            )
            .add_systems(PostUpdate, sync_secret_fields.after(EditableTextSystems));
    }
}

fn paint_text_input_focus_borders(
    focus: Res<InputFocus>,
    theme: Res<UiTheme>,
    entities: &Entities,
    mut borders: Query<(Entity, &CtkTextInputFocusBorder, &mut BorderColor)>,
) {
    for (entity, focus_border, mut border) in &mut borders {
        let focus_target = focus_border.focus_target.unwrap_or(entity);
        // A despawned target counts as unfocused. Bevy only clears a stale
        // `InputFocus` inside `dispatch_focused_input`, i.e. when the next input
        // event arrives — until then a wrapper whose `for_target` entity has died
        // would sit lit with no way for the user to put it out.
        let focused = focus.get() == Some(focus_target) && entities.contains(focus_target);
        let token = if focused {
            &tokens::CONTROL_ACTIVE
        } else {
            &focus_border.resting
        };
        let want = theme.color(token);
        // Compare every edge, not just the top: an external writer could leave the
        // other three stale, and a top-only check would skip the repaint.
        if *border != BorderColor::all(want) {
            *border = BorderColor::all(want);
        }
    }
}

fn strip_secret_clipboard_edits(mut fields: Query<&mut EditableText, With<CtkSecretField>>) {
    for mut editable in &mut fields {
        remove_secret_clipboard_edits(&mut editable);
    }
}

fn remove_secret_clipboard_edits(editable: &mut EditableText) {
    editable
        .pending_edits
        .retain(|edit| !matches!(edit, TextEdit::Copy | TextEdit::Cut));
}

fn sync_secret_fields(
    mut fields: Query<(&EditableText, &mut CtkSecretField, &mut AccessibilityNode)>,
    mut masks: Query<&mut Text>,
) {
    for (editable, mut field, mut accessibility) in &mut fields {
        let value = editable.value().to_string();
        if value != field.value.expose_secret() {
            field.value.0.zeroize();
            field.value.0 = value;
        }
        if let Ok(mut mask) = masks.get_mut(field.mask) {
            mask.0 = "•".repeat(field.value.expose_secret().chars().count());
        }
        accessibility.set_value("[REDACTED]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_input_focus_border_tracks_focus_resting_token_and_theme_changes() {
        use bevy::ecs::world::CommandQueue;
        use bevy::input_focus::FocusCause;

        let resting_control = Color::srgb(0.1, 0.2, 0.3);
        let resting_border = Color::srgb(0.2, 0.3, 0.4);
        let active = Color::srgb(0.8, 0.7, 0.2);
        let changed_border = Color::srgb(0.4, 0.5, 0.6);
        let mut app = App::new();
        app.add_plugins(CtkTextFieldPlugin);
        {
            let mut theme = app.world_mut().resource_mut::<UiTheme>();
            theme.set_color("ctk.control", resting_control);
            theme.set_color("ctk.border", resting_border);
            theme.set_color("ctk.control.active", active);
        }

        let mut queue = CommandQueue::default();
        let (field, secret) = {
            let mut commands = Commands::new(&mut queue, app.world());
            (
                spawn_text_field(&mut commands, CtkTextFieldProps::new("value", "Test field")),
                spawn_secret_field(
                    &mut commands,
                    CtkSecretFieldProps::new("secret", "Secret field"),
                ),
            )
        };
        queue.apply(app.world_mut());
        let secret_border = app
            .world()
            .get::<ChildOf>(secret.input)
            .expect("secret editable is inside its visible border")
            .parent();
        let path_like = app
            .world_mut()
            .spawn((
                BorderColor::all(Color::NONE),
                CtkTextInputFocusBorder::new(tokens::BORDER),
            ))
            .id();

        assert!(app
            .world()
            .get::<CtkTextInputFocusBorder>(field.input)
            .is_some());
        assert!(app
            .world()
            .get::<bevy::feathers::theme::ThemeBorderColor>(field.input)
            .is_none());
        assert!(app
            .world()
            .get::<CtkTextInputFocusBorder>(secret_border)
            .is_some());
        assert!(app
            .world()
            .get::<bevy::feathers::theme::ThemeBorderColor>(secret_border)
            .is_none());

        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(field.input, FocusCause::Navigated);
        app.update();
        assert_eq!(
            app.world().get::<BorderColor>(field.input).unwrap().top,
            active
        );
        assert_eq!(
            app.world().get::<BorderColor>(path_like).unwrap().top,
            resting_border
        );

        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(path_like, FocusCause::Navigated);
        app.update();
        assert_eq!(
            app.world().get::<BorderColor>(field.input).unwrap().top,
            resting_control
        );
        assert_eq!(
            app.world().get::<BorderColor>(path_like).unwrap().top,
            active
        );

        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(secret.input, FocusCause::Navigated);
        app.update();
        assert_eq!(
            app.world().get::<BorderColor>(secret_border).unwrap().top,
            active
        );

        app.world_mut().resource_mut::<InputFocus>().clear();
        app.world_mut()
            .resource_mut::<UiTheme>()
            .set_color("ctk.border", changed_border);
        app.update();
        assert_eq!(
            app.world().get::<BorderColor>(path_like).unwrap().top,
            changed_border
        );
    }

    #[test]
    fn a_focus_border_whose_target_died_goes_back_to_resting() {
        use bevy::ecs::world::CommandQueue;
        use bevy::input_focus::FocusCause;

        let resting_border = Color::srgb(0.2, 0.3, 0.4);
        let active = Color::srgb(0.8, 0.7, 0.2);
        let mut app = App::new();
        app.add_plugins(CtkTextFieldPlugin);
        {
            let mut theme = app.world_mut().resource_mut::<UiTheme>();
            theme.set_color("ctk.border", resting_border);
            theme.set_color("ctk.control.active", active);
        }

        // A wrapper painted from a separate target, as secret fields do.
        let mut queue = CommandQueue::default();
        let target = app.world_mut().spawn_empty().id();
        let wrapper = {
            let mut commands = Commands::new(&mut queue, app.world());
            commands
                .spawn(CtkTextInputFocusBorder::for_target(tokens::BORDER, target))
                .id()
        };
        queue.apply(app.world_mut());

        // `BorderColor` is required by the component, so the painter sees it even
        // though nothing inserted one.
        assert!(app.world().get::<BorderColor>(wrapper).is_some());

        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(target, FocusCause::Navigated);
        app.update();
        assert_eq!(app.world().get::<BorderColor>(wrapper).unwrap().top, active);

        // Bevy only clears a stale `InputFocus` when the next input event is
        // dispatched, so after this despawn the resource still names the dead
        // entity. The border must not stay lit on the strength of that.
        app.world_mut().entity_mut(target).despawn();
        app.update();
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(target),
            "the stale focus this guards against is still present"
        );
        assert_eq!(
            app.world().get::<BorderColor>(wrapper).unwrap().top,
            resting_border
        );
    }

    #[test]
    fn every_edge_of_a_focus_border_is_repainted_not_just_the_top() {
        use bevy::input_focus::FocusCause;

        let resting_border = Color::srgb(0.2, 0.3, 0.4);
        let active = Color::srgb(0.8, 0.7, 0.2);
        let mut app = App::new();
        app.add_plugins(CtkTextFieldPlugin);
        {
            let mut theme = app.world_mut().resource_mut::<UiTheme>();
            theme.set_color("ctk.border", resting_border);
            theme.set_color("ctk.control.active", active);
        }
        let field = app
            .world_mut()
            .spawn(CtkTextInputFocusBorder::new(tokens::BORDER))
            .id();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(field, FocusCause::Navigated);
        app.update();

        // An external writer leaves the top edge correct and the rest stale. A
        // top-only equality check would decide there was nothing to do.
        {
            let mut border = app.world_mut().get_mut::<BorderColor>(field).unwrap();
            border.left = resting_border;
            border.right = resting_border;
            border.bottom = resting_border;
        }
        app.update();
        let border = app.world().get::<BorderColor>(field).unwrap();
        assert_eq!(
            (border.top, border.left, border.right, border.bottom),
            (active, active, active, active)
        );
    }

    /// A modal spawns and focuses its field through deferred `Commands` during
    /// `Update`. The painter must still light it that frame — in `Update` it had
    /// no ordering against the spawner, so the field would render one frame with
    /// a transparent border.
    #[test]
    fn a_field_spawned_and_focused_by_a_deferred_command_is_lit_the_same_frame() {
        use bevy::input_focus::FocusCause;

        let active = Color::srgb(0.8, 0.7, 0.2);
        let mut app = App::new();
        app.add_plugins(CtkTextFieldPlugin);
        app.world_mut()
            .resource_mut::<UiTheme>()
            .set_color("ctk.control.active", active);
        app.add_systems(Update, |mut commands: Commands, mut done: Local<bool>| {
            if *done {
                return;
            }
            *done = true;
            commands.queue(|world: &mut World| {
                let field = world
                    .spawn(CtkTextInputFocusBorder::new(tokens::BORDER))
                    .id();
                world
                    .resource_mut::<InputFocus>()
                    .set(field, FocusCause::Navigated);
            });
        });

        app.update();

        let mut query = app.world_mut().query::<&BorderColor>();
        let borders = query.iter(app.world()).collect::<Vec<_>>();
        assert_eq!(borders.len(), 1, "the deferred spawn should have landed");
        assert_eq!(
            borders[0].top, active,
            "a field focused by a deferred command must be lit in that same frame"
        );
    }

    #[test]
    fn filename_validation_trims_and_rejects_path_components() {
        assert_eq!(validate_filename("  report.txt  ").unwrap(), "report.txt");
        for invalid in ["", " ", ".", "..", "a/b", "a\\b"] {
            assert!(validate_filename(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretValue::new("correct horse battery staple");
        let debug = format!("{secret:?}");
        assert!(!debug.contains("horse"));
        assert_eq!(debug, "SecretValue([REDACTED])");
    }

    #[test]
    fn secret_fields_discard_copy_and_cut_edits() {
        let mut editable = EditableText::new("secret");
        editable
            .pending_edits
            .extend([TextEdit::Copy, TextEdit::Cut, TextEdit::SelectAll]);
        remove_secret_clipboard_edits(&mut editable);
        assert!(!editable
            .pending_edits
            .iter()
            .any(|edit| matches!(edit, TextEdit::Copy | TextEdit::Cut)));
        assert!(editable.pending_edits.contains(&TextEdit::SelectAll));
    }
}
