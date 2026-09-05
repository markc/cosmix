//! Only accepted user state survives a launch; transient holds never reach disk.

use std::fmt::{Display, Formatter};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bevy::prelude::*;
use cosmix_config::{CosmixDir, Value, cosmix_path, parse_mix_data};
use cosmix_shell::core::{Edge, PanelConfig, PanelEffect, PanelInput, PanelMode, ShellModel};
use cosmix_shell::runtime::{ShellEffects, ShellFrameState};

#[derive(Clone, Debug, Default, PartialEq)]
struct EdgeState {
    thickness_px: Option<f32>,
    pinned: bool,
    page: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SavedState {
    edges: [EdgeState; 4],
    scheme: String,
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            edges: std::array::from_fn(|_| EdgeState::default()),
            scheme: "builtin".into(),
        }
    }
}

#[derive(Debug)]
enum StateError {
    Io(std::io::Error),
    Data(String),
}

impl Display for StateError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, f),
            Self::Data(error) => f.write_str(error),
        }
    }
}

impl std::error::Error for StateError {}
impl From<std::io::Error> for StateError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl SavedState {
    fn parse(source: &str) -> Result<Self, StateError> {
        let invalid = || StateError::Data("invalid Quoin state fields".into());
        let value = parse_mix_data(source).map_err(|error| StateError::Data(error.to_string()))?;
        let Value::Map(root) = &value else {
            return Err(invalid());
        };
        let Some(Value::String(scheme)) = root.get("scheme") else {
            return Err(invalid());
        };
        if root.len() != 5 {
            return Err(invalid());
        }
        let mut state = Self {
            scheme: scheme.clone(),
            ..Self::default()
        };
        for edge in Edge::ALL {
            let Some(Value::Map(fields)) = root.get(crate::edge_name(edge)) else {
                return Err(invalid());
            };
            let (
                Some(Value::Number(thickness)),
                Some(Value::Bool(pinned)),
                Some(Value::String(page)),
            ) = (
                fields.get("thickness_px"),
                fields.get("pinned"),
                fields.get("page"),
            )
            else {
                return Err(invalid());
            };
            if fields.len() != 3 {
                return Err(invalid());
            }
            let thickness = *thickness as f32;
            PanelConfig::new(
                thickness,
                Duration::from_millis(800),
                Duration::from_millis(200),
            )
            .map_err(|error| StateError::Data(error.to_string()))?;
            state.edges[edge.index()] = EdgeState {
                thickness_px: Some(thickness),
                pinned: *pinned,
                page: page.clone(),
            };
        }
        Ok(state)
    }

    /// Unknown page IDs retain the registry's default selection.
    pub(crate) fn restore(&self, model: &mut ShellModel) {
        for edge in Edge::ALL {
            let state = &self.edges[edge.index()];
            if let Some(thickness) = state.thickness_px {
                model
                    .restore_thickness(edge, thickness)
                    .expect("saved thickness was validated");
            }
            model.carousel_mut(edge).select_id(&state.page);
            if state.pinned {
                model
                    .panel_input(edge, model.last_update(), PanelInput::Pin)
                    .expect("restore uses model time");
            }
        }
    }

    fn encode(&self) -> Result<String, StateError> {
        let mut fields = vec![("scheme".to_owned(), Value::String(self.scheme.clone()))];
        for edge in Edge::ALL {
            let state = &self.edges[edge.index()];
            let thickness = state
                .thickness_px
                .ok_or_else(|| StateError::Data("state has no model dimensions".into()))?;
            fields.push((
                crate::edge_name(edge).into(),
                Value::map(
                    [
                        ("thickness_px".into(), Value::Number(f64::from(thickness))),
                        ("pinned".into(), Value::Bool(state.pinned)),
                        ("page".into(), Value::String(state.page.clone())),
                    ]
                    .into_iter()
                    .collect(),
                ),
            ));
        }
        Value::map(fields.into_iter().collect())
            .to_mix_data_string_pretty()
            .map_err(|error| StateError::Data(error.to_string()))
    }
}

#[derive(Resource)]
pub(crate) struct StateStore {
    path: Option<PathBuf>,
    saved: SavedState,
    #[cfg(test)]
    write_count: usize,
}

impl StateStore {
    pub(crate) fn startup(smoke: bool) -> Self {
        Self::load((!smoke).then(|| cosmix_path(CosmixDir::Var).join("quoin.state.mix")))
    }

    fn load(path: Option<PathBuf>) -> Self {
        let saved = match path.as_deref() {
            None => SavedState::default(),
            Some(path) => match std::fs::read_to_string(path)
                .map_err(StateError::Io)
                .and_then(|source| SavedState::parse(&source))
            {
                Ok(saved) => {
                    eprintln!("QUOIN_STATE restored=true");
                    saved
                }
                Err(error) => {
                    eprintln!("QUOIN_STATE restored=false reason={error}");
                    SavedState::default()
                }
            },
        };
        Self {
            path,
            saved,
            #[cfg(test)]
            write_count: 0,
        }
    }

    pub(crate) fn snapshot(&self) -> SavedState {
        self.saved.clone()
    }

    /// The restored appearance scheme name, or `None` when the state uses the
    /// sentinel default (`"builtin"`) — meaning no scheme was ever persisted
    /// and the caller should keep CTK's built-in selection.
    pub(crate) fn scheme(&self) -> Option<&str> {
        match self.saved.scheme.as_str() {
            "builtin" => None,
            scheme => Some(scheme),
        }
    }
}

/// A failed save leaves the previous complete file available for the next launch.
fn atomic_save(path: &Path, state: &SavedState) -> Result<(), StateError> {
    let source = state.encode()?;
    let parent = path
        .parent()
        .ok_or_else(|| StateError::Data("state path has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(source.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| StateError::Io(error.error))?;
    Ok(())
}

pub(crate) fn persist_transitions(
    effects: Res<ShellEffects>,
    frame: Res<ShellFrameState>,
    mut store: ResMut<StateStore>,
    mut schemes: MessageReader<cosmix_shell::chrome::QuoinSchemeSelected>,
    mut themes: MessageWriter<ctk::theme::ApplyTheme>,
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
) {
    let selection = schemes.read().last().map(|selection| selection.0);
    if let Some(scheme) = selection {
        let mut spec = ctk::theme::ThemeSpec::from_scheme(scheme, ctk::theme::Mode::Dark);
        if let Some(font) = crate::desktop_font::detect() {
            spec.typography = ctk::theme::TypographySpec {
                family: font.family,
                body_px: font.body_px,
            };
        }
        themes.write(ctk::theme::ApplyTheme(spec));
        // CTK may already have consumed requests in this Update pass.
        redraw.write(bevy::window::RequestRedraw);
    }
    if store.path.is_none()
        || (selection.is_none()
            && !effects.0.iter().any(|effect| {
                matches!(
                    effect.effect,
                    PanelEffect::Pin { .. } | PanelEffect::ResizeCompleted
                )
            })
            && effects.1.is_empty())
    {
        return;
    }
    if let Some(scheme) = selection {
        store.saved.scheme = scheme.name().to_owned();
    }
    for edge in Edge::ALL {
        let panel = frame.0.panel(edge);
        store.saved.edges[edge.index()] = EdgeState {
            thickness_px: Some(panel.settled_thickness_px),
            pinned: panel.mode == PanelMode::Pinned,
            page: panel.active_page_id.clone().unwrap_or_default(),
        };
    }
    if let Some(path) = &store.path {
        match atomic_save(path, &store.saved) {
            Ok(()) => {
                #[cfg(test)]
                {
                    store.write_count += 1;
                }
            }
            Err(error) => bevy::log::warn!("Quoin state save failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, OutputKey};
    use cosmix_shell::runtime::{
        CarouselInput, ShellCommand, ShellCommandKind, ShellRuntimePlugin, ShellRuntimeSet,
    };

    fn resize_app(path: &Path) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model())))
            .add_message::<cosmix_shell::chrome::QuoinSchemeSelected>()
            .add_message::<ctk::theme::ApplyTheme>()
            .add_message::<bevy::window::RequestRedraw>()
            .insert_resource(StateStore::load(Some(path.to_owned())))
            .add_systems(Update, persist_transitions.in_set(ShellRuntimeSet::Host));
        app
    }

    fn resize_command(app: &mut App, kind: ShellCommandKind) {
        app.world_mut().write_message(ShellCommand {
            output: OutputKey::new("DP-1").unwrap(),
            at: Duration::ZERO,
            kind,
        });
        app.update();
    }

    fn resize_input(app: &mut App, input: PanelInput) {
        resize_command(
            app,
            ShellCommandKind::Panel {
                edge: Edge::Left,
                input,
            },
        );
    }

    #[test]
    fn resize_gesture_writes_once_on_completion_not_motion_or_duplicate_release() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut app = resize_app(&path);
        resize_input(&mut app, PanelInput::ResizeStarted);
        for thickness_px in [150.0, 200.0, 300.0] {
            resize_command(
                &mut app,
                ShellCommandKind::Resize {
                    edge: Edge::Left,
                    thickness_px,
                },
            );
            assert_eq!(app.world().resource::<StateStore>().write_count, 0);
            assert!(!path.exists());
        }
        resize_input(&mut app, PanelInput::ResizeCompleted);
        assert_eq!(app.world().resource::<StateStore>().write_count, 1);
        assert_eq!(
            StateStore::load(Some(path)).snapshot().edges[0].thickness_px,
            Some(300.0)
        );
        resize_input(&mut app, PanelInput::ResizeCompleted);
        app.update();
        assert_eq!(app.world().resource::<StateStore>().write_count, 1);
    }

    #[test]
    fn cancelled_resize_never_saves_and_other_transitions_keep_starting_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut app = resize_app(&path);
        let starting = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left)
            .thickness_px;
        resize_input(&mut app, PanelInput::ResizeStarted);
        resize_command(
            &mut app,
            ShellCommandKind::Resize {
                edge: Edge::Left,
                thickness_px: 333.0,
            },
        );
        resize_input(&mut app, PanelInput::ResizeCancelled);
        resize_input(&mut app, PanelInput::ResizeCompleted);
        assert!(!path.exists());
        assert_eq!(app.world().resource::<StateStore>().write_count, 0);
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .thickness_px,
            starting
        );

        resize_input(&mut app, PanelInput::ResizeStarted);
        resize_command(
            &mut app,
            ShellCommandKind::Resize {
                edge: Edge::Left,
                thickness_px: 333.0,
            },
        );
        resize_input(&mut app, PanelInput::Pin);
        assert_eq!(
            StateStore::load(Some(path.clone())).snapshot().edges[0].thickness_px,
            Some(starting)
        );
        resize_input(&mut app, PanelInput::ResizeCancelled);
        assert_eq!(app.world().resource::<StateStore>().write_count, 1);
        assert_eq!(
            StateStore::load(Some(path)).snapshot().edges[0].thickness_px,
            Some(starting)
        );
    }

    fn model() -> ShellModel {
        let mut model = ShellModel::new(
            OutputKey::new("DP-1").unwrap(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let registry = crate::page_registry();
        for edge in Edge::ALL {
            model.set_carousel(edge, registry.carousel(edge));
        }
        model
    }

    fn populated() -> SavedState {
        let model = model();
        let mut state = SavedState {
            scheme: "custom \"blue\" ${literal}\n".into(),
            ..SavedState::default()
        };
        for edge in Edge::ALL {
            state.edges[edge.index()] = EdgeState {
                thickness_px: Some(model.panel(edge).thickness_px + 13.0),
                pinned: edge == Edge::Left,
                page: model.carousel(edge).page_ids()[1].clone(),
            };
        }
        state
    }

    #[test]
    fn scheme_dot_applies_dark_theme_and_persists_for_restart() {
        use cosmix_shell::chrome::{QuoinChromePlugin, scheme_dot};
        use ctk::theme::{ApplyTheme, Mode, Scheme};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut model = model();
        model
            .panel_input(Edge::Right, Duration::ZERO, PanelInput::Pin)
            .unwrap();
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            ShellRuntimePlugin::new(model),
            QuoinChromePlugin,
        ))
        .init_resource::<ButtonInput<KeyCode>>()
        .add_message::<ApplyTheme>()
        .add_message::<bevy::window::RequestRedraw>()
        .insert_resource(StateStore::load(Some(path.clone())))
        .add_systems(Update, persist_transitions.in_set(ShellRuntimeSet::Host));
        for scheme in Scheme::ALL {
            let mut queue = bevy::ecs::world::CommandQueue::default();
            let dot = scheme_dot(
                &mut Commands::new(&mut queue, app.world()),
                Edge::Right,
                "monitor",
                scheme,
            )
            .unwrap();
            queue.apply(app.world_mut());
            assert_eq!(
                app.world().get::<BackgroundColor>(dot).unwrap().0,
                ctk::theme::ThemeSpec::from_scheme(scheme, Mode::Dark)
                    .colors
                    .control_active
            );
            assert!(
                app.world()
                    .get::<bevy::feathers::theme::ThemeBackgroundColor>(dot)
                    .is_none()
            );
            // Newly spawned controls remain disabled until presentation admits them.
            app.world_mut()
                .trigger(bevy::ui_widgets::Activate { entity: dot });
            app.update();
            assert!(app.world().resource::<Messages<ApplyTheme>>().is_empty());
            app.world_mut()
                .trigger(bevy::ui_widgets::Activate { entity: dot });
            app.update();
            let applied = app
                .world_mut()
                .resource_mut::<Messages<ApplyTheme>>()
                .drain()
                .last()
                .unwrap();
            assert_eq!(applied.0.scheme, scheme);
            assert_eq!(applied.0.mode, Mode::Dark);
            assert_eq!(
                StateStore::load(Some(path.clone())).scheme(),
                Some(scheme.name())
            );
            app.world_mut().entity_mut(dot).despawn();
        }
    }

    #[test]
    fn state_round_trip_restores_sizes_pins_pages_and_scheme() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let saved = populated();
        atomic_save(&path, &saved).unwrap();
        let loaded = StateStore::load(Some(path)).snapshot();
        assert_eq!(loaded, saved);
        let mut model = model();
        loaded.restore(&mut model);
        for edge in Edge::ALL {
            assert_eq!(
                model.panel(edge).thickness_px,
                saved.edges[edge.index()].thickness_px.unwrap()
            );
            assert_eq!(
                model.panel(edge).mode == PanelMode::Pinned,
                edge == Edge::Left
            );
            assert_eq!(
                model.carousel(edge).active_id(),
                Some(saved.edges[edge.index()].page.as_str())
            );
        }
    }

    #[test]
    fn corrupt_and_missing_files_fall_back_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        assert_eq!(
            StateStore::load(Some(path.clone())).snapshot(),
            SavedState::default()
        );
        assert!(!path.exists());
        for source in ["{broken", "{scheme: run(\"anything\")}", "{}"] {
            std::fs::write(&path, source).unwrap();
            assert_eq!(
                StateStore::load(Some(path.clone())).snapshot(),
                SavedState::default()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        }
        let mut invalid = populated();
        invalid.edges[0].thickness_px = Some(-1.0);
        assert!(SavedState::parse(&invalid.encode().unwrap()).is_err());
    }

    #[test]
    fn unknown_saved_page_uses_registry_default() {
        let mut state = populated();
        state.edges[0].page = "removed-page".into();
        let mut model = model();
        state.restore(&mut model);
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("nav"));
    }

    #[test]
    fn smoke_skips_restore_and_all_writes() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model())))
            .add_message::<cosmix_shell::chrome::QuoinSchemeSelected>()
            .add_message::<ctk::theme::ApplyTheme>()
            .add_message::<bevy::window::RequestRedraw>()
            .insert_resource(StateStore::startup(true))
            .add_systems(Update, persist_transitions.in_set(ShellRuntimeSet::Host));
        app.world_mut().write_message(ShellCommand {
            output: OutputKey::new("DP-1").unwrap(),
            at: Duration::ZERO,
            kind: ShellCommandKind::Panel {
                edge: Edge::Left,
                input: PanelInput::Pin,
            },
        });
        app.world_mut()
            .write_message(cosmix_shell::chrome::QuoinSchemeSelected(
                ctk::theme::Scheme::Forest,
            ));
        app.update();
        let store = app.world().resource::<StateStore>();
        assert!(store.path.is_none());
        assert_eq!(store.saved, SavedState::default());
    }

    #[test]
    fn accepted_pin_and_page_save_after_model_but_rejected_page_does_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model())))
            .add_message::<cosmix_shell::chrome::QuoinSchemeSelected>()
            .add_message::<ctk::theme::ApplyTheme>()
            .add_message::<bevy::window::RequestRedraw>()
            .insert_resource(StateStore::load(Some(path.clone())))
            .add_systems(Update, persist_transitions.in_set(ShellRuntimeSet::Host));
        let command = |kind| ShellCommand {
            output: OutputKey::new("DP-1").unwrap(),
            at: Duration::ZERO,
            kind,
        };
        app.world_mut()
            .write_message(command(ShellCommandKind::Carousel {
                edge: Edge::Left,
                input: CarouselInput::SelectId("unknown".into()),
            }));
        app.update();
        assert!(!path.exists());
        app.world_mut()
            .write_message(command(ShellCommandKind::Panel {
                edge: Edge::Left,
                input: PanelInput::Pin,
            }));
        app.world_mut()
            .write_message(command(ShellCommandKind::Carousel {
                edge: Edge::Left,
                input: CarouselInput::Next,
            }));
        app.update();
        let saved = StateStore::load(Some(path)).snapshot();
        assert!(saved.edges[0].pinned);
        assert_eq!(saved.edges[0].page, "places");
    }
}
