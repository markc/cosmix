//! Bundled Lucide icons, ported from `ctk/src/icons.rs` (the same SVGs, the
//! same [`file_icon`] heuristic) but rasterised by the app itself: dopus is
//! iced + `resvg =0.47.0` (the pinned version that shares the lock entry with
//! `bevy_resvg` — never 0.45, and iced's `svg` feature stays forbidden).
//!
//! Icons are tinted by replacing `currentColor` in the SVG bytes with a
//! token hex *before* parsing — no colour literal survives into rendering.
//! Rasterisation happens off the UI thread ([`Icons::ensure`] spawns a std
//! thread for the whole catalogue at one `(tint, size)`); rows draw the
//! cached [`Handle`] or nothing while the first rasterisation is in flight.
//! A theme change re-tints by calling [`Icons::ensure`] again with the new
//! token hex.

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
        return if expanded { Icon::FolderOpen } else { Icon::Folder };
    }
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("mid" | "midi" | "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac") => Icon::FileMusic,
        Some("mp4" | "mkv" | "webm" | "mov" | "avi" | "mpeg" | "mpg") => Icon::FileVideo,
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tiff") => Icon::FileImage,
        Some("rs" | "c" | "h" | "cpp" | "js" | "ts" | "html" | "css" | "sh" | "mix") => Icon::FileCode,
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar") => Icon::Archive,
        Some("txt" | "md" | "pdf" | "doc" | "docx" | "odt" | "csv" | "toml" | "json") => Icon::FileText,
        _ => Icon::File,
    }
}

/// `Color` → `#rrggbb`, the form an SVG `currentColor` replacement needs.
pub fn hex(color: iced::Color) -> String {
    let channel = |c: f32| format!("{:02x}", (c.clamp(0.0, 1.0) * 255.0).round() as u8);
    format!("#{}{}{}", channel(color.r), channel(color.g), channel(color.b))
}

/// Cache key: icon, tint, logical pixels.
type Key = (Icon, String, u32);

#[derive(Default)]
struct State {
    cache: HashMap<Key, iced::widget::image::Handle>,
    /// The `(tint, size)` a rasterisation is running (or has run) for.
    ensured: Option<(String, u32)>,
}

/// The shared icon cache. Clone the `Arc` into widgets; `get` never blocks on
/// the rasterisation thread (a single short mutex around map lookups).
#[derive(Clone, Default)]
pub struct Icons {
    state: Arc<Mutex<State>>,
}

impl Icons {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make sure the catalogue is rasterised for `(tint, px)`; if the current
    /// snapshot differs, spawn a std thread to rasterise all of [`ALL`] and
    /// fill the cache. Failures are logged and simply leave that icon absent
    /// (`get` returns `None`; the row draws nothing).
    pub fn ensure(&self, tint: &str, px: u32, scale: u32) {
        let physical = px.saturating_mul(scale).max(1);
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.ensured.as_ref() == Some(&(tint.to_owned(), physical)) {
            return;
        }
        state.ensured = Some((tint.to_owned(), physical));
        let icons = Arc::clone(&self.state);
        let tint = tint.to_owned();
        // Startup and re-tint rasterisation: off the UI thread (35 SVG parses).
        std::thread::Builder::new()
            .name("dopus-icons".to_owned())
            .spawn(move || {
                for icon in ALL {
                    match raster(icon.bytes(), &tint, physical) {
                        Ok(handle) => {
                            let mut state = icons.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.cache.insert((icon, tint.clone(), physical), handle);
                        }
                        Err(error) => tracing::warn!(?icon, %error, "icon raster unavailable"),
                    }
                }
            })
            .expect("spawning the icon raster thread");
    }

    /// The cached handle for `(icon, tint, px)`, or `None` while the
    /// rasterisation is still in flight (the row draws nothing).
    pub fn get(&self, icon: Icon, tint: &str, px: u32) -> Option<iced::widget::image::Handle> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cache.get(&(icon, tint.to_owned(), px)).cloned()
    }
}

/// Rasterise one SVG at `px` physical pixels, tinted. `Err` names the icon
/// file so the warning is actionable.
fn raster(bytes: &[u8], tint: &str, px: u32) -> Result<iced::widget::image::Handle, String> {
    render(bytes, tint, px).map(|pixmap| iced::widget::image::Handle::from_rgba(px, px, pixmap.take()))
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
    let mut pixmap = tiny_skia::Pixmap::new(px, px).ok_or_else(|| format!("allocating {px}x{px} icon"))?;
    let source = tree.size();
    let transform = tiny_skia::Transform::from_scale(px as f32 / source.width(), px as f32 / source.height());
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(file_icon(p("src"), false, true), Icon::File, "expanded only means folders");
    }

    #[test]
    fn hex_renders_eight_bit_channels() {
        assert_eq!(hex(iced::Color::from_rgb8(0x12, 0xfe, 0x03)), "#12fe03");
        assert_eq!(hex(iced::Color::BLACK), "#000000");
        assert_eq!(hex(iced::Color::WHITE), "#ffffff");
    }

    #[test]
    fn tint_replaces_current_color_and_rasterises() {
        // A real bundled icon, tinted red, renders non-transparent pixels.
        let pixmap = render(Icon::Folder.bytes(), "#ff0000", 16).expect("folder.svg rasterises");
        assert_eq!((pixmap.width(), pixmap.height()), (16, 16));
        let pixels = pixmap.data();
        assert!(pixels.chunks(4).any(|px| px[0] > 0 && px[3] > 0), "red ink present");
    }

    #[test]
    fn cache_is_empty_until_the_raster_thread_fills_it() {
        let icons = Icons::new();
        assert!(icons.get(Icon::Folder, "#ffffff", 16).is_none());
        // ensure() runs the rasterisation on its own thread; poll briefly.
        icons.ensure("#ffffff", 16, 1);
        let key = (Icon::Folder, "#ffffff".to_owned(), 16);
        for _ in 0..200 {
            {
                let state = icons.state.lock().unwrap();
                if state.cache.contains_key(&key) {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the raster thread never produced folder.svg");
    }

    #[test]
    fn ensure_is_idempotent_for_the_same_tint() {
        let icons = Icons::new();
        icons.ensure("#ffffff", 16, 1);
        let ensured = icons.state.lock().unwrap().ensured.clone();
        icons.ensure("#ffffff", 16, 1);
        assert_eq!(icons.state.lock().unwrap().ensured, ensured);
    }
}
