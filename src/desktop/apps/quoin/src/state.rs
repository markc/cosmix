//! Only accepted user state survives a launch; transient holds never reach disk.
//!
//! The v3 format scopes every edge set to its output's persistent identity
//! (shell design §7): the root holds `version: 3`, `scheme`, and an `outputs`
//! map from identity string to the four edge fields. v1 and v2 files keyed
//! edges by the implicit single output; they migrate to the reserved
//! [`DEFAULT_OUTPUT`] entry, which the first output to restore claims.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bevy::prelude::*;
use cosmix_config::{CosmixDir, Value, cosmix_path, parse_mix_data};
use cosmix_shell::core::{Edge, OutputKey, PanelConfig, PanelEffect, PanelMode, ShellModel};
use cosmix_shell::runtime::{ShellEffects, ShellFrameState};

/// Reserved identity under which a v1/v2 file's single edge set migrates:
/// it belonged to the output Quoin ran on, so the first output to restore
/// claims it (a later unknown output gets the default config instead).
const DEFAULT_OUTPUT: &str = "default";

/// Persistent identity of one output for state keying (shell design §7):
/// EDID make/model/serial when the compositor reports it, else the connector
/// name, else a new output with the default config. comp's output
/// observations currently carry only the connector name — the `outputs`
/// props row name the layer host's `OutputKey` mirrors — so identities are
/// `connector:<name>` today. File keys are opaque non-empty strings, so an
/// `edid:` tier can be introduced when comp grows EDID fields without
/// another format version. Real identities are always prefixed, which keeps
/// them clear of [`DEFAULT_OUTPUT`].
fn output_identity(output: &OutputKey) -> String {
    format!("connector:{}", output.as_str())
}

#[derive(Clone, Debug, Default, PartialEq)]
struct EdgeState {
    thickness_px: Option<f32>,
    mode: PanelMode,
    page: String,
}

/// One output's remembered per-edge state.
#[derive(Clone, Debug, PartialEq)]
struct OutputState {
    edges: [EdgeState; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SavedState {
    outputs: BTreeMap<String, OutputState>,
    scheme: String,
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            outputs: BTreeMap::new(),
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
        let mut state = Self {
            scheme: scheme.clone(),
            ..Self::default()
        };
        match root.get("version") {
            // v1 (no version) and v2 predate per-output keying: their single
            // edge set belongs to the output Quoin ran on and migrates to the
            // reserved default-output entry, claimed at restore time.
            None => {
                if root.len() != 5 {
                    return Err(invalid());
                }
                let edges = parse_edge_set(&value, true)?;
                state.outputs.insert(DEFAULT_OUTPUT.to_owned(), OutputState { edges });
            }
            Some(Value::Number(version)) if *version == 2.0 => {
                if root.len() != 6 {
                    return Err(invalid());
                }
                let edges = parse_edge_set(&value, false)?;
                state.outputs.insert(DEFAULT_OUTPUT.to_owned(), OutputState { edges });
            }
            Some(Value::Number(version)) if *version == 3.0 => {
                if root.len() != 3 {
                    return Err(invalid());
                }
                let Some(Value::Map(outputs)) = root.get("outputs") else {
                    return Err(invalid());
                };
                for (identity, output) in outputs.iter() {
                    if identity.trim().is_empty() {
                        return Err(invalid());
                    }
                    let Value::Map(fields) = output else {
                        return Err(invalid());
                    };
                    if fields.len() != Edge::ALL.len() {
                        return Err(invalid());
                    }
                    let edges = parse_edge_set(output, false)?;
                    state.outputs.insert(identity.clone(), OutputState { edges });
                }
            }
            _ => return Err(invalid()),
        }
        Ok(state)
    }

    /// Unknown page IDs retain the registry's default selection.
    ///
    /// Restoring claims the entry for the model's output: a match by
    /// identity is reused as-is, while the migrated default-output entry
    /// moves to the first output that restores it. An unknown output
    /// restores nothing — it keeps the default config and gains no entry
    /// until its first save.
    pub(crate) fn restore(&mut self, model: &mut ShellModel) {
        let identity = output_identity(model.output());
        let Some(state) = self
            .outputs
            .remove(&identity)
            .or_else(|| self.outputs.remove(DEFAULT_OUTPUT))
        else {
            return;
        };
        for edge in Edge::ALL {
            let saved = &state.edges[edge.index()];
            if let Some(thickness) = saved.thickness_px {
                model
                    .restore_thickness(edge, thickness)
                    .expect("saved thickness was validated");
            }
            model.carousel_mut(edge).select_id(&saved.page);
            model
                .set_mode(edge, model.last_update(), saved.mode)
                .expect("restore uses model time");
        }
        self.outputs.insert(identity, state);
    }

    fn encode(&self) -> Result<String, StateError> {
        let mut output_fields = Vec::with_capacity(self.outputs.len());
        for (identity, output) in &self.outputs {
            let mut edge_fields = Vec::with_capacity(Edge::ALL.len());
            for edge in Edge::ALL {
                let state = &output.edges[edge.index()];
                let thickness = state.thickness_px.ok_or_else(|| {
                    StateError::Data("state has no model dimensions".into())
                })?;
                edge_fields.push((
                    crate::edge_name(edge).into(),
                    Value::map(
                        [
                            ("thickness_px".into(), Value::Number(f64::from(thickness))),
                            ("mode".into(), Value::String(state.mode.as_str().into())),
                            ("page".into(), Value::String(state.page.clone())),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                ));
            }
            output_fields.push((identity.clone(), Value::map(edge_fields.into_iter().collect())));
        }
        Value::map(
            [
                ("version".to_owned(), Value::Number(3.0)),
                ("scheme".to_owned(), Value::String(self.scheme.clone())),
                ("outputs".to_owned(), Value::map(output_fields.into_iter().collect())),
            ]
            .into_iter()
            .collect(),
        )
        .to_mix_data_string_pretty()
        .map_err(|error| StateError::Data(error.to_string()))
    }
}

/// Parse the four per-edge entries from a map-shaped value. For v1/v2 the
/// value is the file root (whose extra keys were count-checked by the
/// caller); for v3 it is one output's entry, already length-checked.
fn parse_edge_set(edges: &Value, legacy: bool) -> Result<[EdgeState; 4], StateError> {
    let invalid = || StateError::Data("invalid Quoin state fields".into());
    let Value::Map(fields) = edges else {
        return Err(invalid());
    };
    let mut set = std::array::from_fn(|_| EdgeState::default());
    for edge in Edge::ALL {
        let Some(Value::Map(entry)) = fields.get(crate::edge_name(edge)) else {
            return Err(invalid());
        };
        let (Some(Value::Number(thickness)), Some(Value::String(page))) =
            (entry.get("thickness_px"), entry.get("page"))
        else {
            return Err(invalid());
        };
        if entry.len() != 3 {
            return Err(invalid());
        }
        let mode = if legacy {
            match entry.get("pinned") {
                Some(Value::Bool(true)) => PanelMode::Docked,
                Some(Value::Bool(false)) => PanelMode::Hidden,
                _ => return Err(invalid()),
            }
        } else {
            match entry.get("mode") {
                Some(Value::String(mode)) => match mode.as_str() {
                    "hidden" => PanelMode::Hidden,
                    "pinned" => PanelMode::Pinned,
                    "docked" => PanelMode::Docked,
                    _ => return Err(invalid()),
                },
                _ => return Err(invalid()),
            }
        };
        let thickness = *thickness as f32;
        PanelConfig::new(
            thickness,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .map_err(|error| StateError::Data(error.to_string()))?;
        set[edge.index()] = EdgeState {
            thickness_px: Some(thickness),
            mode,
            page: page.clone(),
        };
    }
    Ok(set)
}

#[derive(Resource)]
pub(crate) struct StateStore {
    path: Option<PathBuf>,
    /// Shared with the host's model factory: restoring claims the migrated
    /// default-output entry, and the claim must reach the next save.
    saved: Arc<Mutex<SavedState>>,
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
            saved: Arc::new(Mutex::new(saved)),
            #[cfg(test)]
            write_count: 0,
        }
    }

    /// The saved state as it stands; used by tests to inspect the store.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> SavedState {
        self.lock_saved().clone()
    }

    /// A handle sharing the saved state with a host's model factory, which
    /// builds models outside the Bevy world the store lives in.
    pub(crate) fn shared_saved(&self) -> Arc<Mutex<SavedState>> {
        Arc::clone(&self.saved)
    }

    /// Restore remembered state into `model`, claiming the model output's
    /// identity (see [`SavedState::restore`]).
    pub(crate) fn restore(&self, model: &mut ShellModel) {
        self.lock_saved().restore(model);
    }

    /// Restore through a factory-held shared handle (see
    /// [`Self::shared_saved`]), so the factory's claim is visible to the
    /// next save.
    pub(crate) fn restore_shared(shared: &Arc<Mutex<SavedState>>, model: &mut ShellModel) {
        shared.lock().expect("Quoin state lock").restore(model);
    }

    /// The restored appearance scheme name, or `None` when the state uses the
    /// sentinel default (`"builtin"`) — meaning no scheme was ever persisted
    /// and the caller should keep CTK's built-in selection.
    pub(crate) fn scheme(&self) -> Option<String> {
        match self.lock_saved().scheme.as_str() {
            "builtin" => None,
            scheme => Some(scheme.to_owned()),
        }
    }

    fn lock_saved(&self) -> MutexGuard<'_, SavedState> {
        self.saved.lock().expect("Quoin state lock")
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
                    PanelEffect::ModeChanged { .. } | PanelEffect::ResizeCompleted
                )
            })
            && effects.1.is_empty())
    {
        return;
    }
    let mut saved = store.lock_saved();
    if let Some(scheme) = selection {
        saved.scheme = scheme.name().to_owned();
    }
    // Only the current output's entry is rewritten; other outputs' remembered
    // state stays for reconnection (shell design §7's output-removal row).
    let output = output_identity(&frame.0.geometry.output);
    saved.outputs.insert(
        output,
        OutputState {
            edges: std::array::from_fn(|index| {
                let panel = frame.0.panel(Edge::ALL[index]);
                EdgeState {
                    thickness_px: Some(panel.settled_thickness_px),
                    mode: panel.mode,
                    page: panel.active_page_id.clone().unwrap_or_default(),
                }
            }),
        },
    );
    let saved_result = match store.path.as_deref() {
        Some(path) => atomic_save(path, &saved),
        None => Ok(()),
    };
    drop(saved);
    match saved_result {
        Ok(()) => {
            #[cfg(test)]
            {
                store.write_count += 1;
            }
        }
        Err(error) => bevy::log::warn!("Quoin state save failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, OutputKey, PanelInput};
    use cosmix_shell::runtime::{
        CarouselInput, ShellCommand, ShellCommandKind, ShellRuntimePlugin, ShellRuntimeSet,
    };

    fn resize_app(path: &Path) -> App {
        resize_app_for(path, "DP-1")
    }

    fn resize_app_for(path: &Path, output: &str) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model_for(output))))
            .add_message::<cosmix_shell::chrome::QuoinSchemeSelected>()
            .add_message::<ctk::theme::ApplyTheme>()
            .add_message::<bevy::window::RequestRedraw>()
            .insert_resource(StateStore::load(Some(path.to_owned())))
            .add_systems(Update, persist_transitions.in_set(ShellRuntimeSet::Host));
        app
    }

    fn resize_command(app: &mut App, kind: ShellCommandKind) {
        resize_command_for(app, "DP-1", kind);
    }

    fn resize_command_for(app: &mut App, output: &str, kind: ShellCommandKind) {
        app.world_mut().write_message(ShellCommand {
            output: OutputKey::new(output).unwrap(),
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
        let saved = StateStore::load(Some(path)).snapshot();
        assert_eq!(saved.outputs["connector:DP-1"].edges[0].thickness_px, Some(300.0));
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
        resize_input(&mut app, PanelInput::Dock);
        let saved = StateStore::load(Some(path.clone())).snapshot();
        assert_eq!(saved.outputs["connector:DP-1"].edges[0].thickness_px, Some(starting));
        resize_input(&mut app, PanelInput::ResizeCancelled);
        assert_eq!(app.world().resource::<StateStore>().write_count, 1);
        let saved = StateStore::load(Some(path)).snapshot();
        assert_eq!(saved.outputs["connector:DP-1"].edges[0].thickness_px, Some(starting));
    }

    fn model() -> ShellModel {
        model_for("DP-1")
    }

    fn model_for(output: &str) -> ShellModel {
        let mut model = ShellModel::new(
            OutputKey::new(output).unwrap(),
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

    /// Distinct remembered state for one output, addressable by name.
    fn output_state(output: &str, thickness_px: f32, mode: PanelMode, page: usize) -> OutputState {
        let model = model_for(output);
        OutputState {
            edges: std::array::from_fn(|index| {
                let edge = Edge::ALL[index];
                let pages = model.carousel(edge).page_ids();
                EdgeState {
                    thickness_px: Some(thickness_px + index as f32),
                    mode: if edge == Edge::Left { mode } else { PanelMode::Hidden },
                    page: pages[page.min(pages.len() - 1)].clone(),
                }
            }),
        }
    }

    fn populated() -> SavedState {
        let model = model();
        let mut state = SavedState {
            scheme: "custom \"blue\" ${literal}\n".into(),
            ..SavedState::default()
        };
        let mut edges = std::array::from_fn(|_| EdgeState::default());
        for edge in Edge::ALL {
            edges[edge.index()] = EdgeState {
                thickness_px: Some(model.panel(edge).thickness_px + 13.0),
                mode: if edge == Edge::Left {
                    PanelMode::Docked
                } else {
                    PanelMode::Hidden
                },
                page: model.carousel(edge).page_ids()[1].clone(),
            };
        }
        let identity = output_identity(&OutputKey::new("DP-1").unwrap());
        state.outputs.insert(identity, OutputState { edges });
        state
    }

    fn legacy_source(mask: u8) -> String {
        let mut source = String::from("{scheme: \"legacy\"");
        for edge in Edge::ALL {
            source.push_str(&format!(
                ", {}: {{thickness_px: {}, pinned: {}, page: \"{}\"}}",
                crate::edge_name(edge),
                140 + edge.index(),
                mask & (1 << edge.index()) != 0,
                "removed-page"
            ));
        }
        source.push('}');
        source
    }

    /// A v2 file as today's Quoin writes it: string modes, no output nesting.
    fn v2_source() -> String {
        let mut source = String::from("{version: 2, scheme: \"v2\"");
        for edge in Edge::ALL {
            source.push_str(&format!(
                ", {}: {{thickness_px: {}, mode: \"{}\", page: \"{}\"}}",
                crate::edge_name(edge),
                150 + edge.index(),
                if edge == Edge::Left { "docked" } else { "hidden" },
                "places"
            ));
        }
        source.push('}');
        source
    }

    #[test]
    fn every_legacy_pin_combination_migrates_preserving_sizes_pages_and_scheme() {
        for mask in 0..16 {
            let source = legacy_source(mask);
            let mut saved = SavedState::parse(&source).unwrap();
            assert_eq!(saved.scheme, "legacy");
            let mut model = model();
            saved.restore(&mut model);
            for edge in Edge::ALL {
                let expected = if mask & (1 << edge.index()) != 0 {
                    PanelMode::Docked
                } else {
                    PanelMode::Hidden
                };
                let claimed = &saved.outputs["connector:DP-1"].edges[edge.index()];
                assert_eq!(claimed.mode, expected);
                assert_eq!(claimed.page, "removed-page");
                assert_eq!(claimed.thickness_px, Some((140 + edge.index()) as f32));
                assert_eq!(model.panel(edge).mode, expected);
                assert!(!model.panel(edge).transient_revealed);
            }
            assert_eq!(SavedState::parse(&saved.encode().unwrap()).unwrap(), saved);
        }
    }

    #[test]
    fn v2_round_trips_all_modes_without_transient_visibility() {
        let mut saved = populated();
        let modes = [
            PanelMode::Hidden,
            PanelMode::Pinned,
            PanelMode::Docked,
            PanelMode::Hidden,
        ];
        let output = saved.outputs.get_mut("connector:DP-1").expect("populated");
        for (edge, mode) in output.edges.iter_mut().zip(modes) {
            edge.mode = mode;
        }
        let encoded = saved.encode().unwrap();
        let Value::Map(ref root) = parse_mix_data(&encoded).unwrap() else {
            panic!("map")
        };
        assert_eq!(root.len(), 3);
        assert_eq!(root.get("version"), Some(&Value::Number(3.0)));
        assert!(!encoded.contains("transient"));
        assert_eq!(SavedState::parse(&encoded).unwrap(), saved);
        let mut model = model();
        saved.restore(&mut model);
        for edge in Edge::ALL {
            assert_eq!(
                model.panel(edge).mode,
                saved.outputs["connector:DP-1"].edges[edge.index()].mode
            );
            assert!(!model.panel(edge).transient_revealed);
        }
    }

    #[test]
    fn legacy_load_and_hover_do_not_rewrite_but_normal_mutation_writes_v3() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let source = legacy_source(0);
        std::fs::write(&path, &source).unwrap();
        let mut app = resize_app(&path);
        for input in [
            PanelInput::CornerEntered,
            PanelInput::PointerEntered,
            PanelInput::CornerLeft,
            PanelInput::PointerLeft,
            PanelInput::Hide,
        ] {
            resize_input(&mut app, input);
        }
        assert_eq!(app.world().resource::<StateStore>().write_count, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        resize_input(&mut app, PanelInput::Pin);
        assert_eq!(app.world().resource::<StateStore>().write_count, 1);
        let encoded = std::fs::read_to_string(&path).unwrap();
        let Value::Map(ref root) = parse_mix_data(&encoded).unwrap() else {
            panic!("map")
        };
        assert_eq!(root.get("version"), Some(&Value::Number(3.0)));
        let saved = SavedState::parse(&encoded).unwrap();
        assert_eq!(saved.outputs["connector:DP-1"].edges[0].mode, PanelMode::Pinned);
    }

    #[test]
    fn transient_reveal_is_not_saved_even_when_another_edge_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut app = resize_app(&path);
        for input in [
            PanelInput::CornerEntered,
            PanelInput::PointerEntered,
            PanelInput::CornerLeft,
            PanelInput::PointerLeft,
            PanelInput::Reveal,
        ] {
            resize_input(&mut app, input);
            assert_eq!(app.world().resource::<StateStore>().write_count, 0);
            assert!(!path.exists());
        }
        assert!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .transient_revealed
        );
        resize_command(
            &mut app,
            ShellCommandKind::Panel {
                edge: Edge::Right,
                input: PanelInput::Dock,
            },
        );
        let mut saved = StateStore::load(Some(path)).snapshot();
        assert_eq!(
            saved.outputs["connector:DP-1"].edges[Edge::Left.index()].mode,
            PanelMode::Hidden
        );
        assert_eq!(
            saved.outputs["connector:DP-1"].edges[Edge::Right.index()].mode,
            PanelMode::Docked
        );
        let mut model = model();
        saved.restore(&mut model);
        assert!(!model.panel(Edge::Left).mapped);
        assert!(!model.panel(Edge::Left).transient_revealed);
    }

    #[test]
    fn hover_grace_and_animation_ticks_write_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut app = resize_app(&path);
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::ZERO,
        ));
        resize_input(&mut app, PanelInput::CornerEntered);
        resize_input(&mut app, PanelInput::PointerEntered);
        resize_input(&mut app, PanelInput::CornerLeft);
        resize_input(&mut app, PanelInput::PointerLeft);
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(100),
        ));
        for _ in 0..15 {
            app.update();
            assert_eq!(app.world().resource::<StateStore>().write_count, 0);
            assert!(!path.exists());
        }
        let frame = app.world().resource::<ShellFrameState>();
        assert_eq!(frame.0.panel(Edge::Left).mode, PanelMode::Hidden);
        assert!(!frame.0.panel(Edge::Left).mapped);
        assert!(!frame.0.panel(Edge::Left).transient_revealed);
    }

    #[test]
    fn only_actual_persistent_mode_changes_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut app = resize_app(&path);
        for (input, count) in [
            (PanelInput::Pin, 1),
            (PanelInput::Pin, 1),
            (PanelInput::Dock, 2),
            (PanelInput::Dock, 2),
            (PanelInput::Release, 3),
            (PanelInput::Release, 3),
            (PanelInput::Reveal, 3),
            (PanelInput::Hide, 3),
        ] {
            resize_input(&mut app, input);
            assert_eq!(app.world().resource::<StateStore>().write_count, count);
        }
    }

    #[test]
    fn invalid_versions_and_mixed_shapes_load_defaults_without_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let legacy = legacy_source(15);
        let v3 = populated().encode().unwrap();
        let mut sources = vec![
            "{version: 3, broken".into(),
            legacy.replacen('{', "{version: 2,", 1),
            legacy.replacen('{', "{version: 3,", 1),
            legacy.replacen('{', "{version: 4,", 1),
            legacy.replacen('{', "{version: \"3\",", 1),
            legacy.replace("pinned: true", "mode: \"docked\""),
            legacy.replacen("pinned: true", "pinned: 1", 1),
            legacy.replacen("thickness_px: 140", "thickness_px: -1", 1),
        ];
        // Modify parsed v3 maps rather than depending on the pretty printer's
        // whitespace/key quoting, including unknown and missing fields.
        for case in 0..6 {
            let Value::Map(ref root) = parse_mix_data(&v3).unwrap() else {
                panic!("map")
            };
            let mut root = (**root).clone();
            match case {
                0 => {
                    root.insert("version".into(), Value::Number(2.0));
                }
                1 => {
                    root.shift_remove("version");
                }
                2 => {
                    root.insert("extra".into(), Value::Bool(true));
                }
                _ => {
                    let Some(Value::Map(outputs)) = root.get_mut("outputs") else {
                        panic!("outputs")
                    };
                    let outputs = std::rc::Rc::make_mut(outputs);
                    let Some(Value::Map(fields)) = outputs.get_mut("connector:DP-1") else {
                        panic!("output")
                    };
                    let fields = std::rc::Rc::make_mut(fields);
                    match case {
                        3 => {
                            fields.insert("mode".into(), Value::String("revealed".into()));
                        }
                        4 => {
                            fields.insert("pinned".into(), Value::Bool(true));
                        }
                        _ => {
                            fields.shift_remove("page");
                        }
                    }
                }
            }
            sources.push(Value::map(root).to_mix_data_string_pretty().unwrap());
        }
        for source in sources {
            std::fs::write(&path, &source).unwrap();
            assert!(SavedState::parse(&source).is_err(), "{source}");
            let mut app = resize_app(&path);
            app.update();
            resize_input(&mut app, PanelInput::CornerEntered);
            assert_eq!(
                app.world().resource::<StateStore>().snapshot(),
                SavedState::default()
            );
            assert_eq!(app.world().resource::<StateStore>().write_count, 0);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        }
    }

    #[test]
    fn legacy_popup_release_clears_migrated_reservation_before_conceal() {
        use cosmix_shell::runtime::{ShellSemanticVerb, semantic_shell_command};
        let mut saved = SavedState::parse(&legacy_source(1)).unwrap();
        let mut model = model();
        saved.restore(&mut model);
        model.tick(Duration::from_millis(200)).unwrap();
        assert!(model.panel(Edge::Left).exclusive_zone_px > 0.0);
        let command = semantic_shell_command(
            model.output().clone(),
            model.last_update(),
            Edge::Left,
            ShellSemanticVerb::PanelUnpin,
        );
        let ShellCommandKind::Panel { edge, input } = command.kind else {
            panic!("panel")
        };
        model.panel_input(edge, command.at, input).unwrap();
        assert_eq!(model.panel(edge).mode, PanelMode::Hidden);
        assert_eq!(model.panel(edge).exclusive_zone_px, 0.0);
        // Enqueue acceptance alone is not concealment: the record must remain
        // until the existing props subtree reads pinned=false AND visible=false.
        assert!(model.panel(edge).mapped);
        model
            .panel_input(edge, command.at, PanelInput::Hide)
            .unwrap();
        model.tick(Duration::from_millis(400)).unwrap();
        assert!(!model.panel(edge).mapped);
    }

    #[test]
    fn scheme_dot_applies_dark_theme_and_persists_for_restart() {
        use cosmix_shell::chrome::{QuoinChromePlugin, scheme_dot};
        use ctk::theme::{ApplyTheme, Mode, Scheme};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut model = model();
        model
            .panel_input(Edge::Right, Duration::ZERO, PanelInput::Dock)
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
                Some(scheme.name().to_owned())
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
        let mut loaded = StateStore::load(Some(path)).snapshot();
        assert_eq!(loaded, saved);
        let mut model = model();
        loaded.restore(&mut model);
        for edge in Edge::ALL {
            assert_eq!(
                model.panel(edge).thickness_px,
                saved.outputs["connector:DP-1"].edges[edge.index()].thickness_px.unwrap()
            );
            assert_eq!(
                model.panel(edge).mode == PanelMode::Docked,
                edge == Edge::Left
            );
            assert_eq!(
                model.carousel(edge).active_id(),
                Some(saved.outputs["connector:DP-1"].edges[edge.index()].page.as_str())
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
        invalid
            .outputs
            .get_mut("connector:DP-1")
            .expect("populated output")
            .edges[0]
            .thickness_px = Some(-1.0);
        assert!(SavedState::parse(&invalid.encode().unwrap()).is_err());
    }

    #[test]
    fn unknown_saved_page_uses_registry_default() {
        let mut state = populated();
        state
            .outputs
            .get_mut("connector:DP-1")
            .expect("populated output")
            .edges[0]
            .page = "removed-page".into();
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
                input: PanelInput::Dock,
            },
        });
        app.world_mut()
            .write_message(cosmix_shell::chrome::QuoinSchemeSelected(
                ctk::theme::Scheme::Forest,
            ));
        app.update();
        let store = app.world().resource::<StateStore>();
        assert!(store.path.is_none());
        assert_eq!(store.snapshot(), SavedState::default());
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
                input: PanelInput::Dock,
            }));
        app.world_mut()
            .write_message(command(ShellCommandKind::Carousel {
                edge: Edge::Left,
                input: CarouselInput::Next,
            }));
        app.update();
        let saved = StateStore::load(Some(path)).snapshot();
        assert_eq!(saved.outputs["connector:DP-1"].edges[0].mode, PanelMode::Docked);
        assert_eq!(saved.outputs["connector:DP-1"].edges[0].page, "places");
    }

    #[test]
    fn two_outputs_keep_distinct_selection_and_thickness() {
        let mut state = SavedState::default();
        state.outputs.insert(
            output_identity(&OutputKey::new("DP-1").unwrap()),
            output_state("DP-1", 260.0, PanelMode::Docked, 1),
        );
        state.outputs.insert(
            output_identity(&OutputKey::new("HDMI-1").unwrap()),
            output_state("HDMI-1", 333.0, PanelMode::Pinned, 2),
        );
        assert_eq!(SavedState::parse(&state.encode().unwrap()).unwrap(), state);
        let mut first = model_for("DP-1");
        let mut second = model_for("HDMI-1");
        state.restore(&mut first);
        state.restore(&mut second);
        // Restoring one output never feeds it another output's entry.
        assert_eq!(first.panel(Edge::Left).thickness_px, 260.0);
        assert_eq!(second.panel(Edge::Left).thickness_px, 333.0);
        assert_eq!(first.panel(Edge::Left).mode, PanelMode::Docked);
        assert_eq!(second.panel(Edge::Left).mode, PanelMode::Pinned);
        assert_eq!(first.carousel(Edge::Left).active_id(), Some("places"));
        assert_eq!(second.carousel(Edge::Left).active_id(), Some("info"));
        // Other edges keep their own per-output sizes too.
        assert_eq!(first.panel(Edge::Top).thickness_px, 263.0);
        assert_eq!(second.panel(Edge::Top).thickness_px, 336.0);
    }

    #[test]
    fn legacy_v2_state_migrates_to_default_output() {
        for (source, thickness, page) in [
            (legacy_source(9), 140.0, "removed-page"),
            (v2_source(), 150.0, "places"),
        ] {
            let mut saved = SavedState::parse(&source).unwrap();
            // The single pre-v3 edge set parks under the reserved identity.
            assert_eq!(saved.outputs.len(), 1);
            let pool = &saved.outputs[DEFAULT_OUTPUT];
            assert_eq!(pool.edges[0].thickness_px, Some(thickness));
            assert_eq!(pool.edges[0].mode, PanelMode::Docked);
            assert_eq!(pool.edges[0].page, page);

            // The output Quoin runs on claims it; the claim survives encode.
            let mut first = model_for("DP-1");
            saved.restore(&mut first);
            assert_eq!(first.panel(Edge::Left).thickness_px, thickness);
            assert_eq!(first.panel(Edge::Left).mode, PanelMode::Docked);
            assert!(saved.outputs.contains_key("connector:DP-1"));
            assert!(!saved.outputs.contains_key(DEFAULT_OUTPUT));
            assert_eq!(SavedState::parse(&saved.encode().unwrap()).unwrap(), saved);

            // A later unknown output gets the default config, not the
            // migrated edges (shell design §7's new-output rule).
            let fresh = model_for("HDMI-1");
            let mut second = model_for("HDMI-1");
            saved.restore(&mut second);
            for edge in Edge::ALL {
                assert_eq!(second.panel(edge).mode, PanelMode::Hidden);
                assert_eq!(second.panel(edge).thickness_px, fresh.panel(edge).thickness_px);
                assert_eq!(second.carousel(edge).active_id(), fresh.carousel(edge).active_id());
            }
        }
    }

    #[test]
    fn unknown_output_gets_default_config() {
        let mut state = populated();
        let fresh = model_for("HDMI-1");
        let mut model = model_for("HDMI-1");
        state.restore(&mut model);
        for edge in Edge::ALL {
            assert_eq!(model.panel(edge).mode, PanelMode::Hidden);
            assert_eq!(model.panel(edge).thickness_px, fresh.panel(edge).thickness_px);
            assert_eq!(model.carousel(edge).active_id(), fresh.carousel(edge).active_id());
        }
        // Restoring an unknown output claims nothing and creates nothing;
        // its entry first appears on its own next save.
        assert_eq!(state.outputs.len(), 1);
        assert!(state.outputs.contains_key("connector:DP-1"));
    }

    #[test]
    fn reconnect_restores_removed_outputs_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        let mut state = SavedState {
            scheme: "dual".into(),
            ..SavedState::default()
        };
        state.outputs.insert(
            output_identity(&OutputKey::new("DP-1").unwrap()),
            output_state("DP-1", 210.0, PanelMode::Docked, 2),
        );
        state.outputs.insert(
            output_identity(&OutputKey::new("HDMI-1").unwrap()),
            output_state("HDMI-1", 321.0, PanelMode::Pinned, 1),
        );
        atomic_save(&path, &state).unwrap();

        // Running on HDMI-1 alone rewrites only that output's entry (with
        // its live model state — Pin on the fresh model, not the saved
        // Pinned mode); the removed DP-1's remembered configuration waits
        // for reconnection untouched.
        let mut app = resize_app_for(&path, "HDMI-1");
        resize_command_for(
            &mut app,
            "HDMI-1",
            ShellCommandKind::Panel {
                edge: Edge::Left,
                input: PanelInput::Pin,
            },
        );
        let mut saved = StateStore::load(Some(path)).snapshot();
        let removed = &saved.outputs["connector:DP-1"].edges;
        assert_eq!(removed[0].thickness_px, Some(210.0));
        assert_eq!(removed[0].mode, PanelMode::Docked);
        assert_eq!(removed[0].page, "info");
        assert_eq!(
            saved.outputs["connector:HDMI-1"].edges[0].mode,
            PanelMode::Pinned
        );

        // Reconnected, DP-1 restores exactly what it remembered.
        let mut model = model_for("DP-1");
        saved.restore(&mut model);
        assert_eq!(model.panel(Edge::Left).thickness_px, 210.0);
        assert_eq!(model.panel(Edge::Left).mode, PanelMode::Docked);
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("info"));
    }
}
