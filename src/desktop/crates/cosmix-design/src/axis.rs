//! Closed axes of the table key space.
//!
//! A family table is indexed by the product of several closed enums, so each
//! one has to supply four things that must agree exactly: the variants, an
//! iteration order (`ALL`), a table coordinate per variant (`index`), and an
//! authored name (`name`, which is also the wire spelling). Writing those out
//! by hand gives four places to edit and three ways to be silently wrong — a
//! coordinate that collides two cells, an `ALL` missing a variant so the axis
//! quietly shrinks, or a name that drifts from what the schema publishes.
//!
//! `closed_axis!` takes the list once and derives all four, which is why there
//! are no consistency assertions here: nothing can disagree with itself.

/// Generates `ALL`, `NAMES`, `name()` and `index()` from the variant list.
macro_rules! closed_axis_impl {
    ($name:ident { $($variant:ident => $label:literal),+ }) => {
        impl $name {
            /// Every variant, in table-coordinate order.
            pub const ALL: [Self; [$(stringify!($variant)),+].len()] = [$(Self::$variant),+];

            /// The authored names, in the same order as [`Self::ALL`].
            pub const NAMES: [&'static str; [$(stringify!($variant)),+].len()] = [$($label),+];

            /// The authored name of this variant, as spelled in a design source.
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $label,)+
                }
            }

            /// This variant's table coordinate: its position in [`Self::ALL`].
            pub const fn index(self) -> usize {
                let mut position = 0;
                $(
                    if let Self::$variant = self {
                        return position;
                    }
                    position += 1;
                )+
                position
            }
        }
    };
}

/// Declares a closed axis enum from a single variant list.
///
/// Prefix the declaration with `deserialized` when the axis is authored in a
/// design source; that arm adds the `compiler`-gated `Deserialize` derive and
/// renames each variant to its authored name, so the wire spelling and the
/// generated `name()` cannot disagree either.
macro_rules! closed_axis {
    (
        deserialized
        $(#[$enum_meta:meta])*
        $visibility:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $label:literal),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[cfg_attr(feature = "compiler", derive(serde::Deserialize))]
        $visibility enum $name {
            $(
                $(#[$variant_meta])*
                #[cfg_attr(feature = "compiler", serde(rename = $label))]
                $variant,
            )+
        }

        $crate::axis::closed_axis_impl!($name { $($variant => $label),+ });
    };
    (
        $(#[$enum_meta:meta])*
        $visibility:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $label:literal),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        $visibility enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        $crate::axis::closed_axis_impl!($name { $($variant => $label),+ });
    };
}

pub(crate) use closed_axis;
pub(crate) use closed_axis_impl;
