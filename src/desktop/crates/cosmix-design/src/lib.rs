//! Headless compiler contracts for the Cosmix Design System.
//!
//! This crate owns the closed family schemas and resolved-design data model.
//! It deliberately has no Bevy or CTK dependency; rendering adapters live in
//! their consumer crates.
//!
//! The context-free mapping compiler is deliberately not a public entry point:
//! ```compile_fail
//! use cosmix_design::MappingCompileFailure;
//! ```
//! ```compile_fail
//! use cosmix_design::compile_button_mapping;
//! ```
//! A compiled candidate exposes its artifact through read-only accessors:
//! ```compile_fail
//! fn mutate_candidate(candidate: &mut cosmix_design::UnstampedResolvedDesign) {
//!     candidate.dictionary.colours.pairs.clear();
//! }
//! ```
//! Compiled pair policies likewise cannot be fabricated by consumers:
//! ```compile_fail
//! let _ = cosmix_design::PairSubstitutionPolicy {
//!     slot: 0,
//!     decisions: std::collections::BTreeMap::new(),
//! };
//! ```

/// Revision-1 strict-data default embedded into every compiler build.
#[cfg(feature = "compiler")]
pub const EMBEDDED_DEFAULT_SOURCE: &str = include_str!("defaults/revision-1.theme.conf.mix");
#[cfg(feature = "compiler")]
pub const EMBEDDED_DEFAULT_REVISION: DesignRevision = DesignRevision::FIRST;

mod axis;
#[cfg(feature = "compiler")]
mod colour;
mod colour_model;
#[cfg(feature = "compiler")]
mod compiler;
mod context;
mod design_model;
mod diagnostic;
#[cfg(feature = "compiler")]
mod equivalence;
pub mod family;
#[cfg(feature = "compiler")]
mod mapping;
mod mapping_model;
mod recipe;
#[cfg(feature = "compiler")]
mod recipe_compiler;
#[cfg(feature = "compiler")]
mod source;
mod state;
#[cfg(all(test, feature = "compiler"))]
mod trial;

#[cfg(feature = "compiler")]
pub use colour::{ColourCompileFailure, compile_colour_tokens};
pub use colour_model::{
    FocusRingProvenance, LinearRgba, NON_TEXT_NAMES, ResolvedColours, ResolvedNonTextColour,
    ResolvedPair, TEXT_PAIR_NAMES, contrast_ratio,
};
#[cfg(feature = "compiler")]
pub use compiler::compile_design;
pub use context::{Contrast, DesignContext, Mode, Scheme};
pub use design_model::{
    AuthoredMetric, DesignApplyDecision, DesignApplyTransition, DesignCompileFailure,
    DesignCompileOutcome, DesignCompileResult, DesignCompileStatus, DesignCompileSuccess,
    DesignProvenance, DesignRevision, DesignValueId, PairOverrideDisposition, PairOverrideRoute,
    ResolvedDesign, ResolvedDictionary, ResolvedMetric, ResolvedMetricKind, ResolvedTables,
    ResolvedTypography, ResolvedTypographyRef, SourceIdentity, UnstampedResolvedDesign,
    ValueProvenance, apply_compiled_design,
};
pub use diagnostic::{CompileSuccess, DesignDiagnostic, DiagnosticSeverity};
#[cfg(feature = "compiler")]
pub use equivalence::parse_legacy_v0_hex_colour;
pub use family::button::{ButtonPart, ButtonSize, ButtonVariant};
pub use family::{FAMILY_SCHEMAS, FamilyId, FamilyPart, FamilySchema};
pub use mapping_model::{
    BUTTON_CELL_COUNT, BUTTON_TYPOGRAPHY_COUNT, ButtonCellKey, ButtonProperty, ButtonTypographyKey,
    ButtonTypographyTable, ResolvedButtonCell, ResolvedButtonTable, ResolvedTypeRecord,
    ResolvedTypographyAssignment,
};
pub use recipe::{
    DerivationRecipe, PairRefDecision, PairRefExclusion, PairSubstitutionPolicy, REGISTRY,
    RecipeBinding, RecipeImplicitBinding, RecipeImplicitInput, RecipeMovement, RecipeOutput,
    RecipePairDomain, RecipeParam, RecipeSignature, RecipeSubstitutionDomainConstraint,
};
#[cfg(feature = "compiler")]
pub use source::{
    AuthoredPairSource, ButtonInheritanceSource, ButtonMappingSource, ColourSpace, CoveragePolicy,
    DerivationCallSource, DesignSourceDocument, DesignSourceError, DesignSourceErrorCode,
    DesignV1Source, FamilyMappingsSource, LegacyTypographySource, LegacyV0Source,
    MappingRuleSource, MappingSelectorSource, MappingValueSource, MetricSource, ModifierAxis,
    ModifierBlockSource, NonTextColourSource, OklchSource, PairSource, PrimitiveSource,
    RecipeArgumentSource, SemanticSource, SourceKind, TaggedMetricSource, TypeRecordSource,
    TypographySource, V0CrosswalkExpressionSource, V0MappingProperty, V0PairMember,
    parse_design_source, parse_legacy_v0_source,
};
pub use state::{InteractionState, StyleStateKey};

#[cfg(all(test, not(feature = "compiler")))]
mod model_without_compiler_tests {
    use super::*;

    // Names resolved-model contracts through the crate root so a future
    // accidental compiler gate fails at compile time. Functions are named as
    // values, not just their types: gating a `fn` and its re-export would
    // otherwise leave this test compiling untouched.
    //
    // This is a hand-maintained list, not a proof of the whole API surface — a
    // newly added ungated item is only covered once it is named here. It is
    // worth exactly what it enumerates.
    #[test]
    fn resolved_model_remains_available_without_compiler() {
        let _: fn(
            Option<ResolvedDesign>,
            DesignCompileResult,
            std::time::SystemTime,
        ) -> DesignApplyTransition = apply_compiled_design;
        let _ = ResolvedTypography::button;
        let _ = ResolvedTypography::scale;
        let _ = ResolvedTypography::record;
        let _ = ResolvedTypography::button_assignments;
        let _ = ButtonTypographyTable::assignment;
        let _ = ResolvedDesign::revision;
        let _ = ResolvedDesign::source;
        let _ = ResolvedDesign::tables;
        let _ = ResolvedDesign::dictionary;
        let _ = ResolvedDesign::typography;
        let _ = ResolvedDesign::provenance;
        let _ = ResolvedDesign::button_pair_override;
        let _ = UnstampedResolvedDesign::source;
        let _ = UnstampedResolvedDesign::tables;
        let _ = UnstampedResolvedDesign::dictionary;
        let _ = UnstampedResolvedDesign::typography;
        let _ = UnstampedResolvedDesign::provenance;
        let _ = UnstampedResolvedDesign::button_pair_override;
        let _ = DesignProvenance::value;
        let _ = DesignProvenance::len;
        let _ = DesignProvenance::is_empty;
        let _ = ButtonTypographyTable::len;
        let _ = ButtonTypographyTable::is_empty;
        let _ = RecipeBinding::param;
        let _ = DesignRevision::succeeding;
        let _ = DesignRevision::get;
        let _ = SourceIdentity::as_str;
        let _ = ButtonVariant::name;
        let _ = ButtonVariant::index;
        let _ = ButtonSize::name;
        let _ = ButtonSize::index;
        let _ = ButtonPart::name;
        let _ = ButtonPart::index;
        let _ = InteractionState::name;
        let _ = InteractionState::index;
        let _ = std::mem::size_of::<ResolvedTypographyRef<'_>>();
        let _ = LinearRgba::BLACK.to_srgba8();
        let _ = contrast_ratio(LinearRgba::BLACK, LinearRgba::WHITE);
        let _ = std::mem::size_of::<ResolvedPair>();
        let _ = std::mem::size_of::<ResolvedNonTextColour>();
        let _ = std::mem::size_of::<ResolvedColours>();
        let _ = TEXT_PAIR_NAMES.len();
        let _ = NON_TEXT_NAMES.len();
        let _ = std::mem::size_of::<ButtonCellKey>();
        let _ = BUTTON_CELL_COUNT;
        let _ = std::mem::size_of::<ResolvedButtonCell>();
        let _ = std::mem::size_of::<ResolvedButtonTable>();
        let _ = ResolvedButtonTable::cell;
        let _ = ResolvedButtonTable::len;
        let _ = ResolvedButtonTable::is_empty;
        let _ = std::mem::size_of::<ResolvedTypeRecord>();
        let _ = std::mem::size_of::<DerivationRecipe>();
        let _ = std::mem::size_of::<RecipeSignature>();
        let _ = std::mem::size_of::<PairSubstitutionPolicy>();
        let _ = std::mem::size_of::<PairRefDecision>();
        let _ = std::mem::size_of::<PairRefExclusion>();
        let _ = std::mem::size_of::<PairOverrideDisposition<'_>>();
        let _ = std::mem::size_of::<PairOverrideRoute<'_>>();
        let _ = REGISTRY.len();
        let _ = std::mem::size_of::<ButtonTypographyKey>();
        let _ = std::mem::size_of::<ButtonTypographyTable>();
        let _ = std::mem::size_of::<ResolvedTypography>();
        let _ = std::mem::size_of::<ResolvedDictionary>();
        let _ = std::mem::size_of::<ResolvedMetric>();
        let _ = std::mem::size_of::<ResolvedMetricKind>();
        let _ = std::mem::size_of::<AuthoredMetric>();
        let _ = std::mem::size_of::<ValueProvenance>();
        let _ = std::mem::size_of::<DesignValueId>();
        let _ = std::mem::size_of::<ResolvedTables>();
        let _ = std::mem::size_of::<DesignProvenance>();
        let _ = std::mem::size_of::<UnstampedResolvedDesign>();
        let _ = std::mem::size_of::<ResolvedDesign>();
        let _ = std::mem::size_of::<DesignCompileStatus>();
        let _ = std::mem::size_of::<DesignCompileResult>();
        let _ = std::mem::size_of::<DesignApplyDecision>();
        let _ = std::mem::size_of::<DesignApplyTransition>();
        let _ = DesignRevision::FIRST.next();
        let _ = SourceIdentity::new("model-only");
        let _ = std::mem::size_of::<DesignDiagnostic>();
        let _ = std::mem::size_of::<DiagnosticSeverity>();
        let _ = DesignContext::default();
    }
}
