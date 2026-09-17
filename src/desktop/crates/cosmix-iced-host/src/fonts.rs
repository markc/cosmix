//! Font loading into iced's process-wide text system.
//!
//! iced keeps one global cosmic-text `FontSystem`. Its first use scans the
//! system font directories (fontdb, fontconfig-configured paths) and keeps
//! face metadata for every installed face; font files are memory-mapped, so
//! the resident cost is the metadata plus the pages glyph shaping touches.
//! That scan cannot be disabled through iced 0.14's public API. Fonts loaded
//! here are copied into the database and never released. Load fonts before
//! building surfaces: text already shaped is not re-shaped on load.

use std::borrow::Cow;
use std::io;
use std::path::Path;

/// Loads font bytes (TTF/OTF/TTC). Loading the same `&'static` slice twice
/// is a no-op.
pub fn load_font(bytes: impl Into<Cow<'static, [u8]>>) {
    iced_graphics::text::font_system()
        .write()
        .expect("iced font system lock")
        .load_font(bytes.into());
}

/// Reads a font file and loads it with [`load_font`].
pub fn load_font_file(path: impl AsRef<Path>) -> io::Result<()> {
    let bytes = std::fs::read(path)?;
    load_font(bytes);
    Ok(())
}

/// Number of faces the text system knows, system faces included.
pub fn loaded_face_count() -> usize {
    iced_graphics::text::font_system()
        .write()
        .expect("iced font system lock")
        .raw()
        .db()
        .len()
}
