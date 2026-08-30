use std::collections::{hash_map::DefaultHasher, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::SystemTime;

use bevy::app::App;
use bevy::color::{Color, LinearRgba as BevyLinearRgba};
use bevy::ecs::prelude::{Res, ResMut, Resource, SystemSet};
use bevy::log::{error, info, warn};
use cosmix_design::{
    apply_compiled_design, compile_design, parse_design_source, ButtonCellKey, Contrast,
    DesignApplyDecision, DesignCompileOutcome, DesignCompileStatus, DesignContext,
    DesignDiagnostic, DesignRevision, DiagnosticSeverity, LinearRgba, ResolvedButtonCell,
    ResolvedDesign, SourceIdentity, EMBEDDED_DEFAULT_SOURCE,
};

use crate::theme::{Mode, Scheme, ThemeState};

const EMBEDDED_SOURCE_IDENTITY: &str = "ctk:embedded-default";
const LOGGED_FAILURE_CAPACITY: usize = 8;

#[derive(Clone, Eq, Hash, PartialEq)]
struct CompileKey {
    source_generation: u64,
    source_fingerprint: u64,
    identity: SourceIdentity,
    context: DesignContext,
}

impl fmt::Debug for CompileKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompileKey")
            .field("source_generation", &self.source_generation)
            .field("source_fingerprint", &self.source_fingerprint)
            .field("identity", &self.identity.as_str())
            .field("context", &self.context)
            .finish()
    }
}

/// The last-known-good compiled design used by CTK widgets.
#[derive(Resource, Debug)]
pub struct CtkDesign {
    live: Option<ResolvedDesign>,
}

impl CtkDesign {
    pub fn live(&self) -> Option<&ResolvedDesign> {
        self.live.as_ref()
    }

    pub fn revision(&self) -> Option<DesignRevision> {
        self.live.as_ref().map(ResolvedDesign::revision)
    }

    pub fn button_cell(&self, key: ButtonCellKey) -> Option<&ResolvedButtonCell> {
        self.live
            .as_ref()
            .map(|design| design.tables().button.cell(key))
    }
}

/// Compile state and the selected in-memory design source.
#[derive(Resource)]
pub struct CtkDesignStatus {
    source_identity: SourceIdentity,
    source_generation: u64,
    source_fingerprint: u64,
    source: Arc<[u8]>,
    attempted: Option<CompileKey>,
    applied: Option<CompileKey>,
    last_compile: Option<DesignCompileStatus>,
    last_error: Option<String>,
    logged_failures: VecDeque<u64>,
}

impl fmt::Debug for CtkDesignStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let last_compile_outcome = self.last_compile.as_ref().map(|status| status.outcome);
        formatter
            .debug_struct("CtkDesignStatus")
            .field("source_identity", &self.source_identity.as_str())
            .field("source_generation", &self.source_generation)
            .field("source_fingerprint", &self.source_fingerprint)
            .field("source_len", &self.source.len())
            .field("attempted", &self.attempted)
            .field("applied", &self.applied)
            .field("last_compile_outcome", &last_compile_outcome)
            .field("last_error", &self.last_error)
            .field("logged_failure_count", &self.logged_failures.len())
            .finish()
    }
}

impl CtkDesignStatus {
    /// Replace the complete design source. `CtkWidgetsPlugin` compiles it on
    /// the next update.
    pub fn replace_source(&mut self, identity: impl Into<String>, source: impl Into<String>) {
        self.replace_source_bytes(identity, source.into().into_bytes());
    }

    pub(crate) fn replace_source_bytes(
        &mut self,
        identity: impl Into<String>,
        source: impl Into<Vec<u8>>,
    ) {
        let identity = SourceIdentity::new(identity);
        let source: Arc<[u8]> = Arc::from(source.into());
        if self.source_identity == identity && self.source == source {
            return;
        }
        self.source_identity = identity;
        self.source = source;
        self.source_fingerprint = fingerprint_source(&self.source_identity, &self.source);
        self.source_generation = self.source_generation.wrapping_add(1);
    }

    pub fn use_embedded_source(&mut self) {
        self.replace_source_bytes(
            EMBEDDED_SOURCE_IDENTITY,
            EMBEDDED_DEFAULT_SOURCE.as_bytes().to_vec(),
        );
    }

    pub fn source_identity(&self) -> &SourceIdentity {
        &self.source_identity
    }

    pub fn last_compile(&self) -> Option<&DesignCompileStatus> {
        self.last_compile.as_ref()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum CtkDesignSystems {
    Sync,
}

pub(crate) fn init_design_resources(app: &mut App) {
    if !app.world().contains_resource::<CtkDesign>() {
        let (design, status) = design_resources_for_source(
            EMBEDDED_SOURCE_IDENTITY,
            EMBEDDED_DEFAULT_SOURCE,
            Scheme::Ocean,
            Mode::Light,
        );
        app.insert_resource(design);
        if !app.world().contains_resource::<CtkDesignStatus>() {
            app.insert_resource(status);
        }
    } else if !app.world().contains_resource::<CtkDesignStatus>() {
        app.insert_resource(empty_status());
    }
}

/// Parses and compiles synchronously in `Update`, but only when the key changes.
pub(crate) fn sync_ctk_design(
    mut design: ResMut<CtkDesign>,
    mut status: ResMut<CtkDesignStatus>,
    state: Res<ThemeState>,
) {
    let key = CompileKey {
        source_generation: status.source_generation,
        source_fingerprint: status.source_fingerprint,
        identity: status.source_identity.clone(),
        context: design_context(state.scheme, state.mode),
    };
    if status.attempted.as_ref() != Some(&key) {
        let source = Arc::clone(&status.source);
        match std::str::from_utf8(&source) {
            Ok(source) => apply_source(&mut design, &mut status, key, source),
            Err(parse_error) => {
                let fingerprint = fingerprint(&key);
                let message = format!(
                    "{}: design source is not UTF-8: {parse_error}",
                    key.identity.as_str()
                );
                record_source_rejection(&mut status, key, fingerprint, "invalid-utf8", message);
            }
        }
    }
}

pub(crate) fn design_resources_for_source(
    identity: &str,
    source: &str,
    scheme: Scheme,
    mode: Mode,
) -> (CtkDesign, CtkDesignStatus) {
    let source: Arc<[u8]> = Arc::from(source.as_bytes());
    let key = CompileKey {
        source_generation: 1,
        source_fingerprint: fingerprint_source(&SourceIdentity::new(identity), &source),
        identity: SourceIdentity::new(identity),
        context: design_context(scheme, mode),
    };
    let mut design = CtkDesign { live: None };
    let mut status = CtkDesignStatus {
        source_identity: key.identity.clone(),
        source_generation: key.source_generation,
        source_fingerprint: key.source_fingerprint,
        source: Arc::clone(&source),
        attempted: None,
        applied: None,
        last_compile: None,
        last_error: None,
        logged_failures: VecDeque::new(),
    };
    apply_source(
        &mut design,
        &mut status,
        key,
        std::str::from_utf8(&source).expect("Rust string source is UTF-8"),
    );
    (design, status)
}

fn empty_status() -> CtkDesignStatus {
    CtkDesignStatus {
        source_identity: SourceIdentity::new(EMBEDDED_SOURCE_IDENTITY),
        source_generation: 1,
        source_fingerprint: fingerprint_source(
            &SourceIdentity::new(EMBEDDED_SOURCE_IDENTITY),
            EMBEDDED_DEFAULT_SOURCE.as_bytes(),
        ),
        source: Arc::from(EMBEDDED_DEFAULT_SOURCE.as_bytes()),
        attempted: None,
        applied: None,
        last_compile: None,
        last_error: None,
        logged_failures: VecDeque::new(),
    }
}

fn apply_source(
    design: &mut CtkDesign,
    status: &mut CtkDesignStatus,
    key: CompileKey,
    source: &str,
) {
    let fingerprint = fingerprint(&key);
    let document = match parse_design_source(key.identity.clone(), source) {
        Ok(document) => document,
        Err(parse_error) => {
            let message = parse_error.to_string();
            record_source_rejection(status, key, fingerprint, "parse-error", message);
            return;
        }
    };

    let result = compile_design(&document, key.context.clone());
    let transition = apply_compiled_design(design.live.take(), result, SystemTime::now());
    let replaced = transition.decision == DesignApplyDecision::Replaced;
    let fatal = transition.status.outcome == DesignCompileOutcome::Fatal;
    let should_log_failure = fatal && !status.logged_failures.contains(&fingerprint);
    for diagnostic in &transition.status.diagnostics {
        let message = format!(
            "{} [{}] {}: {}",
            transition.status.attempted_source.as_str(),
            diagnostic.code,
            diagnostic.path,
            diagnostic.message
        );
        if should_log_failure {
            error!("CTK design compile failed: {message}");
        } else if !fatal {
            warn!("CTK design compiler warning: {message}");
        }
    }
    design.live = transition.design;
    status.attempted = Some(key.clone());
    status.last_compile = Some(transition.status);
    if replaced {
        let revision = design
            .revision()
            .expect("a replaced design has an applied revision");
        info!(
            "CTK design applied: source={} revision={} context={}/{} generation={}",
            key.identity.as_str(),
            revision.get(),
            key.context.scheme.name(),
            key.context.mode.name(),
            key.source_generation
        );
        status.applied = Some(key);
        status.last_error = None;
    } else {
        status.last_error = Some("design compilation failed; retaining last-good design".into());
        remember_logged_failure(status, fingerprint);
    }
}

fn record_source_rejection(
    status: &mut CtkDesignStatus,
    key: CompileKey,
    fingerprint: u64,
    code: &'static str,
    message: String,
) {
    status.last_compile = Some(DesignCompileStatus {
        attempted_source: key.identity.clone(),
        outcome: DesignCompileOutcome::Fatal,
        diagnostics: vec![DesignDiagnostic {
            severity: DiagnosticSeverity::Error,
            code,
            path: "design".into(),
            message: message.clone(),
            suggestion: None,
        }],
        compiled_at: SystemTime::now(),
    });
    status.attempted = Some(key);
    status.last_error = Some(message.clone());
    log_failure_once(status, fingerprint, &message);
}

fn log_failure_once(status: &mut CtkDesignStatus, fingerprint: u64, message: &str) {
    if !status.logged_failures.contains(&fingerprint) {
        error!("CTK design source rejected: {message}");
        remember_logged_failure(status, fingerprint);
    }
}

fn remember_logged_failure(status: &mut CtkDesignStatus, fingerprint: u64) {
    if status.logged_failures.contains(&fingerprint) {
        return;
    }
    if status.logged_failures.len() == LOGGED_FAILURE_CAPACITY {
        status.logged_failures.pop_front();
    }
    status.logged_failures.push_back(fingerprint);
}

fn fingerprint(key: &CompileKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.identity.hash(&mut hasher);
    key.source_fingerprint.hash(&mut hasher);
    key.context.hash(&mut hasher);
    hasher.finish()
}

fn fingerprint_source(identity: &SourceIdentity, source: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}

fn design_context(scheme: Scheme, mode: Mode) -> DesignContext {
    DesignContext {
        scheme: match scheme {
            Scheme::Ocean => cosmix_design::Scheme::Ocean,
            Scheme::Crimson => cosmix_design::Scheme::Crimson,
            Scheme::Stone => cosmix_design::Scheme::Stone,
            Scheme::Forest => cosmix_design::Scheme::Forest,
            Scheme::Sunset => cosmix_design::Scheme::Sunset,
            Scheme::Mono => cosmix_design::Scheme::Mono,
        },
        mode: match mode {
            Mode::Light => cosmix_design::Mode::Light,
            Mode::Dark => cosmix_design::Mode::Dark,
        },
        contrast: Contrast::Normal,
        app: None,
    }
}

pub(crate) fn bevy_color(colour: LinearRgba) -> Color {
    Color::LinearRgba(BevyLinearRgba::new(
        colour.red as f32,
        colour.green as f32,
        colour.blue as f32,
        colour.alpha as f32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_replacement_keeps_the_last_good_design() {
        let (mut design, mut status) = design_resources_for_source(
            EMBEDDED_SOURCE_IDENTITY,
            EMBEDDED_DEFAULT_SOURCE,
            Scheme::Ocean,
            Mode::Light,
        );
        let revision = design.revision();
        let key = CompileKey {
            source_generation: status.source_generation.wrapping_add(1),
            source_fingerprint: fingerprint_source(
                &SourceIdentity::new("memory:bad"),
                b"design: nope",
            ),
            identity: SourceIdentity::new("memory:bad"),
            context: design_context(Scheme::Ocean, Mode::Light),
        };

        apply_source(&mut design, &mut status, key, "design: nope");

        assert_eq!(design.revision(), revision);
        assert!(status.last_error().is_some());
        assert_eq!(
            status.last_compile().map(|status| status.outcome),
            Some(DesignCompileOutcome::Fatal)
        );
    }

    #[test]
    fn status_debug_summarises_source_without_dumping_it() {
        let (_, status) = design_resources_for_source(
            EMBEDDED_SOURCE_IDENTITY,
            EMBEDDED_DEFAULT_SOURCE,
            Scheme::Ocean,
            Mode::Light,
        );

        let rendered = format!("{status:?}");

        assert!(rendered.contains("source_generation"));
        assert!(rendered.contains("source_len"));
        assert!(rendered.contains("last_compile_outcome"));
        assert!(!rendered.contains("schema_version"));
        assert!(!rendered.contains(EMBEDDED_DEFAULT_SOURCE));
    }

    #[test]
    fn recent_failure_fingerprints_are_bounded_and_deduplicated() {
        let (_, mut status) = design_resources_for_source(
            EMBEDDED_SOURCE_IDENTITY,
            EMBEDDED_DEFAULT_SOURCE,
            Scheme::Ocean,
            Mode::Light,
        );

        for fingerprint in 0..=LOGGED_FAILURE_CAPACITY as u64 {
            remember_logged_failure(&mut status, fingerprint);
        }
        assert_eq!(status.logged_failures.len(), LOGGED_FAILURE_CAPACITY);
        assert!(!status.logged_failures.contains(&0));
        let before = status.logged_failures.clone();
        remember_logged_failure(&mut status, 1);
        assert_eq!(status.logged_failures, before);
    }
}
