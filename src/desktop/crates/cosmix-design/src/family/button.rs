use super::{FamilyId, FamilySchema};
use crate::axis::closed_axis;

closed_axis! {
    deserialized
    /// The semantic colour treatment of a button.
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    pub enum ButtonVariant {
        #[default]
        Default => "default",
        Primary => "primary",
        Destructive => "destructive",
        Ghost => "ghost",
    }
}

closed_axis! {
    deserialized
    /// The compact desktop size scale of a button.
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    pub enum ButtonSize {
        Sm => "sm",
        #[default]
        Md => "md",
        Lg => "lg",
    }
}

closed_axis! {
    deserialized
    /// Entity-structural parts of the button family.
    #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    pub enum ButtonPart {
        Root => "root",
        Label => "label",
    }
}

pub const SCHEMA: FamilySchema = FamilySchema {
    id: FamilyId::Button,
    name: "button",
    variants: &ButtonVariant::NAMES,
    sizes: &ButtonSize::NAMES,
    parts: &ButtonPart::NAMES,
};

#[cfg(test)]
mod tests {
    use super::*;

    // `index()` is the table coordinate and `ALL` is the order the compiler
    // fills tables in; the macro derives both from one list, and this holds it
    // to that. It would have caught the hand-written mirrors this replaced.
    #[test]
    fn indexes_are_each_variants_position_in_all() {
        for (position, variant) in ButtonVariant::ALL.into_iter().enumerate() {
            assert_eq!(variant.index(), position);
            assert_eq!(variant.name(), ButtonVariant::NAMES[position]);
        }
        for (position, size) in ButtonSize::ALL.into_iter().enumerate() {
            assert_eq!(size.index(), position);
            assert_eq!(size.name(), ButtonSize::NAMES[position]);
        }
        for (position, part) in ButtonPart::ALL.into_iter().enumerate() {
            assert_eq!(part.index(), position);
            assert_eq!(part.name(), ButtonPart::NAMES[position]);
        }
    }

    // These names are a wire contract: they are what a theme file writes in a
    // selector, so renaming one breaks every design source already authored
    // against it. Deriving them from the enum removed the drift between the
    // schema and the parser but not this, because both sides move together —
    // renaming `md` here silently renames it in the published schema too, and
    // no other test noticed. Changing a string below is a source-format break.
    #[test]
    fn the_authored_axis_vocabulary_is_pinned() {
        assert_eq!(
            ButtonVariant::NAMES,
            ["default", "primary", "destructive", "ghost"]
        );
        assert_eq!(ButtonSize::NAMES, ["sm", "md", "lg"]);
        assert_eq!(ButtonPart::NAMES, ["root", "label"]);
    }

    // The schema is what a source author reads; publishing a vocabulary the
    // compiler does not accept is the drift the generated names exist to stop.
    #[test]
    fn the_published_schema_names_every_axis_value() {
        assert_eq!(SCHEMA.variants.len(), ButtonVariant::ALL.len());
        assert_eq!(SCHEMA.sizes.len(), ButtonSize::ALL.len());
        assert_eq!(SCHEMA.parts.len(), ButtonPart::ALL.len());
    }
}
