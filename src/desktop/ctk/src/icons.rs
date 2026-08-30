//! Bundled Lucide icon system for CTK apps: an [`Icon`] catalogue, an
//! [`IconSet`] resource, [`spawn_icon`], the [`file_icon`] path→icon heuristic,
//! and [`prepare_data_root`] which installs the bundled SVGs into the app's
//! cache dir (via [`crate::app_dirs`]) and returns the Bevy asset root.
//! Gated behind the `icons` feature; pulls `bevy_resvg` for SVG rasterisation.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock};

use bevy::feathers::theme::{ThemeToken, UiTheme};
use bevy::log::warn;
use bevy::prelude::*;
use bevy_resvg::resvg::{self, tiny_skia, usvg};

use crate::app_dirs::AppDirs;
use crate::dnd::{ExportIconRaster, ExportIconRasterError};

const EXPORT_LABEL_HEIGHT: u32 = 48;
const EXPORT_LABEL_MAX_WIDTH: u32 = 280;
const EXPORT_LABEL_TEXT_X: u32 = 52;
const EXPORT_LABEL_TEXT_MAX_WIDTH: f32 = 216.0;
const EXPORT_LABEL_RIGHT_PADDING: u32 = 12;
const EXPORT_LABEL_ICON_SIZE: u32 = 40;
const EXPORT_LABEL_ICON_INSET: u32 = 4;
const EXPORT_LABEL_ANCHOR: (u32, u32) = (24, 24);
const EXPORT_LABEL_MAX_CHARS: usize = 1024;

static EXPORT_LABEL_FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
static EXPORT_LABEL_FONT_WARM_STARTED: Once = Once::new();

// Re-export the bevy_resvg surface (used internally AND by apps), so a consumer
// enables ctk's `icons` feature and never depends on bevy_resvg directly.
pub use bevy_resvg::prelude::{SvgColor, SvgFile, SvgPlugin, UiSvg};

/// Theme token retained beside an SVG so runtime palette changes can re-tint
/// the already-spawned icon without respawning it.
#[derive(Component, Clone)]
pub struct ThemeSvgColor(pub ThemeToken);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Icon {
    Archive,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ChevronDown,
    ChevronRight,
    ChevronUp,
    Copy,
    Download,
    Eye,
    EyeOff,
    File,
    FileCode,
    FileImage,
    FileMusic,
    FileText,
    FileVideo,
    Folder,
    FolderOpen,
    Grid,
    HardDrive,
    House,
    Info,
    List,
    LogOut,
    Menu,
    MoveHorizontal,
    Music,
    PanelLeft,
    PanelRight,
    Pin,
    PinOff,
    Refresh,
    Search,
    Trash,
}

impl Icon {
    fn filename(self) -> &'static str {
        match self {
            Self::Archive => "archive.svg",
            Self::ArrowLeft => "arrow-left.svg",
            Self::ArrowRight => "arrow-right.svg",
            Self::ArrowUp => "arrow-up.svg",
            Self::ChevronDown => "chevron-down.svg",
            Self::ChevronRight => "chevron-right.svg",
            Self::ChevronUp => "chevron-up.svg",
            Self::Copy => "copy.svg",
            Self::Download => "download.svg",
            Self::Eye => "eye.svg",
            Self::EyeOff => "eye-off.svg",
            Self::File => "file.svg",
            Self::FileCode => "file-code.svg",
            Self::FileImage => "file-image.svg",
            Self::FileMusic => "file-music.svg",
            Self::FileText => "file-text.svg",
            Self::FileVideo => "file-video-camera.svg",
            Self::Folder => "folder.svg",
            Self::FolderOpen => "folder-open.svg",
            Self::Grid => "grid-2x2.svg",
            Self::HardDrive => "hard-drive.svg",
            Self::House => "house.svg",
            Self::Info => "info.svg",
            Self::List => "list.svg",
            Self::LogOut => "log-out.svg",
            Self::Menu => "menu.svg",
            Self::MoveHorizontal => "arrow-left-right.svg",
            Self::Music => "music.svg",
            Self::PanelLeft => "panel-left.svg",
            Self::PanelRight => "panel-right.svg",
            Self::Pin => "pin.svg",
            Self::PinOff => "pin-off.svg",
            Self::Refresh => "refresh-cw.svg",
            Self::Search => "search.svg",
            Self::Trash => "trash-2.svg",
        }
    }

    /// Stable catalogue key used by wire-facing consumers such as notify.v1.
    #[cfg(test)]
    fn catalogue_key(self) -> &'static str {
        match self {
            Self::FileVideo => "file-video",
            Self::Grid => "grid",
            Self::MoveHorizontal => "move-horizontal",
            Self::Refresh => "refresh",
            Self::Trash => "trash",
            _ => self
                .filename()
                .strip_suffix(".svg")
                .expect("CTK icon filenames end in .svg"),
        }
    }
}

const ICONS: &[Icon] = &[
    Icon::Archive,
    Icon::ArrowLeft,
    Icon::ArrowRight,
    Icon::ArrowUp,
    Icon::ChevronDown,
    Icon::ChevronRight,
    Icon::ChevronUp,
    Icon::Copy,
    Icon::Download,
    Icon::Eye,
    Icon::EyeOff,
    Icon::File,
    Icon::FileCode,
    Icon::FileImage,
    Icon::FileMusic,
    Icon::FileText,
    Icon::FileVideo,
    Icon::Folder,
    Icon::FolderOpen,
    Icon::Grid,
    Icon::HardDrive,
    Icon::House,
    Icon::Info,
    Icon::List,
    Icon::LogOut,
    Icon::Menu,
    Icon::MoveHorizontal,
    Icon::Music,
    Icon::PanelLeft,
    Icon::PanelRight,
    Icon::Pin,
    Icon::PinOff,
    Icon::Refresh,
    Icon::Search,
    Icon::Trash,
];

const BUNDLED: &[(&str, &[u8])] = &[
    ("archive.svg", include_bytes!("../assets/icons/archive.svg")),
    (
        "arrow-left.svg",
        include_bytes!("../assets/icons/arrow-left.svg"),
    ),
    (
        "arrow-right.svg",
        include_bytes!("../assets/icons/arrow-right.svg"),
    ),
    (
        "arrow-up.svg",
        include_bytes!("../assets/icons/arrow-up.svg"),
    ),
    (
        "chevron-down.svg",
        include_bytes!("../assets/icons/chevron-down.svg"),
    ),
    (
        "chevron-right.svg",
        include_bytes!("../assets/icons/chevron-right.svg"),
    ),
    (
        "chevron-up.svg",
        include_bytes!("../assets/icons/chevron-up.svg"),
    ),
    ("copy.svg", include_bytes!("../assets/icons/copy.svg")),
    (
        "download.svg",
        include_bytes!("../assets/icons/download.svg"),
    ),
    ("eye.svg", include_bytes!("../assets/icons/eye.svg")),
    ("eye-off.svg", include_bytes!("../assets/icons/eye-off.svg")),
    ("file.svg", include_bytes!("../assets/icons/file.svg")),
    (
        "file-code.svg",
        include_bytes!("../assets/icons/file-code.svg"),
    ),
    (
        "file-image.svg",
        include_bytes!("../assets/icons/file-image.svg"),
    ),
    (
        "file-music.svg",
        include_bytes!("../assets/icons/file-music.svg"),
    ),
    (
        "file-text.svg",
        include_bytes!("../assets/icons/file-text.svg"),
    ),
    (
        "file-video-camera.svg",
        include_bytes!("../assets/icons/file-video-camera.svg"),
    ),
    ("folder.svg", include_bytes!("../assets/icons/folder.svg")),
    (
        "folder-open.svg",
        include_bytes!("../assets/icons/folder-open.svg"),
    ),
    (
        "grid-2x2.svg",
        include_bytes!("../assets/icons/grid-2x2.svg"),
    ),
    (
        "hard-drive.svg",
        include_bytes!("../assets/icons/hard-drive.svg"),
    ),
    ("house.svg", include_bytes!("../assets/icons/house.svg")),
    ("info.svg", include_bytes!("../assets/icons/info.svg")),
    ("list.svg", include_bytes!("../assets/icons/list.svg")),
    ("log-out.svg", include_bytes!("../assets/icons/log-out.svg")),
    ("menu.svg", include_bytes!("../assets/icons/menu.svg")),
    (
        "arrow-left-right.svg",
        include_bytes!("../assets/icons/arrow-left-right.svg"),
    ),
    ("music.svg", include_bytes!("../assets/icons/music.svg")),
    (
        "panel-left.svg",
        include_bytes!("../assets/icons/panel-left.svg"),
    ),
    (
        "panel-right.svg",
        include_bytes!("../assets/icons/panel-right.svg"),
    ),
    ("pin.svg", include_bytes!("../assets/icons/pin.svg")),
    ("pin-off.svg", include_bytes!("../assets/icons/pin-off.svg")),
    (
        "refresh-cw.svg",
        include_bytes!("../assets/icons/refresh-cw.svg"),
    ),
    ("search.svg", include_bytes!("../assets/icons/search.svg")),
    ("trash-2.svg", include_bytes!("../assets/icons/trash-2.svg")),
    (
        "LUCIDE-LICENSE",
        include_bytes!("../assets/icons/LUCIDE-LICENSE"),
    ),
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RasterKey {
    icon: Icon,
    logical_size: u32,
    buffer_scale: i32,
}

struct RasterSource {
    asset_root: Option<PathBuf>,
    cache: Mutex<HashMap<RasterKey, Result<Arc<ExportIconRaster>, IconRasterError>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconRasterError(String);

impl fmt::Display for IconRasterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IconRasterError {}

/// Starts the one process-wide system-font scan used by labelled export icons.
///
/// Rasterisation still uses the same [`OnceLock`] synchronously, so a drag
/// blocks only when it beats this background warm-up.
pub fn warm_export_label_fonts() {
    EXPORT_LABEL_FONT_WARM_STARTED.call_once(|| {
        if let Err(error) = std::thread::Builder::new()
            .name("ctk-export-label-fonts".into())
            .spawn(|| {
                let _ = export_label_fonts();
            })
        {
            warn!(%error, "could not start CTK export-label font warm-up");
        }
    });
}

#[derive(Resource)]
pub struct IconSet {
    handles: HashMap<Icon, Handle<SvgFile>>,
    rasters: RasterSource,
}

impl IconSet {
    /// Placeholder-handle set so sibling modules can drive layout tests
    /// through [`spawn_icon`] without an asset server.
    #[cfg(test)]
    pub(crate) fn placeholder_for_test(icons: &[Icon]) -> Self {
        Self {
            handles: icons
                .iter()
                .map(|icon| (*icon, Handle::<SvgFile>::default()))
                .collect(),
            rasters: RasterSource {
                asset_root: None,
                cache: Mutex::new(HashMap::new()),
            },
        }
    }

    pub fn load(asset_server: &AssetServer) -> Self {
        Self::load_inner(asset_server, None)
    }

    /// Loads GPU handles and enables synchronous CPU rasters from the same
    /// installed asset root.
    pub fn load_with_rasters(asset_server: &AssetServer, asset_root: impl Into<PathBuf>) -> Self {
        Self::load_inner(asset_server, Some(asset_root.into()))
    }

    fn load_inner(asset_server: &AssetServer, asset_root: Option<PathBuf>) -> Self {
        let handles = ICONS
            .iter()
            .copied()
            .map(|icon| {
                let path = format!("icons/{}", icon.filename());
                (icon, asset_server.load(path))
            })
            .collect();
        Self {
            handles,
            rasters: RasterSource {
                asset_root,
                cache: Mutex::new(HashMap::new()),
            },
        }
    }

    pub fn handle(&self, icon: Icon) -> Handle<SvgFile> {
        self.handles
            .get(&icon)
            .expect("FileMgr icon catalogue is complete")
            .clone()
    }

    /// Returns a cached square raster at `logical_size * buffer_scale` pixels.
    ///
    /// SVG parsing and rendering happen on the first request for a key, during
    /// ordinary UI construction rather than at the drag threshold. Both
    /// successes and failures are retained for the life of this [`IconSet`]
    /// with no eviction; callers must keep the set of `(icon, logical_size,
    /// buffer_scale)` combinations bounded.
    pub fn raster(
        &self,
        icon: Icon,
        logical_size: u32,
        buffer_scale: i32,
    ) -> Result<Arc<ExportIconRaster>, IconRasterError> {
        let source = &self.rasters;
        let key = RasterKey {
            icon,
            logical_size,
            buffer_scale,
        };
        let mut cache = source
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(result) = cache.get(&key) {
            return result.clone();
        }

        // Keep the lock through the first attempt. Otherwise simultaneous
        // callers can both miss and repeat the same file I/O and warning.
        let result = rasterize(source, icon, logical_size, buffer_scale);
        cache.insert(key, result.clone());
        if let Err(error) = &result {
            warn!(
                ?icon,
                logical_size,
                buffer_scale,
                %error,
                "CPU icon raster is unavailable"
            );
        }
        result
    }
}

fn rasterize(
    source: &RasterSource,
    icon: Icon,
    logical_size: u32,
    buffer_scale: i32,
) -> Result<Arc<ExportIconRaster>, IconRasterError> {
    let asset_root = source.asset_root.as_ref().ok_or_else(|| {
        IconRasterError("IconSet was loaded without a CPU raster asset root".into())
    })?;
    let scale = u32::try_from(buffer_scale)
        .ok()
        .filter(|scale| *scale > 0)
        .ok_or_else(|| IconRasterError(format!("invalid Wayland buffer scale {buffer_scale}")))?;
    let physical_size = logical_size.checked_mul(scale).ok_or_else(|| {
        IconRasterError(format!(
            "icon size {logical_size} at scale {buffer_scale} overflows u32"
        ))
    })?;
    if physical_size == 0 {
        return Err(IconRasterError(
            "icon logical and physical dimensions must be non-zero".into(),
        ));
    }

    let path = asset_root.join("icons").join(icon.filename());
    let bytes = std::fs::read(&path).map_err(|error| {
        IconRasterError(format!("reading export icon {}: {error}", path.display()))
    })?;
    let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default()).map_err(|error| {
        IconRasterError(format!("parsing export icon {}: {error}", path.display()))
    })?;
    let mut pixmap = tiny_skia::Pixmap::new(physical_size, physical_size).ok_or_else(|| {
        IconRasterError(format!(
            "allocating {physical_size}x{physical_size} export icon"
        ))
    })?;
    let source_size = tree.size();
    let transform = tiny_skia::Transform::from_scale(
        physical_size as f32 / source_size.width(),
        physical_size as f32 / source_size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let raster = Arc::new(
        ExportIconRaster::new(pixmap.take(), physical_size, physical_size, buffer_scale)
            .map_err(icon_raster_contract_error)?,
    );
    Ok(raster)
}

/// Composes a filename pill around an existing cached 40-logical-pixel icon.
///
/// The square icon remains the shared catalogue raster. This function creates
/// one uncached wide raster for the active export gesture.
pub fn labelled_export_icon(
    icon: &ExportIconRaster,
    label: &str,
) -> Result<ExportIconRaster, IconRasterError> {
    let scale = u32::try_from(icon.buffer_scale())
        .ok()
        .filter(|scale| *scale > 0)
        .ok_or_else(|| {
            IconRasterError(format!(
                "invalid Wayland buffer scale {}",
                icon.buffer_scale()
            ))
        })?;
    let logical_icon_width = icon.width() / scale;
    let logical_icon_height = icon.height() / scale;
    if logical_icon_width != EXPORT_LABEL_ICON_SIZE || logical_icon_height != EXPORT_LABEL_ICON_SIZE
    {
        return Err(IconRasterError(format!(
            "labelled export icon requires a {EXPORT_LABEL_ICON_SIZE}x{EXPORT_LABEL_ICON_SIZE} \
             logical raster, got {logical_icon_width}x{logical_icon_height}"
        )));
    }

    let label = sanitise_export_label(label);
    let options = export_label_options();
    let label = truncate_export_label(&label, &options)?;
    let label_width = measure_export_label(&label, &options)?;
    let logical_width = EXPORT_LABEL_TEXT_X
        .checked_add(label_width.ceil() as u32)
        .and_then(|width| width.checked_add(EXPORT_LABEL_RIGHT_PADDING))
        .map(|width| width.min(EXPORT_LABEL_MAX_WIDTH))
        .ok_or_else(|| IconRasterError("labelled export icon width overflowed u32".into()))?;
    let physical_width = logical_width.checked_mul(scale).ok_or_else(|| {
        IconRasterError(format!(
            "labelled export icon width {logical_width} at scale {scale} overflows u32"
        ))
    })?;
    let physical_height = EXPORT_LABEL_HEIGHT.checked_mul(scale).ok_or_else(|| {
        IconRasterError(format!(
            "labelled export icon height {EXPORT_LABEL_HEIGHT} at scale {scale} overflows u32"
        ))
    })?;

    let escaped = escape_xml_text(&label);
    let pill_width = logical_width as f32 - 1.0;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{logical_width}" height="{EXPORT_LABEL_HEIGHT}" viewBox="0 0 {logical_width} {EXPORT_LABEL_HEIGHT}">
<rect x="0.5" y="0.5" width="{pill_width}" height="47" rx="10" fill="#111722" fill-opacity="0.90" stroke="#ffffff" stroke-opacity="0.25" stroke-width="1"/>
<text id="label" x="{EXPORT_LABEL_TEXT_X}" y="29" xml:space="preserve" font-family="sans-serif" font-size="14" font-weight="400" fill="#f5f7fa" fill-opacity="0.96" text-rendering="optimizeLegibility">{escaped}</text>
</svg>"##
    );
    let tree = usvg::Tree::from_data(svg.as_bytes(), &options)
        .map_err(|error| IconRasterError(format!("parsing labelled export icon SVG: {error}")))?;
    // Check containment on the tree that is about to be rendered, not on the
    // measurement pass. Two things make the earlier measurement insufficient on
    // its own. fontdb holds system fonts as file paths and reopens them on each
    // parse, so a font removed or atomically replaced between the two parses
    // measures one way and composes another — and the composition still parses,
    // because the pill rect survives, so a label-less or overflowing pill would
    // otherwise ship instead of the square-icon fallback. And the width the
    // measurement returns is a *metric* box: usvg computes it "not based on the
    // outlines of a glyph, but instead the glyph metrics" (usvg 0.47
    // `src/text/mod.rs`), so stacked combining marks can measure narrow and
    // still paint past the pill edge. `abs_stroke_bounding_box` is the box of
    // the flattened outlines, which is what actually gets rasterised.
    //
    // The bound is the pill itself, not the text budget. `logical_width`
    // already includes `EXPORT_LABEL_RIGHT_PADDING`, so that padding is exactly
    // the slack an outline needs to overshoot its own metric advance — which
    // ordinary diacritics do. Re-subtracting it here would leave zero tolerance
    // and refuse a filename as tame as `ĵŷÿ.tar.gz`. What actually matters is
    // that nothing paints outside the pill, and when the label is long enough
    // for `logical_width` to hit the `EXPORT_LABEL_MAX_WIDTH` clamp, this is
    // also what catches the genuine overflow.
    let outline = tree
        .node_by_id("label")
        .ok_or_else(|| IconRasterError("labelled export icon SVG produced no text node".into()))?
        .abs_stroke_bounding_box();
    if !outline.width().is_finite() || outline.width() <= 0.0 {
        return Err(IconRasterError(
            "labelled export icon text resolved to no visible outline".into(),
        ));
    }
    // The pill is stroked on the half-pixel, so its painted extent is the full
    // 0..logical_width by 0..EXPORT_LABEL_HEIGHT rectangle. Its rounded corners
    // sit within 10px of each end, and text starts at x=52, so for the corners
    // to matter the text would have to overflow the straight edges first.
    if outline.left() < 0.0
        || outline.right() > logical_width as f32
        || outline.top() < 0.0
        || outline.bottom() > EXPORT_LABEL_HEIGHT as f32
    {
        return Err(IconRasterError(format!(
            "labelled export icon text spans {}..{} by {}..{}, outside the {logical_width}x{EXPORT_LABEL_HEIGHT} pill",
            outline.left(),
            outline.right(),
            outline.top(),
            outline.bottom(),
        )));
    }

    let mut pixmap = tiny_skia::Pixmap::new(physical_width, physical_height).ok_or_else(|| {
        IconRasterError(format!(
            "allocating {physical_width}x{physical_height} labelled export icon"
        ))
    })?;
    let transform = tiny_skia::Transform::from_scale(scale as f32, scale as f32);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let source = tiny_skia::PixmapRef::from_bytes(icon.pixels(), icon.width(), icon.height())
        .ok_or_else(|| {
            IconRasterError("cached export icon pixels do not form a tiny-skia pixmap".into())
        })?;
    let icon_offset = EXPORT_LABEL_ICON_INSET
        .checked_mul(scale)
        .ok_or_else(|| IconRasterError("labelled export icon inset overflowed u32".into()))?;
    let icon_offset = i32::try_from(icon_offset)
        .map_err(|_| IconRasterError("labelled export icon inset exceeded i32".into()))?;
    pixmap.draw_pixmap(
        icon_offset,
        icon_offset,
        source,
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );

    Ok(ExportIconRaster::new(
        pixmap.take(),
        physical_width,
        physical_height,
        icon.buffer_scale(),
    )
    .map_err(icon_raster_contract_error)?
    .with_logical_anchor(EXPORT_LABEL_ANCHOR))
}

fn export_label_fonts() -> &'static Arc<usvg::fontdb::Database> {
    EXPORT_LABEL_FONTS.get_or_init(|| {
        let mut fonts = usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        Arc::new(fonts)
    })
}

fn export_label_options() -> usvg::Options<'static> {
    usvg::Options {
        font_family: "sans-serif".into(),
        fontdb: Arc::clone(export_label_fonts()),
        ..usvg::Options::default()
    }
}

fn sanitise_export_label(label: &str) -> String {
    let mut output = String::with_capacity(label.len().min(EXPORT_LABEL_MAX_CHARS));
    let mut chars = label.chars();
    for _ in 0..EXPORT_LABEL_MAX_CHARS {
        let Some(character) = chars.next() else {
            return output;
        };
        match character {
            '\t' => output.push('\u{2409}'),
            '\n' => output.push('\u{240A}'),
            '\r' => output.push('\u{240D}'),
            character if is_bidi_control(character) || is_invisible_format(character) => {
                output.push('\u{FFFD}');
            }
            character if character.is_control() || !is_xml_character(character) => {
                output.push('\u{FFFD}');
            }
            character => output.push(character),
        }
    }
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{206F}'
    )
}

/// Zero-width and format scalars that let two different filenames render as one
/// pill.
///
/// Deliberately narrower than "every default-ignorable scalar". Some invisible
/// scalars are *shaping* characters — they pick a glyph variant or bind a
/// sequence together — and are kept:
///
/// - the joiners U+200C and U+200D, without which ZWJ emoji shatter into their
///   parts and Persian and Indic joining behaviour breaks;
/// - the variation selectors U+FE00..=U+FE0F and the supplement
///   U+E0100..=U+E01EF, which choose the emoji-versus-text presentation and the
///   kanji variant an ideographic variation sequence selects;
/// - the Mongolian free variation selectors U+180B..=U+180D and the Mongolian
///   vowel separator U+180E, which is not an FVS but does both disconnect the
///   cluster and select the final vowel's form, so replacing it merges two
///   distinct Mongolian words;
/// - the emoji tag characters U+E0020..=U+E007F, which spell out subdivision
///   flags such as 🏴󠁧󠁢󠁷󠁬󠁳󠁿 after a base U+1F3F4.
///
/// Keeping them is a knowing trade, not an oversight. Between two Latin
/// characters a joiner usually has no ligature to form, so `a\u{200D}b` can
/// render exactly as `ab` and two distinct filenames can produce one pill. This
/// label is a drag icon for a file the user selected themselves in their own
/// file manager — nothing is authorised off the strength of it — whereas
/// mangled emoji, broken subdivision flags and mis-shaped Persian filenames
/// would be an everyday regression for people whose filenames contain them.
///
/// The scalars below have no shaping role and are replaced unconditionally.
fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{200B}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{2060}'..='\u{2065}'
            | '\u{FEFF}'
            | '\u{FFF0}'..='\u{FFFB}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E001F}'
            | '\u{E0080}'..='\u{E00FF}'
            | '\u{E01F0}'..='\u{E0FFF}'
    )
}

fn is_xml_character(character: char) -> bool {
    matches!(
        character,
        '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}'
    ) || ('\u{10000}'..='\u{10FFFF}').contains(&character)
}

fn escape_xml_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn truncate_export_label(
    label: &str,
    options: &usvg::Options<'_>,
) -> Result<String, IconRasterError> {
    crate::text_elide::elide_filename_middle_with_measure(
        label,
        EXPORT_LABEL_TEXT_MAX_WIDTH,
        |candidate| measure_export_label(candidate, options),
    )
}

fn measure_export_label(label: &str, options: &usvg::Options<'_>) -> Result<f32, IconRasterError> {
    let escaped = escape_xml_text(label);
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32768" height="{EXPORT_LABEL_HEIGHT}">
<text id="label" x="0" y="29" xml:space="preserve" font-family="sans-serif" font-size="14" font-weight="400" text-rendering="optimizeLegibility">{escaped}</text>
</svg>"#
    );
    let tree = usvg::Tree::from_data(svg.as_bytes(), options)
        .map_err(|error| IconRasterError(format!("measuring export label SVG: {error}")))?;
    if !tree.has_text_nodes() {
        return Err(IconRasterError(
            "export label SVG produced no measurable text node".into(),
        ));
    }
    let node = tree
        .node_by_id("label")
        .ok_or_else(|| IconRasterError("export label SVG lost its text node".into()))?;
    let width = node.abs_bounding_box().width();
    if !width.is_finite() || width <= 0.0 {
        return Err(IconRasterError(
            "export label SVG produced an invalid text width".into(),
        ));
    }
    Ok(width)
}

fn icon_raster_contract_error(error: ExportIconRasterError) -> IconRasterError {
    IconRasterError(format!("raster violated the CTK export contract: {error}"))
}

/// Spawn an SVG with its initial colour resolved from the live theme.
///
/// Install [`crate::theme::CtkThemePlugin`] in the app as well: the retained
/// [`ThemeSvgColor`] token lets that plugin re-tint this entity after runtime
/// theme changes.
pub fn spawn_icon(
    commands: &mut Commands,
    icons: &IconSet,
    theme: &UiTheme,
    icon: Icon,
    size: f32,
    colour: ThemeToken,
) -> Entity {
    commands
        .spawn((
            Node {
                width: px(size),
                min_width: px(size),
                height: px(size),
                ..default()
            },
            UiSvg(icons.handle(icon)),
            SvgColor(theme.color(&colour)),
            ThemeSvgColor(colour),
        ))
        .id()
}

pub(crate) fn retint_added_icons(
    theme: Res<UiTheme>,
    mut icons: Query<(&ThemeSvgColor, &mut SvgColor), Added<ThemeSvgColor>>,
) {
    for (token, mut colour) in &mut icons {
        colour.0 = theme.color(&token.0);
    }
}

pub(crate) fn retint_icons_on_theme_change(
    theme: Res<UiTheme>,
    state: Res<crate::theme::ThemeState>,
    mut icons: Query<(&ThemeSvgColor, &mut SvgColor)>,
) {
    if !state.is_changed() {
        return;
    }
    for (token, mut colour) in &mut icons {
        colour.0 = theme.color(&token.0);
    }
}

pub fn file_icon(path: &Path, is_dir: bool, expanded: bool) -> Icon {
    if is_dir {
        return if expanded {
            Icon::FolderOpen
        } else {
            Icon::Folder
        };
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mid" | "midi" | "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac") => {
            Icon::FileMusic
        }
        Some("mp4" | "mkv" | "webm" | "mov" | "avi" | "mpeg" | "mpg") => Icon::FileVideo,
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tiff") => Icon::FileImage,
        Some("rs" | "c" | "h" | "cpp" | "js" | "ts" | "html" | "css" | "sh" | "mix") => {
            Icon::FileCode
        }
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar") => Icon::Archive,
        Some("txt" | "md" | "pdf" | "doc" | "docx" | "odt" | "csv" | "toml" | "json") => {
            Icon::FileText
        }
        _ => Icon::File,
    }
}

/// Install the bundled icon subset under the app's cache dir
/// (`<app-root>/cache/icons`) and return the Bevy asset root (the cache dir,
/// so `asset_server.load("icons/foo.svg")` resolves). Assets are replaced
/// atomically when the bundled version changes — the cache is disposable and
/// self-healing.
pub fn prepare_data_root(dirs: &AppDirs) -> Result<PathBuf, String> {
    let root = dirs.cache();
    let icons = root.join("icons");
    std::fs::create_dir_all(&icons)
        .map_err(|error| format!("creating {}: {error}", icons.display()))?;
    for (name, bytes) in BUNDLED {
        let target = icons.join(name);
        if std::fs::read(&target).ok().as_deref() != Some(*bytes) {
            crate::fs::write_atomic(&target, bytes)?;
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RasterTestRoot(PathBuf);

    impl RasterTestRoot {
        fn new(name: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("ctk-icons-{name}-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(path.join("icons")).unwrap();
            Self(path)
        }
    }

    impl Drop for RasterTestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn raster_icon_set_at(asset_root: PathBuf) -> IconSet {
        IconSet {
            handles: HashMap::new(),
            rasters: RasterSource {
                asset_root: Some(asset_root),
                cache: Mutex::new(HashMap::new()),
            },
        }
    }

    fn raster_icon_set() -> IconSet {
        raster_icon_set_at(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"))
    }

    #[test]
    fn file_types_map_to_distinct_icons() {
        assert_eq!(
            file_icon(Path::new("song.flac"), false, false),
            Icon::FileMusic
        );
        assert_eq!(
            file_icon(Path::new("clip.webm"), false, false),
            Icon::FileVideo
        );
        assert_eq!(
            file_icon(Path::new("cover.png"), false, false),
            Icon::FileImage
        );
        assert_eq!(file_icon(Path::new("src.rs"), false, false), Icon::FileCode);
        assert_eq!(
            file_icon(Path::new("bundle.zip"), false, false),
            Icon::Archive
        );
        assert_eq!(file_icon(Path::new("folder"), true, true), Icon::FolderOpen);
    }

    #[test]
    fn raster_cache_reuses_an_arc_only_for_the_same_key() {
        let icons = raster_icon_set();
        let first = icons.raster(Icon::File, 40, 1).unwrap();
        let repeated = icons.raster(Icon::File, 40, 1).unwrap();
        let other_icon = icons.raster(Icon::Folder, 40, 1).unwrap();
        let other_size = icons.raster(Icon::File, 24, 1).unwrap();
        let other_scale = icons.raster(Icon::File, 40, 2).unwrap();

        assert!(Arc::ptr_eq(&first, &repeated));
        assert!(!Arc::ptr_eq(&first, &other_icon));
        assert!(!Arc::ptr_eq(&first, &other_size));
        assert!(!Arc::ptr_eq(&first, &other_scale));
        assert_eq!((other_scale.width(), other_scale.height()), (80, 80));
        assert_eq!(other_scale.buffer_scale(), 2);
    }

    #[test]
    fn raster_cache_retains_a_failure_without_rereading_the_file() {
        let root = RasterTestRoot::new("cached-failure");
        let icons = raster_icon_set_at(root.0.clone());
        let first = icons.raster(Icon::File, 40, 1).unwrap_err();

        std::fs::write(
            root.0.join("icons").join(Icon::File.filename()),
            include_bytes!("../assets/icons/file.svg"),
        )
        .unwrap();

        let repeated = icons.raster(Icon::File, 40, 1).unwrap_err();
        assert_eq!(repeated, first);
        assert_eq!(icons.rasters.cache.lock().unwrap().len(), 1);

        // A different key reads the now-present file, proving the repeated
        // failure came from the cache rather than another read.
        assert!(icons.raster(Icon::File, 24, 1).is_ok());
        assert_eq!(icons.raster(Icon::File, 40, 1).unwrap_err(), first);
    }

    #[test]
    fn export_label_sanitises_hostile_text_before_xml_escaping() {
        let hostile = "<&>\"'\t\n\r\u{0}\u{7f}\u{202e}\u{2066}😀";
        let sanitised = sanitise_export_label(hostile);
        assert_eq!(sanitised, "<&>\"'␉␊␍����😀");
        assert_eq!(
            escape_xml_text(&sanitised),
            "&lt;&amp;&gt;&quot;&apos;␉␊␍����😀"
        );
    }

    #[test]
    fn export_label_replaces_every_bidi_control_class() {
        let controls = [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{206a}', '\u{206f}',
        ];
        let hostile: String = controls.into_iter().collect();
        assert_eq!(sanitise_export_label(&hostile), "�".repeat(controls.len()));
    }

    #[test]
    fn export_label_replaces_invisible_format_scalars() {
        // Without this, `report.txt` and `report\u{200B}.txt` render as
        // identical pills, which defeats the point of showing the name.
        for hidden in [
            '\u{00ad}',
            '\u{200b}',
            '\u{2028}',
            '\u{2029}',
            '\u{034f}',
            '\u{2060}',
            '\u{2064}',
            '\u{2065}',
            '\u{feff}',
            '\u{fff0}',
            '\u{fff8}',
            '\u{fff9}',
            '\u{1bca0}',
            '\u{1bca3}',
            '\u{1d173}',
            '\u{e0001}',
            '\u{e001f}',
            '\u{e0080}',
            '\u{e00ff}',
            '\u{e01f0}',
            '\u{e0fff}',
        ] {
            assert_eq!(
                sanitise_export_label(&format!("a{hidden}b")),
                "a\u{FFFD}b",
                "U+{:04X} must not survive into the pill",
                hidden as u32
            );
        }

        // The shaping characters are a knowing exception, not an oversight —
        // see `is_invisible_format`. Pin them so the trade is a decision
        // someone has to revisit deliberately rather than drift.
        for kept in [
            '\u{200c}',
            '\u{200d}',
            '\u{fe0f}',
            '\u{180b}',
            '\u{180e}',
            '\u{e0020}',
            '\u{e007f}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            assert_eq!(
                sanitise_export_label(&format!("a{kept}b")),
                format!("a{kept}b"),
                "U+{:04X} is shaping and must survive into the pill",
                kept as u32
            );
        }
    }

    #[test]
    fn export_label_keeps_scalars_that_shape_their_neighbours() {
        // ZWJ holds a family emoji together, ZWNJ is required for Persian and
        // Indic text, and VS16 is what makes an emoji render in colour.
        for kept in ['\u{200c}', '\u{200d}', '\u{fe0f}'] {
            let label = format!("a{kept}b");
            assert_eq!(sanitise_export_label(&label), label);
        }
        let family = "👨\u{200d}👩\u{200d}👧.png";
        assert_eq!(sanitise_export_label(family), family);
    }

    #[test]
    fn export_label_defensive_cap_stays_on_character_boundaries() {
        let label = "😀".repeat(EXPORT_LABEL_MAX_CHARS + 10);
        let sanitised = sanitise_export_label(&label);
        assert_eq!(sanitised.chars().count(), EXPORT_LABEL_MAX_CHARS + 1);
        assert!(sanitised.ends_with('…'));
    }

    #[test]
    fn labelled_export_raster_satisfies_the_contract_at_common_scales() {
        for scale in [1, 2, 3] {
            let physical_icon_size = EXPORT_LABEL_ICON_SIZE * scale;
            let icon = ExportIconRaster::new(
                vec![0; physical_icon_size as usize * physical_icon_size as usize * 4],
                physical_icon_size,
                physical_icon_size,
                scale as i32,
            )
            .unwrap();
            let labelled = labelled_export_icon(&icon, "quarterly-report.final.xlsx").unwrap();

            assert_eq!(labelled.height(), EXPORT_LABEL_HEIGHT * scale);
            assert!(labelled.width() > physical_icon_size);
            assert!(labelled.width() <= EXPORT_LABEL_MAX_WIDTH * scale);
            assert_eq!(labelled.width() % scale, 0);
            assert_eq!(labelled.height() % scale, 0);
            assert_eq!(
                labelled.pixels().len(),
                labelled.width() as usize * labelled.height() as usize * 4
            );
            assert_eq!(labelled.buffer_scale(), scale as i32);
            assert_eq!(labelled.logical_anchor(), EXPORT_LABEL_ANCHOR);
            assert!(
                labelled.pixels().iter().any(|channel| *channel != 0),
                "pill and text raster was empty at scale {scale}"
            );
        }
    }

    #[test]
    fn labelled_export_accepts_labels_whose_outlines_exceed_their_metrics() {
        // The containment check bounds the *flattened outline* box, not the
        // metric box usvg returns from measurement. Combining marks stack above
        // the ascender and diacritics reach past the advance width, so an
        // over-tight bound would refuse them and silently drop every such
        // filename back to the bare square icon.
        let icon = ExportIconRaster::new(
            vec![0; EXPORT_LABEL_ICON_SIZE as usize * EXPORT_LABEL_ICON_SIZE as usize * 4],
            EXPORT_LABEL_ICON_SIZE,
            EXPORT_LABEL_ICON_SIZE,
            1,
        )
        .unwrap();

        for label in [
            "e\u{0301}te\u{0301}.txt",
            "a\u{0300}\u{0301}\u{0302}\u{0303}\u{0304}.txt",
            "Ẹ̀ọ̀kọ̀.pdf",
            "ĵŷÿ.tar.gz",
        ] {
            assert!(
                labelled_export_icon(&icon, label).is_ok(),
                "{label:?} must still produce a pill"
            );
        }
    }

    #[test]
    fn labelled_export_rejects_a_label_that_produces_no_text() {
        let icon = ExportIconRaster::new(
            vec![0; EXPORT_LABEL_ICON_SIZE as usize * EXPORT_LABEL_ICON_SIZE as usize * 4],
            EXPORT_LABEL_ICON_SIZE,
            EXPORT_LABEL_ICON_SIZE,
            1,
        )
        .unwrap();
        let error = labelled_export_icon(&icon, "").unwrap_err();
        assert!(error.to_string().contains("no measurable text node"));
    }

    #[test]
    fn file_icon_catalogue_choices_all_rasterise() {
        let icons = raster_icon_set();
        let cases = [
            (Path::new("folder"), true, false, Icon::Folder),
            (Path::new("photo.png"), false, false, Icon::FileImage),
            (Path::new("opaque.unknown"), false, false, Icon::File),
        ];

        for (path, is_dir, expanded, expected) in cases {
            let icon = file_icon(path, is_dir, expanded);
            assert_eq!(icon, expected);
            let raster = icons.raster(icon, 40, 2).unwrap();
            assert_eq!((raster.width(), raster.height()), (80, 80));
            assert_eq!(raster.pixels().len(), 80 * 80 * 4);
            assert!(
                raster.pixels().iter().any(|channel| *channel != 0),
                "{icon:?} raster was empty"
            );
        }
    }

    #[test]
    fn runtime_catalogue_matches_the_shared_wire_key_list() {
        let shared: Vec<_> = include_str!("../assets/icons/catalogue.txt")
            .lines()
            .filter(|line| !line.is_empty())
            .collect();
        let runtime: Vec<_> = ICONS.iter().map(|icon| icon.catalogue_key()).collect();
        assert_eq!(runtime, shared);
    }

    #[test]
    fn interactd_packaged_catalogue_copy_stays_in_sync() {
        let ctk = include_str!("../assets/icons/catalogue.txt");
        let interactd =
            include_str!("../../../src/crates/cosmix-interactd/src/lucide-catalogue.txt");
        assert_eq!(interactd, ctk);
    }

    #[test]
    fn spawned_icon_colour_tracks_runtime_theme_changes() {
        #[derive(Resource)]
        struct SpawnedIcon(Entity);

        let mut app = App::new();
        app.add_plugins(crate::theme::CtkThemePlugin::default());
        let initial = crate::theme::ThemeSpec::builtin();
        let mut theme = UiTheme::default();
        let mut state = crate::theme::ThemeState::default();
        crate::theme::apply_theme(&mut theme, &mut state, &initial);
        app.insert_resource(theme)
            .insert_resource(state)
            .insert_resource(IconSet {
                handles: HashMap::from([(Icon::Info, Handle::<SvgFile>::default())]),
                rasters: RasterSource {
                    asset_root: None,
                    cache: Mutex::new(HashMap::new()),
                },
            })
            .add_systems(
                Startup,
                |mut commands: Commands, icons: Res<IconSet>, theme: Res<UiTheme>| {
                    let icon = spawn_icon(
                        &mut commands,
                        &icons,
                        &theme,
                        Icon::Info,
                        16.0,
                        crate::theme::tokens::TEXT,
                    );
                    commands.insert_resource(SpawnedIcon(icon));
                },
            );
        app.update();
        let icon = app.world().resource::<SpawnedIcon>().0;
        assert_eq!(
            app.world().get::<SvgColor>(icon).unwrap().0,
            initial.colors.text,
            "spawn resolves the token before the first rendered frame"
        );

        let spec = crate::theme::ThemeSpec::from_scheme(
            crate::theme::Scheme::Sunset,
            crate::theme::Mode::Light,
        );
        app.world_mut()
            .write_message(crate::theme::ApplyTheme(spec.clone()));
        app.update();

        assert_eq!(
            app.world().get::<SvgColor>(icon).unwrap().0,
            spec.colors.text
        );
    }
}
