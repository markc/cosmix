use crate::colour_model::{FocusRingProvenance, LinearRgba, ResolvedPair};
use crate::recipe::DerivationRecipe;
use crate::{ButtonPart, ButtonSize, ButtonVariant, InteractionState};

/// Focus-visible is a boolean axis rather than an enum, so it contributes two.
const FOCUS_VISIBLE_COUNT: usize = 2;

pub const BUTTON_CELL_COUNT: usize = ButtonVariant::ALL.len()
    * ButtonSize::ALL.len()
    * InteractionState::ALL.len()
    * FOCUS_VISIBLE_COUNT;
pub const BUTTON_TYPOGRAPHY_COUNT: usize =
    ButtonVariant::ALL.len() * ButtonSize::ALL.len() * ButtonPart::ALL.len();

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ButtonProperty {
    Pair,
    Border,
    Ring,
    Height,
    MinWidth,
    PaddingX,
    BorderWidth,
    Radius,
    Typography,
}

impl ButtonProperty {
    pub const ALL: [Self; 9] = [
        Self::Pair,
        Self::Border,
        Self::Ring,
        Self::Height,
        Self::MinWidth,
        Self::PaddingX,
        Self::BorderWidth,
        Self::Radius,
        Self::Typography,
    ];
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ButtonCellKey {
    pub variant: ButtonVariant,
    pub size: ButtonSize,
    pub interaction: InteractionState,
    pub focus_visible: bool,
}

impl ButtonCellKey {
    const fn index(self) -> usize {
        (((self.variant.index() * ButtonSize::ALL.len() + self.size.index())
            * InteractionState::ALL.len()
            + self.interaction.index())
            * FOCUS_VISIBLE_COUNT)
            + self.focus_visible as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ButtonTypographyKey {
    pub variant: ButtonVariant,
    pub size: ButtonSize,
    pub part: ButtonPart,
}

impl ButtonTypographyKey {
    const fn index(self) -> usize {
        (self.variant.index() * ButtonSize::ALL.len() + self.size.index()) * ButtonPart::ALL.len()
            + self.part.index()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTypeRecord {
    pub family: String,
    pub font_size_metric: String,
    pub font_size: f64,
    pub weight: u16,
    pub line_height: Option<f64>,
}

/// Which named type record a `(variant, size, part)` coordinate resolves to.
///
/// This deliberately stores the name alone. The record itself lives once, in
/// `ResolvedTypography::scale`; copying it here would make the assignment a
/// second mutable authority that can silently disagree with the scale.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTypographyAssignment {
    pub record_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedButtonCell {
    pub pair_name: String,
    pub pair: ResolvedPair,
    pub pair_recipe: Option<DerivationRecipe>,
    pub border_name: Option<String>,
    pub border: Option<LinearRgba>,
    pub ring_name: Option<String>,
    pub ring: Option<LinearRgba>,
    pub ring_recipe: Option<DerivationRecipe>,
    pub ring_provenance: Option<FocusRingProvenance>,
    pub height: f64,
    pub min_width: f64,
    pub padding_x: f64,
    pub border_width: f64,
    pub radius: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedButtonTable {
    cells: Vec<ResolvedButtonCell>,
}

impl ResolvedButtonTable {
    #[cfg(feature = "compiler")]
    pub(crate) fn new(cells: Vec<ResolvedButtonCell>) -> Self {
        assert_eq!(
            cells.len(),
            BUTTON_CELL_COUNT,
            "a resolved button table must be total over the closed key space"
        );
        Self { cells }
    }

    pub fn cell(&self, key: ButtonCellKey) -> &ResolvedButtonCell {
        &self.cells[key.index()]
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ButtonTypographyTable {
    assignments: Vec<ResolvedTypographyAssignment>,
}

impl ButtonTypographyTable {
    #[cfg(feature = "compiler")]
    pub(crate) fn new(assignments: Vec<ResolvedTypographyAssignment>) -> Self {
        assert_eq!(
            assignments.len(),
            BUTTON_TYPOGRAPHY_COUNT,
            "a button typography table must be total over the closed key space"
        );
        Self { assignments }
    }

    pub fn assignment(&self, key: ButtonTypographyKey) -> &ResolvedTypographyAssignment {
        &self.assignments[key.index()]
    }

    pub fn len(&self) -> usize {
        self.assignments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }
}

#[cfg(all(test, feature = "compiler"))]
mod tests {
    use super::*;

    // Both constructors are crate-private and production compilation always
    // supplies total vectors. These direct tests keep their assertions
    // falsifiable until another in-crate constructor exists.
    #[test]
    #[should_panic(expected = "total over the closed key space")]
    fn a_short_button_table_is_refused_at_construction() {
        let _ = ResolvedButtonTable::new(Vec::new());
    }

    #[test]
    #[should_panic(expected = "total over the closed key space")]
    fn a_short_button_typography_table_is_refused_at_construction() {
        let _ = ButtonTypographyTable::new(Vec::new());
    }
}
