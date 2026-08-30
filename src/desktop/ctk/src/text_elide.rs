//! Width-aware single-line text elision for dense CTK surfaces.
//!
//! [`MiddleElideText`] keeps the authored string separate from its visual
//! projection. The runtime system measures candidates through Bevy's own text
//! pipeline after CTK typography has applied, so the chosen string uses the
//! same font resolution, shaping and physical-pixel quantisation as rendering.

use bevy::a11y::AccessibilityNode;
use bevy::asset::Assets;
use bevy::color::Color;
use bevy::ecs::change_detection::{DetectChanges, Ref};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::system::{Query, Res, ResMut};
use bevy::text::{
    ComputedTextBlock, Font, FontCx, Justify, LayoutCx, LetterSpacing, LineBreak, LineHeight,
    RemSize, TextFont, TextLayout, TextLayoutInfo, TextPipeline,
};
use bevy::ui::widget::Text;
use bevy::ui::{ComputedNode, ComputedUiRenderTargetInfo};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::CtkTypography;

const ELLIPSIS: &str = "…";
const MAX_EXTENSION_GRAPHEMES: usize = 12;
const MAX_CANDIDATE_MEASURES: usize = 32;

/// A single-line text entity whose middle-elided visual follows another UI
/// entity's computed content width.
///
/// Add this to the `Text` entity and point `width_host` at a stable flex child
/// that owns the exact remaining width. Do not use the text entity itself as
/// the host: changing the displayed string would then change the measurement
/// budget.
#[derive(Component, Clone, Debug)]
pub struct MiddleElideText {
    full_text: String,
    width_host: Entity,
    accessibility_target: Option<Entity>,
    cache: Option<ElideCache>,
}

impl MiddleElideText {
    /// Create a filename-style middle-elided text projection.
    pub fn new(full_text: impl Into<String>, width_host: Entity) -> Self {
        Self {
            full_text: full_text.into(),
            width_host,
            accessibility_target: None,
            cache: None,
        }
    }

    /// Keep the unelided source as the label of this accessibility entity.
    pub fn with_accessibility_target(mut self, target: Entity) -> Self {
        self.accessibility_target = Some(target);
        self
    }

    /// The complete authored string, never the elided visual projection.
    pub fn full_text(&self) -> &str {
        &self.full_text
    }

    /// Replace the authored string and invalidate prior width measurements.
    pub fn set_full_text(&mut self, full_text: impl Into<String>) {
        let full_text = full_text.into();
        if self.full_text != full_text {
            self.full_text = full_text;
            self.cache = None;
        }
    }

    /// Point this projection at a different stable width owner.
    pub fn set_width_host(&mut self, width_host: Entity) {
        if self.width_host != width_host {
            self.width_host = width_host;
            self.cache = None;
        }
    }
}

#[derive(Clone, Debug)]
struct ElideCache {
    source: String,
    width: f32,
    font: TextFont,
    justify: Justify,
    linebreak: LineBreak,
    line_height: LineHeight,
    letter_spacing: LetterSpacing,
    scale_factor: f32,
    physical_viewport: bevy::math::UVec2,
    rem_size: f32,
    typography_revision: u64,
    rendered: String,
}

impl ElideCache {
    #[allow(clippy::too_many_arguments)]
    fn matches(
        &self,
        source: &str,
        width: f32,
        font: &TextFont,
        layout: &TextLayout,
        line_height: LineHeight,
        letter_spacing: LetterSpacing,
        target: &ComputedUiRenderTargetInfo,
        rem_size: f32,
        typography_revision: u64,
    ) -> bool {
        self.source == source
            && self.width == width
            && self.font == *font
            && self.justify == layout.justify
            && self.linebreak == layout.linebreak
            && self.line_height == line_height
            && self.letter_spacing == letter_spacing
            && self.scale_factor == target.scale_factor()
            && self.physical_viewport == target.physical_size()
            && self.rem_size == rem_size
            && self.typography_revision == typography_revision
    }

    #[allow(clippy::too_many_arguments)]
    fn style_matches(
        &self,
        source: &str,
        font: &TextFont,
        layout: &TextLayout,
        line_height: LineHeight,
        letter_spacing: LetterSpacing,
        target: &ComputedUiRenderTargetInfo,
        rem_size: f32,
        typography_revision: u64,
    ) -> bool {
        self.source == source
            && self.font == *font
            && self.justify == layout.justify
            && self.linebreak == layout.linebreak
            && self.line_height == line_height
            && self.letter_spacing == letter_spacing
            && self.scale_factor == target.scale_factor()
            && self.physical_viewport == target.physical_size()
            && self.rem_size == rem_size
            && self.typography_revision == typography_revision
            && self.rendered == source
    }
}

/// Middle-elide a filename using caller-supplied rendered-width measurement.
///
/// The extension-preservation rule is:
///
/// - preserve the complete final extension when it is at most 12 grapheme
///   clusters including the dot;
/// - for a longer extension, preserve the dot plus its final 11 graphemes;
/// - for names without an extension, preserve at least the final grapheme.
///
/// If even `…` does not fit, the visual result is empty. Candidate measurement
/// is bounded; exhaustion safely falls back to `…`, which has already been
/// proven to fit.
///
/// The longest-prefix search assumes measured width is broadly monotonic as
/// graphemes are added. Ligatures, bidi shaping or another context-sensitive
/// feature can violate that assumption and produce a shorter-than-optimal
/// result, but every returned candidate was individually measured to fit, so
/// this cannot cause overflow.
pub fn elide_filename_middle_with_measure<E>(
    text: &str,
    max_width: f32,
    mut measure: impl FnMut(&str) -> Result<f32, E>,
) -> Result<String, E> {
    if text.is_empty() || !max_width.is_finite() || max_width <= 0.0 {
        return Ok(String::new());
    }
    if measure(text)? <= max_width {
        return Ok(text.to_owned());
    }
    if measure(ELLIPSIS)? > max_width {
        return Ok(String::new());
    }

    let boundaries: Vec<usize> = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect();
    let grapheme_count = boundaries.len().saturating_sub(1);
    if grapheme_count <= 1 {
        return Ok(ELLIPSIS.to_owned());
    }

    let extension_start = text
        .rfind('.')
        .filter(|index| *index > 0 && *index + 1 < text.len())
        .and_then(|index| boundaries.iter().position(|boundary| *boundary == index));
    let stem_count = extension_start.unwrap_or(grapheme_count - 1);
    let extension_count = extension_start.map_or(0, |start| grapheme_count - start);
    let long_extension = extension_count > MAX_EXTENSION_GRAPHEMES;
    let initial_suffix_count = if extension_count == 0 {
        1
    } else {
        extension_count.min(MAX_EXTENSION_GRAPHEMES)
    };

    let candidate = |prefix_count: usize, suffix_count: usize| {
        let prefix = &text[..boundaries[prefix_count]];
        if long_extension {
            let tail_count = suffix_count.saturating_sub(1);
            let tail = &text[boundaries[grapheme_count - tail_count]..];
            format!("{prefix}{ELLIPSIS}.{tail}")
        } else {
            let suffix = &text[boundaries[grapheme_count - suffix_count]..];
            format!("{prefix}{ELLIPSIS}{suffix}")
        }
    };

    let mut measures = 2;
    for suffix_count in (1..=initial_suffix_count).rev() {
        let mut low = 0;
        let mut high = stem_count;
        let mut longest_fitting = None;
        while low <= high {
            if measures >= MAX_CANDIDATE_MEASURES {
                return Ok(ELLIPSIS.to_owned());
            }
            measures += 1;
            let middle = low + (high - low) / 2;
            let candidate = candidate(middle, suffix_count);
            if measure(&candidate)? <= max_width {
                longest_fitting = Some(candidate);
                low = middle + 1;
            } else if middle == 0 {
                break;
            } else {
                high = middle - 1;
            }
        }
        if let Some(candidate) = longest_fitting {
            return Ok(candidate);
        }
    }

    Ok(ELLIPSIS.to_owned())
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn update_middle_elided_text(
    fonts: Res<Assets<Font>>,
    typography: Res<CtkTypography>,
    rem_size: Res<RemSize>,
    mut text_pipeline: ResMut<TextPipeline>,
    mut font_cx: ResMut<FontCx>,
    mut layout_cx: ResMut<LayoutCx>,
    width_hosts: Query<&ComputedNode>,
    mut accessibility: Query<&mut AccessibilityNode>,
    mut texts: Query<(
        Entity,
        &mut Text,
        &TextFont,
        &TextLayout,
        &LineHeight,
        &LetterSpacing,
        &ComputedUiRenderTargetInfo,
        Ref<TextLayoutInfo>,
        &mut MiddleElideText,
    )>,
) {
    let font_assets_changed = fonts.is_changed();
    for (
        entity,
        mut text,
        font,
        layout,
        line_height,
        letter_spacing,
        target,
        layout_info,
        mut elide,
    ) in &mut texts
    {
        if let Some(target_entity) = elide.accessibility_target {
            if let Ok(mut node) = accessibility.get_mut(target_entity) {
                if node.label() != Some(elide.full_text.as_str()) {
                    node.set_label(elide.full_text.clone());
                }
            }
        }

        let Ok(host) = width_hosts.get(elide.width_host) else {
            continue;
        };
        // A newly spawned host has the default zero-by-zero ComputedNode until
        // the first Taffy pass. Leave the authored text in place for that pass;
        // a real zero-width host still has a laid-out height and is emptied on
        // the next branch.
        if host.size().y <= 0.0 {
            continue;
        }
        let width = host.content_box().size().x.max(0.0);
        let source = elide.full_text.as_str();
        let line_height = *line_height;
        let letter_spacing = *letter_spacing;
        let typography_revision = typography.revision;

        if !font_assets_changed {
            if let Some(cache) = elide.cache.as_ref() {
                if cache.matches(
                    source,
                    width,
                    font,
                    layout,
                    line_height,
                    letter_spacing,
                    target,
                    rem_size.0,
                    typography_revision,
                ) {
                    if text.0 != cache.rendered {
                        text.0.clone_from(&cache.rendered);
                    }
                    continue;
                }
            }
        }

        let full_layout_is_current = !font_assets_changed
            && !layout_info.is_added()
            && text.0 == source
            && layout_info.scale_factor == target.scale_factor()
            && elide.cache.as_ref().is_none_or(|cache| {
                cache.style_matches(
                    source,
                    font,
                    layout,
                    line_height,
                    letter_spacing,
                    target,
                    rem_size.0,
                    typography_revision,
                )
            });
        let rendered =
            if full_layout_is_current && layout_info.size.x * layout_info.scale_factor <= width {
                source.to_owned()
            } else {
                match elide_filename_middle_with_measure(source, width, |candidate| {
                    let mut computed = ComputedTextBlock::default();
                    text_pipeline
                        .create_text_measure(
                            entity,
                            &fonts,
                            std::iter::once((
                                entity,
                                0,
                                candidate,
                                font,
                                Color::WHITE,
                                line_height,
                                letter_spacing,
                            )),
                            target.scale_factor(),
                            layout,
                            &mut computed,
                            &mut font_cx,
                            &mut layout_cx,
                            target.logical_size(),
                            rem_size.0,
                        )
                        .map(|measure| measure.max.x)
                }) {
                    Ok(rendered) => rendered,
                    // Missing fonts and degenerate target state are transient in
                    // Bevy. Cache the safe fallback to avoid a per-row retry on
                    // every frame; an Assets<Font> change invalidates it above.
                    Err(_) => {
                        if width > 0.0 {
                            ELLIPSIS.to_owned()
                        } else {
                            String::new()
                        }
                    }
                }
            };

        let cached_source = source.to_owned();
        if text.0 != rendered {
            text.0.clone_from(&rendered);
        }
        elide.cache = Some(ElideCache {
            source: cached_source,
            width,
            font: font.clone(),
            justify: layout.justify,
            linebreak: layout.linebreak,
            line_height,
            letter_spacing,
            scale_factor: target.scale_factor(),
            physical_viewport: target.physical_size(),
            rem_size: rem_size.0,
            typography_revision,
            rendered,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grapheme_width(text: &str) -> Result<f32, ()> {
        Ok(text.graphemes(true).count() as f32 * 10.0)
    }

    #[test]
    fn elision_never_splits_grapheme_clusters() {
        let text = "e\u{301}e\u{301}e\u{301}-👨\u{200d}👩\u{200d}👧.png";
        let elided = elide_filename_middle_with_measure(text, 80.0, grapheme_width).unwrap();

        assert!(elided.starts_with("e\u{301}"));
        assert!(elided.ends_with(".png"));
        assert!(!elided.starts_with('\u{301}'));
        assert!(!elided.ends_with('\u{200d}'));
    }

    #[test]
    fn ordinary_extension_is_preserved_whole() {
        let text = "building-bare-metal-automation-system.md";
        let elided = elide_filename_middle_with_measure(text, 180.0, grapheme_width).unwrap();

        assert!(elided.contains(ELLIPSIS));
        assert!(elided.ends_with(".md"));
    }

    #[test]
    fn middle_elision_spends_the_whole_text_budget() {
        let text = format!("{}-final-report.xlsx", "w".repeat(400));
        let elided = elide_filename_middle_with_measure(&text, 210.0, grapheme_width).unwrap();

        assert_eq!(elided.graphemes(true).count(), 21);
        assert!(elided.ends_with(".xlsx"));
    }

    #[test]
    fn long_extension_keeps_its_dot_and_final_eleven_graphemes() {
        let text = "report.this-extension-is-far-too-long";
        let elided = elide_filename_middle_with_measure(text, 180.0, grapheme_width).unwrap();

        assert!(elided.contains(ELLIPSIS));
        assert!(elided.ends_with(".ar-too-long"));
        assert!(!elided.ends_with(".far-too-long"));
    }

    #[test]
    fn tiny_width_falls_back_to_ellipsis_then_empty() {
        assert_eq!(
            elide_filename_middle_with_measure("long-name.md", 10.0, grapheme_width).unwrap(),
            ELLIPSIS
        );
        assert_eq!(
            elide_filename_middle_with_measure("long-name.md", 9.0, grapheme_width).unwrap(),
            ""
        );
        assert_eq!(
            elide_filename_middle_with_measure("long-name.md", 0.0, grapheme_width).unwrap(),
            ""
        );
    }
}
