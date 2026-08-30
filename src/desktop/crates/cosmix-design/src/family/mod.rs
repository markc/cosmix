//! Closed family-schema registry.

pub mod button;

/// Stable identifier for a registered desktop family.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FamilyId {
    Button,
}

/// Stable identifier for a registered family part.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FamilyPart {
    Button(button::ButtonPart),
}

/// Plain schema metadata used by the compiler and introspection tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FamilySchema {
    pub id: FamilyId,
    pub name: &'static str,
    pub variants: &'static [&'static str],
    pub sizes: &'static [&'static str],
    pub parts: &'static [&'static str],
}

pub const FAMILY_SCHEMAS: &[FamilySchema] = &[button::SCHEMA];
