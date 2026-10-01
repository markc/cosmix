//! Installed Material Symbols Rounded, drawn through iced's native text path.
//! The pinned shared catalogue supplies glyph codepoints and the font family;
//! design tokens supply tint and logical size at the actual output scale.
//! A missing, broken or incomplete catalogue uses the retained Lucide SVGs.
//! That fallback replaces `currentColor` before parsing with pinned resvg,
//! rasterises off the UI thread and caches images by tint and physical size.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use resvg::{self, tiny_skia, usvg};

/// The catalogue, in step with `ctk/src/icons.rs`'s `Icon`.
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
    /// Stable semantic names in the installed Material Symbols catalogue.
    pub const fn material_name(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::ArrowLeft => "arrow_back",
            Self::ArrowRight => "arrow_forward",
            Self::ArrowUp => "arrow_upward",
            Self::ChevronDown => "expand_more",
            Self::ChevronRight => "chevron_right",
            Self::ChevronUp => "expand_less",
            Self::Copy => "content_copy",
            Self::Download => "download",
            Self::Eye => "visibility",
            Self::EyeOff => "visibility_off",
            Self::File => "draft",
            Self::FileCode => "code",
            Self::FileImage => "image",
            Self::FileMusic => "audio_file",
            Self::FileText => "description",
            Self::FileVideo => "video_file",
            Self::Folder => "folder",
            Self::FolderOpen => "folder_open",
            Self::Grid => "grid_view",
            Self::HardDrive => "hard_drive",
            Self::House => "home",
            Self::Info => "info",
            Self::List => "view_list",
            Self::LogOut => "logout",
            Self::Menu => "menu",
            Self::MoveHorizontal => "swap_horiz",
            Self::Music => "music_note",
            Self::PanelLeft => "left_panel_open",
            Self::PanelRight => "right_panel_open",
            Self::Pin => "push_pin",
            Self::PinOff => "keep_off",
            Self::Refresh => "refresh",
            Self::Search => "search",
            Self::Trash => "delete",
        }
    }

    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Archive => include_bytes!("../assets/icons/archive.svg"),
            Self::ArrowLeft => include_bytes!("../assets/icons/arrow-left.svg"),
            Self::ArrowRight => include_bytes!("../assets/icons/arrow-right.svg"),
            Self::ArrowUp => include_bytes!("../assets/icons/arrow-up.svg"),
            Self::ChevronDown => include_bytes!("../assets/icons/chevron-down.svg"),
            Self::ChevronRight => include_bytes!("../assets/icons/chevron-right.svg"),
            Self::ChevronUp => include_bytes!("../assets/icons/chevron-up.svg"),
            Self::Copy => include_bytes!("../assets/icons/copy.svg"),
            Self::Download => include_bytes!("../assets/icons/download.svg"),
            Self::Eye => include_bytes!("../assets/icons/eye.svg"),
            Self::EyeOff => include_bytes!("../assets/icons/eye-off.svg"),
            Self::File => include_bytes!("../assets/icons/file.svg"),
            Self::FileCode => include_bytes!("../assets/icons/file-code.svg"),
            Self::FileImage => include_bytes!("../assets/icons/file-image.svg"),
            Self::FileMusic => include_bytes!("../assets/icons/file-music.svg"),
            Self::FileText => include_bytes!("../assets/icons/file-text.svg"),
            Self::FileVideo => include_bytes!("../assets/icons/file-video-camera.svg"),
            Self::Folder => include_bytes!("../assets/icons/folder.svg"),
            Self::FolderOpen => include_bytes!("../assets/icons/folder-open.svg"),
            Self::Grid => include_bytes!("../assets/icons/grid-2x2.svg"),
            Self::HardDrive => include_bytes!("../assets/icons/hard-drive.svg"),
            Self::House => include_bytes!("../assets/icons/house.svg"),
            Self::Info => include_bytes!("../assets/icons/info.svg"),
            Self::List => include_bytes!("../assets/icons/list.svg"),
            Self::LogOut => include_bytes!("../assets/icons/log-out.svg"),
            Self::Menu => include_bytes!("../assets/icons/menu.svg"),
            Self::MoveHorizontal => include_bytes!("../assets/icons/arrow-left-right.svg"),
            Self::Music => include_bytes!("../assets/icons/music.svg"),
            Self::PanelLeft => include_bytes!("../assets/icons/panel-left.svg"),
            Self::PanelRight => include_bytes!("../assets/icons/panel-right.svg"),
            Self::Pin => include_bytes!("../assets/icons/pin.svg"),
            Self::PinOff => include_bytes!("../assets/icons/pin-off.svg"),
            Self::Refresh => include_bytes!("../assets/icons/refresh-cw.svg"),
            Self::Search => include_bytes!("../assets/icons/search.svg"),
            Self::Trash => include_bytes!("../assets/icons/trash-2.svg"),
        }
    }
}

/// All 35 icons (the row pane uses the folder/file subset; the rest stay for
/// P2/P3 chrome).
pub const ALL: [Icon; 35] = [
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

/// The `path → icon` heuristic, ported verbatim from
/// `ctk/src/icons.rs::file_icon` (keep in step).
pub fn file_icon(path: &std::path::Path, is_dir: bool, expanded: bool) -> Icon {
    if is_dir {
        return if expanded {
            Icon::FolderOpen
        } else {
            Icon::Folder
        };
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
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

/// The raster size, physical pixels: every icon is drawn at 16 logical px
/// and rasterised at ×2 (the default Wayland scale) and downscaled, so a
/// scale-1 draw is still crisp. One constant so every `get` site agrees.
pub const RASTER_PX: u32 = 16 * 2;

/// `Color` → `#rrggbb`, the form an SVG `currentColor` replacement needs.
pub fn hex(color: iced::Color) -> String {
    let channel = |c: f32| format!("{:02x}", (c.clamp(0.0, 1.0) * 255.0).round() as u8);
    format!(
        "#{}{}{}",
        channel(color.r),
        channel(color.g),
        channel(color.b)
    )
}

/// Cache key: icon, tint, logical pixels.
type Key = (Icon, String, u32);

#[derive(Default)]
struct State {
    cache: HashMap<Key, iced::widget::image::Handle>,
    /// The `(palette, size)` a rasterisation is running (or has run) for.
    ensured: Option<(Vec<String>, u32)>,
}

/// The shared icon cache. Clone the `Arc` into widgets; `get` never blocks on
/// the rasterisation thread (a single short mutex around map lookups).
#[derive(Clone)]
pub struct Icons {
    state: Arc<Mutex<State>>,
    material: Option<Arc<HashMap<Icon, (char, iced::Font)>>>,
    asset_set: Option<String>,
}

impl Default for Icons {
    fn default() -> Self {
        let mut icons = Self::lucide();
        let set = match cosmix_iced_widgets::fonts::register_installed() {
            Ok(Some(set)) => set,
            Ok(None) => return icons,
            Err(error) => {
                tracing::warn!(%error, "DOpus uses bundled Lucide icons");
                return icons;
            }
        };
        let mut glyphs = HashMap::new();
        for icon in ALL {
            match cosmix_iced_widgets::fonts::material_icon(icon.material_name()) {
                Ok(Some(glyph)) => {
                    glyphs.insert(icon, glyph);
                }
                Ok(None) => {
                    tracing::warn!(
                        name = icon.material_name(),
                        "Material catalogue incomplete; DOpus uses bundled Lucide icons"
                    );
                    return icons;
                }
                Err(error) => {
                    tracing::warn!(%error, "DOpus uses bundled Lucide icons");
                    return icons;
                }
            }
        }
        if let Err(error) = validate_material_glyphs(&glyphs) {
            tracing::warn!(%error, "Material face incomplete; DOpus uses bundled Lucide icons");
            return icons;
        }
        icons.material = Some(Arc::new(glyphs));
        icons.asset_set = Some(set.set_id().to_owned());
        tracing::info!(
            asset_set = set.set_id(),
            "DOpus uses Material Symbols Rounded icons"
        );
        icons
    }
}

/// Check actual selected-font coverage once at startup. A catalogue entry
/// alone does not prove a user-supplied font contains its mapped glyph.
fn validate_material_glyphs(glyphs: &HashMap<Icon, (char, iced::Font)>) -> Result<(), String> {
    use iced::advanced::graphics::text::{
        cosmic_text::fontdb::{Family, Query, Weight},
        font_system,
    };
    let (_, font) = glyphs.values().next().ok_or("empty Material catalogue")?;
    let iced::font::Family::Name(family) = font.family else {
        return Err("Material face must use a named family".into());
    };
    let mut system = font_system()
        .write()
        .map_err(|_| "font system lock poisoned")?;
    let raw = system.raw();
    let id = raw
        .db()
        .query(&Query {
            families: &[Family::Name(family)],
            ..Default::default()
        })
        .ok_or_else(|| format!("Material family {family:?} is unavailable"))?;
    let selected = raw
        .get_font(id, Weight::NORMAL)
        .ok_or_else(|| format!("Material family {family:?} cannot be read"))?;
    for (icon, (glyph, glyph_font)) in glyphs {
        if glyph_font.family != font.family || selected.as_swash().charmap().map(*glyph) == 0 {
            return Err(format!(
                "Material glyph {} is absent from {family:?}",
                icon.material_name()
            ));
        }
    }
    Ok(())
}

impl Icons {
    fn lucide() -> Self {
        Self {
            state: Default::default(),
            material: None,
            asset_set: None,
        }
    }

    pub fn mode(&self) -> &'static str {
        if self.material.is_some() {
            "material-symbols-rounded"
        } else {
            "lucide"
        }
    }

    pub fn asset_set(&self) -> Option<&str> {
        self.asset_set.as_deref()
    }

    pub fn glyph(&self, icon: Icon) -> Option<(char, iced::Font)> {
        self.material.as_ref()?.get(&icon).copied()
    }

    /// One icon draw path for custom file rows and drag previews. Native text
    /// keeps glyphs sharp at the actual output scale; the fallback uses the
    /// existing SVG raster cache.
    pub fn draw(
        &self,
        renderer: &mut iced_tiny_skia::Renderer,
        icon: Icon,
        tint: &str,
        bounds: iced::Rectangle,
        clip: iced::Rectangle,
    ) {
        use iced::advanced::{image::Renderer as _, text::Renderer as _};
        if let Some((glyph, font)) = self.glyph(icon) {
            renderer.fill_text(
                iced::advanced::text::Text {
                    content: glyph.to_string(),
                    bounds: bounds.size(),
                    size: iced::Pixels(bounds.height),
                    line_height: iced::advanced::text::LineHeight::Absolute(iced::Pixels(
                        bounds.height,
                    )),
                    font,
                    align_x: iced::advanced::text::Alignment::Center,
                    align_y: iced::alignment::Vertical::Center,
                    shaping: iced::advanced::text::Shaping::Advanced,
                    wrapping: iced::advanced::text::Wrapping::None,
                },
                bounds.center(),
                tint_color(tint),
                clip,
            );
        } else if let Some(handle) = self.get(icon, tint, RASTER_PX) {
            renderer.draw_image(iced::advanced::image::Image::new(handle), bounds, clip);
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Make sure the catalogue is rasterised for `(palette, px)`; if the current
    /// snapshot differs, spawn a std thread to rasterise all of [`ALL`] and
    /// fill the cache. Failures are logged and simply leave that icon absent
    /// (`get` returns `None`; the row draws nothing).
    pub fn ensure(&self, tints: &[&str], px: u32, scale: u32) {
        if self.material.is_some() {
            return;
        }
        let physical = px.saturating_mul(scale).max(1);
        let palette: Vec<String> = tints.iter().map(|tint| (*tint).to_owned()).collect();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.ensured.as_ref() == Some(&(palette.clone(), physical)) {
            return;
        }
        state.ensured = Some((palette.clone(), physical));
        // Retain both enabled and disabled roles for this theme only.
        state
            .cache
            .retain(|key, _| palette.contains(&key.1) && key.2 == physical);
        let icons = Arc::clone(&self.state);
        // Startup and re-tint rasterisation: off the UI thread, once per role.
        std::thread::Builder::new()
            .name("dopus-icons".to_owned())
            .spawn(move || {
                for tint in &palette {
                    for icon in ALL {
                        match raster(icon.bytes(), tint, physical) {
                            Ok(handle) => {
                                let mut state = icons
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                // A newer theme may have replaced this worker's palette.
                                if state.ensured.as_ref() != Some(&(palette.clone(), physical)) {
                                    return;
                                }
                                state.cache.insert((icon, tint.clone(), physical), handle);
                            }
                            Err(error) => tracing::warn!(?icon, %error, "icon raster unavailable"),
                        }
                    }
                }
            })
            .expect("spawning the icon raster thread");
    }

    /// The cached handle for `(icon, tint, px)`, or `None` while the
    /// rasterisation is still in flight (the row draws nothing).
    pub fn get(&self, icon: Icon, tint: &str, px: u32) -> Option<iced::widget::image::Handle> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cache.get(&(icon, tint.to_owned(), px)).cloned()
    }
}

pub fn tint_color(tint: &str) -> iced::Color {
    let channel = |range| u8::from_str_radix(tint.get(range).unwrap_or("00"), 16).unwrap_or(0);
    iced::Color::from_rgb8(channel(1..3), channel(3..5), channel(5..7))
}

/// Rasterise one SVG at `px` physical pixels, tinted. `Err` names the icon
/// file so the warning is actionable.
fn raster(bytes: &[u8], tint: &str, px: u32) -> Result<iced::widget::image::Handle, String> {
    render(bytes, tint, px)
        .map(|pixmap| iced::widget::image::Handle::from_rgba(px, px, pixmap.take()))
}

/// The raster itself, split from [`raster`] so tests inspect pixels without
/// reaching into `Handle`'s internals.
fn render(bytes: &[u8], tint: &str, px: u32) -> Result<tiny_skia::Pixmap, String> {
    // Lucide icons draw with `stroke="currentColor"`; substitute the token
    // hex before parsing — the only place a colour enters an icon.
    let tinted = std::str::from_utf8(bytes)
        .map_err(|error| format!("svg is not utf-8: {error}"))?
        .replace("currentColor", tint);
    let tree = usvg::Tree::from_data(tinted.as_bytes(), &usvg::Options::default())
        .map_err(|error| format!("parsing svg: {error}"))?;
    let mut pixmap =
        tiny_skia::Pixmap::new(px, px).ok_or_else(|| format!("allocating {px}x{px} icon"))?;
    let source = tree.size();
    let transform =
        tiny_skia::Transform::from_scale(px as f32 / source.width(), px as f32 / source.height());
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_mapping_covers_distinct_semantic_actions() {
        let names: std::collections::HashSet<_> =
            ALL.into_iter().map(Icon::material_name).collect();
        assert_eq!(names.len(), ALL.len());
        assert_eq!(Icon::Trash.material_name(), "delete");
        assert_eq!(Icon::ArrowLeft.material_name(), "arrow_back");
        assert_eq!(Icon::ArrowRight.material_name(), "arrow_forward");
        assert_ne!(Icon::Eye.material_name(), Icon::EyeOff.material_name());
        assert_ne!(
            Icon::Folder.material_name(),
            Icon::FolderOpen.material_name()
        );
    }

    #[test]
    #[ignore = "requires a bootstrapped static asset set"]
    fn installed_material_catalogue_draws_tinted_glyphs_at_output_scales() {
        let icons = Icons::new();
        assert_eq!(icons.mode(), "material-symbols-rounded");
        assert!(icons.asset_set().is_some());
        let mut damaged = icons.material.as_ref().unwrap().as_ref().clone();
        damaged.get_mut(&Icon::Trash).unwrap().0 = '\u{10ffff}';
        assert!(
            validate_material_glyphs(&damaged)
                .unwrap_err()
                .contains("delete"),
            "incomplete font coverage must refuse Material mode"
        );
        let bounds = iced::Rectangle {
            x: 8.0,
            y: 8.0,
            width: 16.0,
            height: 16.0,
        };
        for scale in [1.0, 2.0] {
            let side = (32.0 * scale) as u32;
            let viewport = iced_tiny_skia::graphics::Viewport::with_physical_size(
                iced::Size::new(side, side),
                scale,
            );
            for icon in ALL {
                let (glyph, font) = icons.glyph(icon).unwrap();
                let iced::font::Family::Name(family) = font.family else {
                    panic!("Material must use its named family");
                };
                {
                    use iced::advanced::graphics::text::{
                        cosmic_text::fontdb::{Family, Query, Weight},
                        font_system,
                    };
                    let mut system = font_system().write().unwrap();
                    let raw = system.raw();
                    let id = raw
                        .db()
                        .query(&Query {
                            families: &[Family::Name(family)],
                            ..Default::default()
                        })
                        .unwrap();
                    let selected = raw.get_font(id, Weight::NORMAL).unwrap();
                    assert_ne!(
                        selected.as_swash().charmap().map(glyph),
                        0,
                        "{icon:?}: glyph missing from selected Material face"
                    );
                }
                let mut renderer =
                    iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(16.0));
                icons.draw(&mut renderer, icon, "#ff0000", bounds, bounds);
                let text_count: usize = renderer
                    .layers()
                    .iter()
                    .flat_map(|layer| layer.text.iter())
                    .map(|group| group.as_slice().len())
                    .sum();
                assert_eq!(text_count, 1, "{icon:?}: native text, not image fallback");
                let mut pixels = iced_tiny_skia_pixels::Pixmap::new(side, side).unwrap();
                let mut mask = iced_tiny_skia_pixels::Mask::new(side, side).unwrap();
                renderer.draw(
                    &mut pixels.as_mut(),
                    &mut mask,
                    &viewport,
                    &[iced::Rectangle::with_size(iced::Size::new(32.0, 32.0))],
                    iced::Color::TRANSPARENT,
                );
                let ink: Vec<_> = pixels
                    .data()
                    .chunks_exact(4)
                    .filter(|pixel| pixel[3] != 0)
                    .collect();
                assert!(!ink.is_empty(), "{icon:?}: no ink at scale {scale}");
                assert!(
                    ink.iter()
                        .all(|pixel| pixel[0] > 0 && pixel[1] == 0 && pixel[2] == 0),
                    "{icon:?}: tint differs at scale {scale}"
                );
            }
        }
        icons.ensure(&["#ff0000"], 16, 2);
        assert!(
            icons.state.lock().unwrap().ensured.is_none(),
            "Material bypasses SVG raster workers"
        );
    }

    #[test]
    fn file_types_map_to_distinct_icons() {
        let p = std::path::Path::new;
        assert_eq!(file_icon(p("notes.txt"), false, false), Icon::FileText);
        assert_eq!(file_icon(p("song.FLAC"), false, false), Icon::FileMusic);
        assert_eq!(file_icon(p("clip.mkv"), false, false), Icon::FileVideo);
        assert_eq!(file_icon(p("shot.PNG"), false, false), Icon::FileImage);
        assert_eq!(file_icon(p("main.rs"), false, false), Icon::FileCode);
        assert_eq!(file_icon(p("lib.tar.gz"), false, false), Icon::Archive);
        assert_eq!(file_icon(p("README"), false, false), Icon::File);
        assert_eq!(file_icon(p("src"), true, false), Icon::Folder);
        assert_eq!(file_icon(p("src"), true, true), Icon::FolderOpen);
        assert_eq!(
            file_icon(p("src"), false, true),
            Icon::File,
            "expanded only means folders"
        );
    }

    #[test]
    fn hex_renders_eight_bit_channels() {
        assert_eq!(hex(iced::Color::from_rgb8(0x12, 0xfe, 0x03)), "#12fe03");
        assert_eq!(hex(iced::Color::BLACK), "#000000");
        assert_eq!(hex(iced::Color::WHITE), "#ffffff");
    }

    #[test]
    fn tint_replaces_current_color_and_rasterises() {
        // A real bundled icon, tinted red: the ink must actually be red. A
        // white-ink render (an asset missing `currentColor`) or a no-op tint
        // fails here.
        let pixmap =
            render(Icon::Folder.bytes(), "#ff0000", RASTER_PX).expect("folder.svg rasterises");
        assert_eq!((pixmap.width(), pixmap.height()), (RASTER_PX, RASTER_PX));
        let ink = pixmap
            .data()
            .chunks(4)
            .filter(|px| px[3] > 0)
            .find(|px| px[0] > 0)
            .expect("red ink present");
        assert_eq!(
            (ink[1], ink[2]),
            (0, 0),
            "ink is the requested tint, not white"
        );
    }

    #[test]
    fn cache_is_empty_until_the_raster_thread_fills_it() {
        let icons = Icons::lucide();
        assert!(icons.get(Icon::Folder, "#ffffff", 16).is_none());
        // ensure() runs the rasterisation on its own thread; poll briefly.
        icons.ensure(&["#ffffff", "#888888"], 16, 1);
        let key = (Icon::Folder, "#ffffff".to_owned(), 16);
        for _ in 0..200 {
            {
                let state = icons.state.lock().unwrap();
                if state.cache.contains_key(&key)
                    && state
                        .cache
                        .contains_key(&(Icon::Folder, "#888888".to_owned(), 16))
                {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the raster thread never produced folder.svg");
    }

    #[test]
    fn ensure_is_idempotent_for_the_same_tint() {
        let icons = Icons::lucide();
        icons.ensure(&["#ffffff"], 16, 1);
        let ensured = icons.state.lock().unwrap().ensured.clone();
        icons.ensure(&["#ffffff"], 16, 1);
        assert_eq!(icons.state.lock().unwrap().ensured, ensured);
    }
}
