use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use cosmic::app::Settings;
use cosmic::iced::futures::SinkExt;
use cosmic::iced::stream;
use cosmic::iced::widget::image as iced_image;
use cosmic::iced::{self, Length, Size, Subscription};
use cosmic::iced_widget::Action as CanvasAction;
use cosmic::widget::canvas::{self, Cache, Frame, Geometry, Path as CPath, Stroke, Text as CText};
use cosmic::widget::menu::{self, key_bind::Modifier, ItemHeight, ItemWidth, KeyBind};
use cosmic::widget::{button, column, icon, row, text, text_input, Canvas};
use cosmic::{executor, Core, Element};

type PortRx = Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<cosmix_port::PortEvent>>>;
static PORT_RX: OnceLock<PortRx> = OnceLock::new();
use image::{DynamicImage, GenericImageView, RgbaImage};

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif", "svg"];
const VIEWABLE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif", "svg", "md", "markdown",
];

static MENU_ID: LazyLock<iced::id::Id> = LazyLock::new(|| iced::id::Id::new("view_menu"));

// ── Annotations ──

#[derive(Debug, Clone, Copy, PartialEq)]
enum AnnotationTool {
    Select,
    Arrow,
    Rect,
    Circle,
    Text,
    Freehand,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AnnotationStyle {
    color: iced::Color,
    width: f32,
}

impl Default for AnnotationStyle {
    fn default() -> Self {
        Self {
            color: iced::Color::from_rgb(1.0, 0.0, 0.0),
            width: 3.0,
        }
    }
}

#[derive(Debug, Clone)]
enum Annotation {
    Arrow {
        from: iced::Point,
        to: iced::Point,
        style: AnnotationStyle,
    },
    Rect {
        from: iced::Point,
        to: iced::Point,
        style: AnnotationStyle,
    },
    Circle {
        center: iced::Point,
        radius: f32,
        style: AnnotationStyle,
    },
    Text {
        position: iced::Point,
        content: String,
        color: iced::Color,
        size: f32,
    },
    Freehand {
        points: Vec<iced::Point>,
        style: AnnotationStyle,
    },
}

// ── Predefined Colors ──

const COLORS: &[(iced::Color, &str)] = &[
    (iced::Color::from_rgb(1.0, 0.0, 0.0), "Red"),
    (iced::Color::from_rgb(0.0, 0.4, 1.0), "Blue"),
    (iced::Color::from_rgb(0.0, 0.8, 0.0), "Green"),
    (iced::Color::from_rgb(1.0, 0.85, 0.0), "Yellow"),
    (iced::Color::WHITE, "White"),
    (iced::Color::BLACK, "Black"),
];

const LINE_WIDTHS: &[(f32, &str)] = &[(2.0, "Thin"), (4.0, "Medium"), (6.0, "Thick")];

// ── Menu Actions ──

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MenuAction {
    Open,
    Save,
    SaveAs,
    CopyClipboard,
    Quit,
    RotateLeft,
    RotateRight,
    FlipH,
    FlipV,
    Crop,
    Scale,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    Prev,
    Next,
    ScreenFull,
    ScreenInteractive,
    Undo,
    ClearAnnotations,
    ToggleExif,
    ToggleGallery,
    SetWallpaper,
    About,
    RunScript(usize),
    RescanScripts,
}

impl menu::Action for MenuAction {
    type Message = Msg;

    fn message(&self) -> Msg {
        match self {
            MenuAction::Open => Msg::Open,
            MenuAction::Save => Msg::Save,
            MenuAction::SaveAs => Msg::SaveAs,
            MenuAction::CopyClipboard => Msg::CopyClipboard,
            MenuAction::Quit => Msg::Quit,
            MenuAction::RotateLeft => Msg::RotateLeft,
            MenuAction::RotateRight => Msg::RotateRight,
            MenuAction::FlipH => Msg::FlipH,
            MenuAction::FlipV => Msg::FlipV,
            MenuAction::Crop => Msg::ToggleCrop,
            MenuAction::Scale => Msg::ToggleScale,
            MenuAction::ZoomIn => Msg::ZoomIn,
            MenuAction::ZoomOut => Msg::ZoomOut,
            MenuAction::ZoomFit => Msg::ZoomFit,
            MenuAction::Prev => Msg::Prev,
            MenuAction::Next => Msg::Next,
            MenuAction::ScreenFull => Msg::ScreenFull,
            MenuAction::ScreenInteractive => Msg::ScreenInteractive,
            MenuAction::Undo => Msg::Undo,
            MenuAction::ClearAnnotations => Msg::ClearAnnotations,
            MenuAction::ToggleExif => Msg::ToggleExif,
            MenuAction::ToggleGallery => Msg::ToggleGallery,
            MenuAction::SetWallpaper => Msg::SetWallpaper,
            MenuAction::About => Msg::About,
            MenuAction::RunScript(i) => Msg::RunScript(*i),
            MenuAction::RescanScripts => Msg::RescanScripts,
        }
    }
}

fn key_binds() -> HashMap<KeyBind, MenuAction> {
    use iced::keyboard::{key::Named, Key};

    let mut kb = HashMap::new();
    macro_rules! bind {
        ([$($m:ident),+], $key:expr, $action:ident) => {
            kb.insert(KeyBind { modifiers: vec![$(Modifier::$m),+], key: $key }, MenuAction::$action);
        };
        ([], $key:expr, $action:ident) => {
            kb.insert(KeyBind { modifiers: vec![], key: $key }, MenuAction::$action);
        };
    }

    bind!([Ctrl], Key::Character("o".into()), Open);
    bind!([Ctrl], Key::Character("s".into()), Save);
    bind!([Ctrl, Shift], Key::Character("s".into()), SaveAs);
    bind!([Ctrl], Key::Character("c".into()), CopyClipboard);
    bind!([Ctrl], Key::Character("q".into()), Quit);
    bind!([Ctrl], Key::Character("=".into()), ZoomIn);
    bind!([Ctrl], Key::Character("+".into()), ZoomIn);
    bind!([Ctrl], Key::Character("-".into()), ZoomOut);
    bind!([Ctrl], Key::Character("0".into()), ZoomFit);
    bind!([Ctrl], Key::Character("z".into()), Undo);
    bind!([], Key::Named(Named::ArrowLeft), Prev);
    bind!([], Key::Named(Named::ArrowRight), Next);
    bind!([Ctrl], Key::Character("e".into()), ToggleExif);
    bind!([Ctrl], Key::Character("g".into()), ToggleGallery);

    kb
}

// ── Image Engine ──

struct ViewEngine {
    image: Option<DynamicImage>,
    path: Option<PathBuf>,
    dir_images: Vec<PathBuf>,
    dir_index: usize,
    modified: bool,
}

impl ViewEngine {
    fn new() -> Self {
        Self {
            image: None,
            path: None,
            dir_images: Vec::new(),
            dir_index: 0,
            modified: false,
        }
    }

    fn open(&mut self, path: &Path) -> anyhow::Result<()> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        let img = if ext == "svg" {
            render_svg(path)?
        } else {
            image::open(path)?
        };

        self.image = Some(img);
        self.path = Some(path.to_path_buf());
        self.modified = false;
        self.scan_directory();
        Ok(())
    }

    fn scan_directory(&mut self) {
        let Some(path) = &self.path else { return };
        let Some(dir) = path.parent() else { return };
        let mut images: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| VIEWABLE_EXTS.contains(&ext.to_lowercase().as_str()))
            })
            .map(|e| e.path())
            .collect();
        images.sort();
        self.dir_index = images.iter().position(|p| p == path).unwrap_or(0);
        self.dir_images = images;
    }

    fn navigate(&mut self, delta: i32) -> anyhow::Result<()> {
        if self.dir_images.is_empty() {
            anyhow::bail!("No images in directory");
        }
        let len = self.dir_images.len() as i32;
        let new_idx = ((self.dir_index as i32 + delta) % len + len) % len;
        let path = self.dir_images[new_idx as usize].clone();
        self.open(&path)
    }

    fn rotate_cw(&mut self) {
        if let Some(img) = &mut self.image {
            *img = img.rotate90();
            self.modified = true;
        }
    }

    fn rotate_ccw(&mut self) {
        if let Some(img) = &mut self.image {
            *img = img.rotate270();
            self.modified = true;
        }
    }

    fn flip_h(&mut self) {
        if let Some(img) = &mut self.image {
            *img = img.fliph();
            self.modified = true;
        }
    }

    fn flip_v(&mut self) {
        if let Some(img) = &mut self.image {
            *img = img.flipv();
            self.modified = true;
        }
    }

    fn crop(&mut self, x: u32, y: u32, w: u32, h: u32) -> anyhow::Result<()> {
        let img = self
            .image
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("No image"))?;
        let (iw, ih) = img.dimensions();
        if x + w > iw || y + h > ih {
            anyhow::bail!("Crop out of bounds: image {iw}x{ih}, crop {x},{y} {w}x{h}");
        }
        *img = img.crop_imm(x, y, w, h);
        self.modified = true;
        Ok(())
    }

    fn scale(&mut self, w: u32, h: u32) {
        if let Some(img) = &mut self.image {
            *img = img.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
            self.modified = true;
        }
    }

    fn save(&self, path: &Path, quality: u8) -> anyhow::Result<()> {
        let img = self
            .image
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No image"))?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        match ext.as_str() {
            "jpg" | "jpeg" => {
                let mut file = std::fs::File::create(path)?;
                let encoder =
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, quality);
                img.write_with_encoder(encoder)?;
            }
            _ => {
                img.save(path)?;
            }
        }
        Ok(())
    }

    fn dimensions(&self) -> Option<(u32, u32)> {
        self.image.as_ref().map(|img| img.dimensions())
    }

    fn filename(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("No image")
            .to_string()
    }

    fn to_handle(&self) -> Option<iced_image::Handle> {
        let img = self.image.as_ref()?;
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        Some(iced_image::Handle::from_rgba(w, h, rgba.into_raw()))
    }

    fn to_rgba(&self) -> Option<RgbaImage> {
        self.image.as_ref().map(|img| img.to_rgba8())
    }

    fn nav_info(&self) -> String {
        if self.dir_images.is_empty() {
            String::new()
        } else {
            format!("{}/{}", self.dir_index + 1, self.dir_images.len())
        }
    }

    fn file_size(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| {
                let bytes = m.len();
                if bytes < 1024 {
                    format!("{bytes} B")
                } else if bytes < 1024 * 1024 {
                    format!("{:.1} KB", bytes as f64 / 1024.0)
                } else {
                    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
                }
            })
            .unwrap_or_default()
    }
}

// ── Canvas Program ──

struct AnnotationCanvas<'a> {
    handle: Option<&'a iced_image::Handle>,
    annotations: &'a [Annotation],
    tool: AnnotationTool,
    style: AnnotationStyle,
    img_w: u32,
    img_h: u32,
    zoom: f32,
}

#[derive(Default)]
struct CanvasState {
    drag_start: Option<iced::Point>,
    drag_current: Option<iced::Point>,
    freehand_points: Vec<iced::Point>,
    is_dragging: bool,
}

fn image_rect(frame_size: iced::Size, img_w: u32, img_h: u32, zoom: f32) -> iced::Rectangle {
    let scale = if zoom == 1.0 {
        (frame_size.width / img_w as f32).min(frame_size.height / img_h as f32)
    } else {
        zoom
    };
    let w = img_w as f32 * scale;
    let h = img_h as f32 * scale;
    let x = (frame_size.width - w) / 2.0;
    let y = (frame_size.height - h) / 2.0;
    iced::Rectangle::new(iced::Point::new(x, y), iced::Size::new(w, h))
}

fn canvas_to_image(
    point: iced::Point,
    img_rect: iced::Rectangle,
    img_w: u32,
    img_h: u32,
) -> Option<iced::Point> {
    let x = (point.x - img_rect.x) / img_rect.width * img_w as f32;
    let y = (point.y - img_rect.y) / img_rect.height * img_h as f32;
    if x >= 0.0 && x <= img_w as f32 && y >= 0.0 && y <= img_h as f32 {
        Some(iced::Point::new(x, y))
    } else {
        None
    }
}

fn image_to_canvas(
    point: iced::Point,
    img_rect: iced::Rectangle,
    img_w: u32,
    img_h: u32,
) -> iced::Point {
    let x = img_rect.x + point.x / img_w as f32 * img_rect.width;
    let y = img_rect.y + point.y / img_h as f32 * img_rect.height;
    iced::Point::new(x, y)
}

fn draw_annotation(
    frame: &mut Frame,
    ann: &Annotation,
    img_rect: iced::Rectangle,
    img_w: u32,
    img_h: u32,
) {
    match ann {
        Annotation::Arrow { from, to, style } => {
            let p1 = image_to_canvas(*from, img_rect, img_w, img_h);
            let p2 = image_to_canvas(*to, img_rect, img_w, img_h);
            let stroke = Stroke::default()
                .with_color(style.color)
                .with_width(style.width);
            // Shaft
            frame.stroke(&CPath::line(p1, p2), stroke);
            // Arrowhead
            let dx = p2.x - p1.x;
            let dy = p2.y - p1.y;
            let len = (dx * dx + dy * dy).sqrt();
            if len > 0.0 {
                let head_len = (style.width * 4.0).max(12.0);
                let head_w = head_len * 0.5;
                let ux = dx / len;
                let uy = dy / len;
                let base_x = p2.x - ux * head_len;
                let base_y = p2.y - uy * head_len;
                let left = iced::Point::new(base_x - uy * head_w, base_y + ux * head_w);
                let right = iced::Point::new(base_x + uy * head_w, base_y - ux * head_w);
                let head = CPath::new(|b| {
                    b.move_to(p2);
                    b.line_to(left);
                    b.line_to(right);
                    b.close();
                });
                frame.fill(&head, style.color);
            }
        }
        Annotation::Rect { from, to, style } => {
            let p1 = image_to_canvas(*from, img_rect, img_w, img_h);
            let p2 = image_to_canvas(*to, img_rect, img_w, img_h);
            let top_left = iced::Point::new(p1.x.min(p2.x), p1.y.min(p2.y));
            let size = iced::Size::new((p2.x - p1.x).abs(), (p2.y - p1.y).abs());
            let stroke = Stroke::default()
                .with_color(style.color)
                .with_width(style.width);
            frame.stroke(&CPath::rectangle(top_left, size), stroke);
        }
        Annotation::Circle {
            center,
            radius,
            style,
        } => {
            let c = image_to_canvas(*center, img_rect, img_w, img_h);
            let scale = img_rect.width / img_w as f32;
            let r = radius * scale;
            let stroke = Stroke::default()
                .with_color(style.color)
                .with_width(style.width);
            frame.stroke(&CPath::circle(c, r), stroke);
        }
        Annotation::Text {
            position,
            content,
            color,
            size,
        } => {
            let p = image_to_canvas(*position, img_rect, img_w, img_h);
            let scale = img_rect.width / img_w as f32;
            frame.fill_text(CText {
                content: content.clone(),
                position: p,
                color: *color,
                size: iced::Pixels(*size * scale),
                ..CText::default()
            });
        }
        Annotation::Freehand { points, style } => {
            if points.len() < 2 {
                return;
            }
            let stroke = Stroke::default()
                .with_color(style.color)
                .with_width(style.width)
                .with_line_cap(canvas::LineCap::Round)
                .with_line_join(canvas::LineJoin::Round);
            let path = CPath::new(|b| {
                let first = image_to_canvas(points[0], img_rect, img_w, img_h);
                b.move_to(first);
                for pt in &points[1..] {
                    let p = image_to_canvas(*pt, img_rect, img_w, img_h);
                    b.line_to(p);
                }
            });
            frame.stroke(&path, stroke);
        }
    }
}

impl<'a> canvas::Program<Msg, cosmic::Theme> for AnnotationCanvas<'a> {
    type State = CanvasState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &canvas::Event,
        bounds: iced::Rectangle,
        cursor: iced::mouse::Cursor,
    ) -> Option<CanvasAction<Msg>> {
        if self.tool == AnnotationTool::Select || self.img_w == 0 || self.img_h == 0 {
            return None;
        }

        let cursor_pos = cursor.position_in(bounds)?;
        let ir = image_rect(bounds.size(), self.img_w, self.img_h, self.zoom);
        let img_pt = canvas_to_image(cursor_pos, ir, self.img_w, self.img_h)?;

        match event {
            canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(
                iced::mouse::Button::Left,
            )) => {
                state.is_dragging = true;
                state.drag_start = Some(img_pt);
                state.drag_current = Some(img_pt);
                state.freehand_points.clear();
                if self.tool == AnnotationTool::Freehand {
                    state.freehand_points.push(img_pt);
                }
                Some(CanvasAction::capture())
            }
            canvas::Event::Mouse(iced::mouse::Event::CursorMoved { .. }) => {
                if !state.is_dragging {
                    return None;
                }
                state.drag_current = Some(img_pt);
                if self.tool == AnnotationTool::Freehand {
                    state.freehand_points.push(img_pt);
                }
                Some(CanvasAction::request_redraw().and_capture())
            }
            canvas::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )) => {
                if !state.is_dragging {
                    return None;
                }
                state.is_dragging = false;
                let start = state.drag_start.take()?;
                let end = state.drag_current.take()?;

                let ann = match self.tool {
                    AnnotationTool::Arrow => Annotation::Arrow {
                        from: start,
                        to: end,
                        style: self.style,
                    },
                    AnnotationTool::Rect => Annotation::Rect {
                        from: start,
                        to: end,
                        style: self.style,
                    },
                    AnnotationTool::Circle => {
                        let dx = end.x - start.x;
                        let dy = end.y - start.y;
                        let radius = (dx * dx + dy * dy).sqrt();
                        Annotation::Circle {
                            center: start,
                            radius,
                            style: self.style,
                        }
                    }
                    AnnotationTool::Text => Annotation::Text {
                        position: start,
                        content: String::new(), // filled by message handler
                        color: self.style.color,
                        size: 24.0,
                    },
                    AnnotationTool::Freehand => {
                        let pts = std::mem::take(&mut state.freehand_points);
                        Annotation::Freehand {
                            points: pts,
                            style: self.style,
                        }
                    }
                    AnnotationTool::Select => return None,
                };

                Some(CanvasAction::publish(Msg::AddAnnotation(ann)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &cosmic::Renderer,
        _theme: &cosmic::Theme,
        bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<Geometry<cosmic::Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());

        // Draw image
        if let Some(handle) = self.handle {
            if self.img_w > 0 && self.img_h > 0 {
                let ir = image_rect(bounds.size(), self.img_w, self.img_h, self.zoom);
                let img = canvas::Image::new(handle.clone());
                frame.draw_image(ir, img);

                // Draw completed annotations
                for ann in self.annotations {
                    draw_annotation(&mut frame, ann, ir, self.img_w, self.img_h);
                }

                // Draw in-progress annotation
                if state.is_dragging {
                    if let (Some(start), Some(current)) = (state.drag_start, state.drag_current) {
                        let preview = match self.tool {
                            AnnotationTool::Arrow => Some(Annotation::Arrow {
                                from: start,
                                to: current,
                                style: self.style,
                            }),
                            AnnotationTool::Rect => Some(Annotation::Rect {
                                from: start,
                                to: current,
                                style: self.style,
                            }),
                            AnnotationTool::Circle => {
                                let dx = current.x - start.x;
                                let dy = current.y - start.y;
                                let radius = (dx * dx + dy * dy).sqrt();
                                Some(Annotation::Circle {
                                    center: start,
                                    radius,
                                    style: self.style,
                                })
                            }
                            AnnotationTool::Freehand if state.freehand_points.len() >= 2 => {
                                Some(Annotation::Freehand {
                                    points: state.freehand_points.clone(),
                                    style: self.style,
                                })
                            }
                            _ => None,
                        };
                        if let Some(ann) = preview {
                            draw_annotation(&mut frame, &ann, ir, self.img_w, self.img_h);
                        }
                    }
                }
            }
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        _bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> iced::mouse::Interaction {
        match self.tool {
            AnnotationTool::Select => iced::mouse::Interaction::default(),
            _ => iced::mouse::Interaction::Crosshair,
        }
    }
}

// ── Flatten Annotations onto Image ──

fn flatten_annotations(base: &RgbaImage, annotations: &[Annotation]) -> RgbaImage {
    use imageproc::drawing::{
        draw_hollow_circle_mut, draw_hollow_rect_mut,
        draw_line_segment_mut, draw_text_mut,
    };
    use imageproc::rect::Rect;

    let mut img = base.clone();
    let to_rgba = |c: iced::Color| -> image::Rgba<u8> {
        image::Rgba([
            (c.r * 255.0) as u8,
            (c.g * 255.0) as u8,
            (c.b * 255.0) as u8,
            (c.a * 255.0) as u8,
        ])
    };

    for ann in annotations {
        match ann {
            Annotation::Arrow { from, to, style } => {
                let color = to_rgba(style.color);
                draw_line_segment_mut(
                    &mut img,
                    (from.x, from.y),
                    (to.x, to.y),
                    color,
                );
                // Arrowhead
                let dx = to.x - from.x;
                let dy = to.y - from.y;
                let len = (dx * dx + dy * dy).sqrt();
                if len > 0.0 {
                    let head_len = (style.width * 4.0).max(12.0);
                    let head_w = head_len * 0.5;
                    let ux = dx / len;
                    let uy = dy / len;
                    let base_x = to.x - ux * head_len;
                    let base_y = to.y - uy * head_len;
                    let left = (base_x - uy * head_w, base_y + ux * head_w);
                    let right = (base_x + uy * head_w, base_y - ux * head_w);
                    draw_line_segment_mut(&mut img, (to.x, to.y), left, color);
                    draw_line_segment_mut(&mut img, (to.x, to.y), right, color);
                    draw_line_segment_mut(&mut img, left, right, color);
                }
            }
            Annotation::Rect { from, to, style } => {
                let x1 = from.x.min(to.x) as i32;
                let y1 = from.y.min(to.y) as i32;
                let w = (to.x - from.x).abs() as u32;
                let h = (to.y - from.y).abs() as u32;
                if w > 0 && h > 0 {
                    let rect = Rect::at(x1, y1).of_size(w, h);
                    draw_hollow_rect_mut(&mut img, rect, to_rgba(style.color));
                }
            }
            Annotation::Circle {
                center,
                radius,
                style,
            } => {
                draw_hollow_circle_mut(
                    &mut img,
                    (center.x as i32, center.y as i32),
                    *radius as i32,
                    to_rgba(style.color),
                );
            }
            Annotation::Text {
                position,
                content,
                color,
                size,
            } => {
                let font =
                    ab_glyph::FontArc::try_from_slice(include_bytes!("/usr/share/fonts/noto/NotoSans-Regular.ttf"))
                        .unwrap_or_else(|_| {
                            ab_glyph::FontArc::try_from_slice(include_bytes!(
                                "/usr/share/fonts/TTF/DejaVuSans.ttf"
                            ))
                            .expect("No fallback font found")
                        });
                draw_text_mut(
                    &mut img,
                    to_rgba(*color),
                    position.x as i32,
                    position.y as i32,
                    *size,
                    &font,
                    content,
                );
            }
            Annotation::Freehand { points, style } => {
                let color = to_rgba(style.color);
                for pair in points.windows(2) {
                    draw_line_segment_mut(
                        &mut img,
                        (pair[0].x, pair[0].y),
                        (pair[1].x, pair[1].y),
                        color,
                    );
                }
            }
        }
    }

    img
}

// ── Screenshot Capture ──

async fn capture_screenshot(interactive: bool) -> Option<PathBuf> {
    let screenshots_dir = dirs_screenshot_dir();
    std::fs::create_dir_all(&screenshots_dir).ok()?;

    let mut cmd = tokio::process::Command::new("cosmic-screenshot");
    if interactive {
        cmd.arg("--interactive=true");
    } else {
        cmd.arg("--interactive=false")
            .arg("--save-dir")
            .arg(&screenshots_dir);
    }
    cmd.arg("--notify=false");

    let output = cmd.output().await.ok()?;
    let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path_str.is_empty() {
        return None;
    }

    // cosmic-screenshot may return a file:// URI
    let path = if let Some(stripped) = path_str.strip_prefix("file://") {
        PathBuf::from(stripped)
    } else {
        PathBuf::from(&path_str)
    };

    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn dirs_screenshot_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| d.picture_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Pictures"))
        .join("Screenshots")
}

// ── Clipboard ──

async fn copy_to_clipboard(png_bytes: Vec<u8>) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("wl-copy")
        .arg("--type")
        .arg("image/png")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn wl-copy: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&png_bytes)
            .await
            .map_err(|e| format!("Failed to write to wl-copy: {e}"))?;
    }
    child
        .wait()
        .await
        .map_err(|e| format!("wl-copy failed: {e}"))?;
    Ok(())
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut buf = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut buf);
    let (w, h) = img.dimensions();
    image::ImageEncoder::write_image(
        encoder,
        img.as_raw(),
        w,
        h,
        image::ExtendedColorType::Rgba8,
    )
    .expect("PNG encode failed");
    buf
}

// ── SVG Rendering ──

fn render_svg(path: &Path) -> anyhow::Result<DynamicImage> {
    let data = std::fs::read(path)?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default())?;
    let size = tree.size();
    let (w, h) = (size.width() as u32, size.height() as u32);
    let w = w.max(1);
    let h = h.max(1);
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("pixmap alloc"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
    let rgba = RgbaImage::from_raw(w, h, pixmap.take()).ok_or_else(|| anyhow::anyhow!("rgba"))?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

// ── EXIF Metadata ──

#[derive(Debug, Clone, Default)]
struct ExifInfo {
    camera: Option<String>,
    lens: Option<String>,
    exposure: Option<String>,
    aperture: Option<String>,
    iso: Option<String>,
    focal: Option<String>,
    date: Option<String>,
    dimensions: Option<String>,
    gps: Option<String>,
}

fn read_exif(path: &Path) -> Option<ExifInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    let get = |tag: exif::Tag| -> Option<String> {
        exif.get_field(tag, exif::In::PRIMARY)
            .map(|f| f.display_value().with_unit(&exif).to_string())
    };

    let camera = match (get(exif::Tag::Make), get(exif::Tag::Model)) {
        (Some(make), Some(model)) => {
            if model.starts_with(&make) {
                Some(model)
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (None, Some(model)) => Some(model),
        (Some(make), None) => Some(make),
        _ => None,
    };

    let gps = {
        let lat = exif
            .get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
            .map(|f| f.display_value().to_string());
        let lon = exif
            .get_field(exif::Tag::GPSLongitude, exif::In::PRIMARY)
            .map(|f| f.display_value().to_string());
        match (lat, lon) {
            (Some(la), Some(lo)) => Some(format!("{la}, {lo}")),
            _ => None,
        }
    };

    Some(ExifInfo {
        camera,
        lens: get(exif::Tag::LensModel),
        exposure: get(exif::Tag::ExposureTime),
        aperture: get(exif::Tag::FNumber),
        iso: get(exif::Tag::PhotographicSensitivity),
        focal: get(exif::Tag::FocalLength),
        date: get(exif::Tag::DateTimeOriginal),
        dimensions: None,
        gps,
    })
}

// ── Wallpaper ──

async fn set_wallpaper(path: PathBuf) -> Result<(), String> {
    // cosmic-bg uses D-Bus interface com.system76.CosmicBackground
    // Fallback: use cosmic-bg-cli or gsettings
    let result = tokio::process::Command::new("busctl")
        .args([
            "--user",
            "call",
            "com.system76.CosmicBackground",
            "/com/system76/CosmicBackground",
            "com.system76.CosmicBackground",
            "SetWallpaperSource",
            "ss",
            &path.display().to_string(),
            "all",
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => Ok(()),
        _ => {
            // Fallback: try cosmic-bg command
            let status = tokio::process::Command::new("cosmic-bg")
                .arg("set")
                .arg(&path)
                .status()
                .await
                .map_err(|e| format!("cosmic-bg: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("Failed to set wallpaper via D-Bus or cosmic-bg CLI".into())
            }
        }
    }
}

// ── Gallery ──

struct GalleryEntry {
    path: PathBuf,
    name: String,
    thumb: Option<iced_image::Handle>,
}

fn generate_thumbnail(path: &Path) -> Option<iced_image::Handle> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    let img = if ext == "svg" {
        render_svg(path).ok()?
    } else {
        image::open(path).ok()?
    };

    let thumb = img.thumbnail(160, 120);
    let rgba = thumb.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(iced_image::Handle::from_rgba(w, h, rgba.into_raw()))
}

// ── Content Mode ──

#[derive(Debug, Clone, PartialEq)]
enum ContentMode {
    Image,
    Markdown,
    Gallery,
}

// ── Edit Mode ──

#[derive(Debug, Clone, PartialEq)]
enum EditMode {
    None,
    Crop,
    Scale,
}

// ── Messages ──

#[derive(Debug, Clone)]
enum Msg {
    Open,
    Opened(Option<PathBuf>),
    Save,
    SaveAs,
    SaveTo(Option<PathBuf>),
    CopyClipboard,
    Copied(Result<(), String>),
    Next,
    Prev,
    RotateLeft,
    RotateRight,
    FlipH,
    FlipV,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ToggleCrop,
    ToggleScale,
    CropX(String),
    CropY(String),
    CropW(String),
    CropH(String),
    ApplyCrop,
    ScaleW(String),
    ScaleH(String),
    ApplyScale,
    CancelEdit,
    QualityInput(String),
    // Screenshot
    ScreenFull,
    ScreenInteractive,
    ScreenCaptured(Option<PathBuf>),
    // Annotations
    SetTool(AnnotationTool),
    SetColor(iced::Color),
    SetLineWidth(f32),
    AddAnnotation(Annotation),
    Undo,
    ClearAnnotations,
    AnnotationText(String),
    // EXIF / Gallery / Wallpaper / Markdown
    ToggleExif,
    ToggleGallery,
    GallerySelect(usize),
    SetWallpaper,
    WallpaperResult(Result<(), String>),
    MarkdownLink(String),
    // System
    Quit,
    About,
    Surface(cosmic::surface::Action),
    SyncFromPort,
    PortActivate,
    RunScript(usize),
    RescanScripts,
    ScriptsUpdated(Vec<cosmix_port::ScriptInfo>),
}

// ── App ──

struct ViewApp {
    core: Core,
    engine: Arc<Mutex<ViewEngine>>,
    keybinds: HashMap<KeyBind, MenuAction>,
    handle: Option<iced_image::Handle>,
    status: String,
    zoom: f32,
    edit_mode: EditMode,
    crop_x: String,
    crop_y: String,
    crop_w: String,
    crop_h: String,
    scale_w: String,
    scale_h: String,
    quality: String,
    // Annotations
    annotations: Vec<Annotation>,
    current_tool: AnnotationTool,
    ann_style: AnnotationStyle,
    ann_text: String,
    canvas_cache: Cache,
    // EXIF / Gallery / Markdown
    exif_info: Option<ExifInfo>,
    show_exif: bool,
    content_mode: ContentMode,
    gallery_entries: Vec<GalleryEntry>,
    markdown_source: String,
    port_scripts: Vec<cosmix_port::ScriptInfo>,
    _port: Option<cosmix_port::PortHandle>,
}

impl ViewApp {
    fn rebuild_handle(&mut self) {
        let eng = self.engine.lock().unwrap();
        self.handle = eng.to_handle();
        let status = Self::build_status(&eng, self.zoom);
        self.status = status;
        self.canvas_cache.clear();
    }

    fn build_status(eng: &ViewEngine, zoom: f32) -> String {
        let name = eng.filename();
        let dims = eng
            .dimensions()
            .map(|(w, h)| format!("  {w}x{h}"))
            .unwrap_or_default();
        let size = eng.file_size();
        let nav = if !eng.nav_info().is_empty() {
            format!("  [{}]", eng.nav_info())
        } else {
            String::new()
        };
        let modified = if eng.modified { " *" } else { "" };
        let zoom_pct = (zoom * 100.0) as u32;
        format!("{name}{dims}  {size}{nav}  {zoom_pct}%{modified}")
    }

    fn update_status_from_engine(&mut self) {
        let eng = self.engine.lock().unwrap();
        self.status = Self::build_status(&eng, self.zoom);
    }

    fn open_file(&mut self, path: &Path) {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        if ext == "md" || ext == "markdown" {
            // Markdown mode
            match std::fs::read_to_string(path) {
                Ok(source) => {
                    self.markdown_source = source;
                    self.content_mode = ContentMode::Markdown;
                    self.status = format!(
                        "{}",
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("Markdown")
                    );
                    // Still track the file for nav purposes
                    let mut eng = self.engine.lock().unwrap();
                    eng.path = Some(path.to_path_buf());
                    eng.scan_directory();
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
            self.exif_info = None;
            return;
        }

        // Image/SVG mode
        let mut eng = self.engine.lock().unwrap();
        if let Err(e) = eng.open(path) {
            self.status = format!("Error: {e}");
            return;
        }
        drop(eng);
        self.content_mode = ContentMode::Image;
        self.zoom = 1.0;
        self.annotations.clear();
        self.rebuild_handle();

        // Load EXIF
        self.exif_info = read_exif(path);
        if let Some(ref mut info) = self.exif_info {
            let eng = self.engine.lock().unwrap();
            if let Some((w, h)) = eng.dimensions() {
                info.dimensions = Some(format!("{w} x {h}"));
            }
        }
    }

    fn build_gallery(&mut self) {
        let eng = self.engine.lock().unwrap();
        let entries: Vec<GalleryEntry> = eng
            .dir_images
            .iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                GalleryEntry {
                    path: p.clone(),
                    name,
                    thumb: None,
                }
            })
            .collect();
        drop(eng);
        self.gallery_entries = entries;

        // Generate thumbnails (lazy — just do first batch)
        for entry in &mut self.gallery_entries {
            entry.thumb = generate_thumbnail(&entry.path);
        }
    }

    fn view_exif_panel(&self) -> Element<'_, Msg> {
        let spacing = cosmic::theme::spacing();
        let mut col = column::with_capacity(12).spacing(4).padding(spacing.space_xxs);

        col = col.push(text::heading("EXIF Metadata"));

        if let Some(ref info) = self.exif_info {
            let fields: &[(&str, &Option<String>)] = &[
                ("Camera", &info.camera),
                ("Lens", &info.lens),
                ("Exposure", &info.exposure),
                ("Aperture", &info.aperture),
                ("ISO", &info.iso),
                ("Focal", &info.focal),
                ("Date", &info.date),
                ("Size", &info.dimensions),
                ("GPS", &info.gps),
            ];
            for &(label, value) in fields {
                if let Some(v) = value {
                    col = col.push(
                        row::with_capacity(2)
                            .push(
                                text::caption(format!("{label}:"))
                                    .width(Length::Fixed(80.0)),
                            )
                            .push(text::body(v))
                            .spacing(4),
                    );
                }
            }
        } else {
            col = col.push(text::body("No EXIF data available"));
        }

        cosmic::widget::container(col)
            .width(Length::Fixed(250.0))
            .height(Length::Fill)
            .class(cosmic::theme::Container::Card)
            .into()
    }

    fn view_gallery(&self) -> Element<'_, Msg> {
        let spacing = cosmic::theme::spacing();
        let mut grid = cosmic::widget::column().spacing(spacing.space_xs);
        let mut current_row = row::with_capacity(6).spacing(spacing.space_xs);
        let items_per_row = 5;

        for (idx, entry) in self.gallery_entries.iter().enumerate() {
            let eng = self.engine.lock().unwrap();
            let is_current = eng
                .path
                .as_ref()
                .is_some_and(|p| p == &entry.path);
            drop(eng);

            let thumb_element: Element<'_, Msg> = if let Some(ref handle) = entry.thumb {
                cosmic::widget::image(handle.clone())
                    .width(Length::Fixed(160.0))
                    .height(Length::Fixed(120.0))
                    .content_fit(iced::ContentFit::Contain)
                    .into()
            } else {
                cosmic::widget::container(icon::from_name("image-x-generic-symbolic").size(48))
                    .width(Length::Fixed(160.0))
                    .height(Length::Fixed(120.0))
                    .align_x(iced::Alignment::Center)
                    .align_y(iced::Alignment::Center)
                    .into()
            };

            let card = cosmic::widget::column()
                .push(thumb_element)
                .push(
                    text::caption(&entry.name)
                        .width(Length::Fixed(160.0)),
                )
                .spacing(2);

            let btn = button::custom(card)
                .on_press(Msg::GallerySelect(idx))
                .padding(4);

            let btn = if is_current {
                btn.selected(true).class(cosmic::theme::Button::ListItem)
            } else {
                btn.class(cosmic::theme::Button::ListItem)
            };

            current_row = current_row.push(btn);

            if (idx + 1) % items_per_row == 0 {
                grid = grid.push(current_row);
                current_row = row::with_capacity(6).spacing(spacing.space_xs);
            }
        }

        // Push remaining row
        if !self.gallery_entries.is_empty()
            && self.gallery_entries.len() % items_per_row != 0
        {
            grid = grid.push(current_row);
        }

        cosmic::widget::scrollable::vertical(grid.padding(spacing.space_xs))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_markdown(&self) -> Element<'_, Msg> {
        let is_dark = cosmic::theme::is_dark();
        let md_theme = if is_dark {
            cosmix_markdown::Theme::dark()
        } else {
            cosmix_markdown::Theme::light()
        };
        let md_view = cosmix_markdown::view(&self.markdown_source, &md_theme, Msg::MarkdownLink);
        let spacing = cosmic::theme::spacing();
        cosmic::widget::scrollable::vertical(
            cosmic::widget::container(md_view)
                .padding(spacing.space_m)
                .width(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn get_flattened_image(&self) -> Option<RgbaImage> {
        let eng = self.engine.lock().unwrap();
        let base = eng.to_rgba()?;
        if self.annotations.is_empty() {
            Some(base)
        } else {
            Some(flatten_annotations(&base, &self.annotations))
        }
    }

    fn save_to_screenshots(&self) -> Result<String, String> {
        let img = self
            .get_flattened_image()
            .ok_or_else(|| "No image loaded".to_string())?;
        let dir = dirs_screenshot_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
        let ts = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let path = dir.join(format!("Screenshot_{ts}.png"));
        let dyn_img = DynamicImage::ImageRgba8(img);
        dyn_img.save(&path).map_err(|e| format!("save: {e}"))?;
        Ok(format!("Saved: {}", path.display()))
    }

    fn annotation_toolbar(&self) -> Element<'_, Msg> {
        let tool_btn =
            |label: &str, tool: AnnotationTool, current: AnnotationTool| -> Element<'_, Msg> {
                let b = button::standard(label.to_string());
                if current == tool {
                    b.on_press(Msg::SetTool(AnnotationTool::Select))
                        .class(cosmic::style::Button::Suggested)
                        .into()
                } else {
                    b.on_press(Msg::SetTool(tool)).into()
                }
            };

        let mut toolbar = row::with_capacity(20).spacing(4).align_y(iced::Alignment::Center);

        // Tools
        toolbar = toolbar
            .push(tool_btn("Arrow", AnnotationTool::Arrow, self.current_tool))
            .push(tool_btn("Rect", AnnotationTool::Rect, self.current_tool))
            .push(tool_btn("Circle", AnnotationTool::Circle, self.current_tool))
            .push(tool_btn("Text", AnnotationTool::Text, self.current_tool))
            .push(tool_btn("Draw", AnnotationTool::Freehand, self.current_tool));

        // Separator
        toolbar = toolbar.push(text::body("  |  "));

        // Color buttons
        for &(color, name) in COLORS {
            let is_selected = (self.ann_style.color.r - color.r).abs() < 0.01
                && (self.ann_style.color.g - color.g).abs() < 0.01
                && (self.ann_style.color.b - color.b).abs() < 0.01;
            let label = if is_selected {
                format!("[{name}]")
            } else {
                name.to_string()
            };
            toolbar = toolbar.push(
                button::text(label)
                    .on_press(Msg::SetColor(color)),
            );
        }

        toolbar = toolbar.push(text::body("  |  "));

        // Line width
        for &(width, name) in LINE_WIDTHS {
            let is_selected = (self.ann_style.width - width).abs() < 0.5;
            let label = if is_selected {
                format!("[{name}]")
            } else {
                name.to_string()
            };
            toolbar = toolbar.push(
                button::text(label)
                    .on_press(Msg::SetLineWidth(width)),
            );
        }

        // Text input for text annotations
        if self.current_tool == AnnotationTool::Text {
            toolbar = toolbar
                .push(text::body("  Text:"))
                .push(
                    text_input("Type here...", &self.ann_text)
                        .on_input(Msg::AnnotationText)
                        .width(Length::Fixed(150.0))
                        .size(13),
                );
        }

        toolbar = toolbar.push(text::body("  |  "));

        // Undo / Clear
        toolbar = toolbar
            .push(button::text("Undo").on_press(Msg::Undo))
            .push(
                button::text("Clear All")
                    .on_press(Msg::ClearAnnotations),
            );

        toolbar.into()
    }
}

impl cosmic::Application for ViewApp {
    type Executor = executor::Default;
    type Flags = Option<PathBuf>;
    type Message = Msg;

    const APP_ID: &'static str = "org.cosmix.View";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, flags: Self::Flags) -> (Self, cosmic::app::Task<Self::Message>) {
        // Don't set header_title — it fills the header bar center area and
        // blocks the system-level RMB context menu (sticky, minimize, etc.).
        core.window.content_container = true;

        let engine = Arc::new(Mutex::new(ViewEngine::new()));
        let (port_tx, port_rx) = tokio::sync::mpsc::unbounded_channel();
        PORT_RX.set(Arc::new(tokio::sync::Mutex::new(port_rx))).ok();
        let port = start_port(engine.clone(), port_tx);

        let mut app = Self {
            core,
            engine,
            keybinds: key_binds(),
            handle: None,
            status: "No image loaded -- Ctrl+O to open, Screenshot menu to capture".into(),
            zoom: 1.0,
            edit_mode: EditMode::None,
            crop_x: "0".into(),
            crop_y: "0".into(),
            crop_w: String::new(),
            crop_h: String::new(),
            scale_w: String::new(),
            scale_h: String::new(),
            quality: "90".into(),
            annotations: Vec::new(),
            current_tool: AnnotationTool::Select,
            ann_style: AnnotationStyle::default(),
            ann_text: String::new(),
            canvas_cache: Cache::default(),
            exif_info: None,
            show_exif: false,
            content_mode: ContentMode::Image,
            gallery_entries: Vec::new(),
            markdown_source: String::new(),
            port_scripts: Vec::new(),
            _port: port,
        };

        if let Some(ref path) = flags {
            app.open_file(path);
        }

        (app, cosmic::app::Task::none())
    }

    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        vec![
            text::body("Quality:").into(),
            text_input("90", &self.quality)
                .on_input(Msg::QualityInput)
                .width(Length::Fixed(48.0))
                .size(13)
                .into(),
        ]
    }

    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        let mut menus = vec![
            (
                "File".into(),
                vec![
                    menu::Item::Button(
                        "Capture Full Screen",
                        Some(icon::from_name("camera-photo-symbolic").into()),
                        MenuAction::ScreenFull,
                    ),
                    menu::Item::Button(
                        "Capture Interactive",
                        Some(icon::from_name("edit-select-all-symbolic").into()),
                        MenuAction::ScreenInteractive,
                    ),
                    menu::Item::Divider,
                    menu::Item::Button(
                        "Open",
                        Some(icon::from_name("document-open-symbolic").into()),
                        MenuAction::Open,
                    ),
                    menu::Item::Button(
                        "Save",
                        Some(icon::from_name("document-save-symbolic").into()),
                        MenuAction::Save,
                    ),
                    menu::Item::Button(
                        "Save As",
                        Some(icon::from_name("document-save-as-symbolic").into()),
                        MenuAction::SaveAs,
                    ),
                    menu::Item::Divider,
                    menu::Item::Button(
                        "Copy to Clipboard",
                        Some(icon::from_name("edit-copy-symbolic").into()),
                        MenuAction::CopyClipboard,
                    ),
                    menu::Item::Divider,
                    menu::Item::Button(
                        "Set as Wallpaper",
                        Some(icon::from_name("preferences-desktop-wallpaper-symbolic").into()),
                        MenuAction::SetWallpaper,
                    ),
                    menu::Item::Divider,
                    menu::Item::Button(
                        "Quit",
                        Some(icon::from_name("window-close-symbolic").into()),
                        MenuAction::Quit,
                    ),
                ],
            ),
            (
                "Edit".into(),
                vec![
                    menu::Item::Button(
                        "Rotate Left",
                        Some(icon::from_name("object-rotate-left-symbolic").into()),
                        MenuAction::RotateLeft,
                    ),
                    menu::Item::Button(
                        "Rotate Right",
                        Some(icon::from_name("object-rotate-right-symbolic").into()),
                        MenuAction::RotateRight,
                    ),
                    menu::Item::Divider,
                    menu::Item::Button(
                        "Flip Horizontal",
                        Some(icon::from_name("object-flip-horizontal-symbolic").into()),
                        MenuAction::FlipH,
                    ),
                    menu::Item::Button(
                        "Flip Vertical",
                        Some(icon::from_name("object-flip-vertical-symbolic").into()),
                        MenuAction::FlipV,
                    ),
                    menu::Item::Divider,
                    menu::Item::Button("Crop", None, MenuAction::Crop),
                    menu::Item::Button("Scale", None, MenuAction::Scale),
                    menu::Item::Divider,
                    menu::Item::Button("Undo Annotation", None, MenuAction::Undo),
                    menu::Item::Button(
                        "Clear Annotations",
                        None,
                        MenuAction::ClearAnnotations,
                    ),
                ],
            ),
            (
                "View".into(),
                vec![
                    menu::Item::Button(
                        "Zoom In",
                        Some(icon::from_name("zoom-in-symbolic").into()),
                        MenuAction::ZoomIn,
                    ),
                    menu::Item::Button(
                        "Zoom Out",
                        Some(icon::from_name("zoom-out-symbolic").into()),
                        MenuAction::ZoomOut,
                    ),
                    menu::Item::Button(
                        "Fit to Window",
                        Some(icon::from_name("zoom-fit-best-symbolic").into()),
                        MenuAction::ZoomFit,
                    ),
                    menu::Item::Divider,
                    menu::Item::Button("Previous Image", None, MenuAction::Prev),
                    menu::Item::Button("Next Image", None, MenuAction::Next),
                    menu::Item::Divider,
                    menu::Item::Button(
                        "Gallery",
                        Some(icon::from_name("view-grid-symbolic").into()),
                        MenuAction::ToggleGallery,
                    ),
                    menu::Item::Button(
                        "EXIF Metadata",
                        Some(icon::from_name("document-properties-symbolic").into()),
                        MenuAction::ToggleExif,
                    ),
                ],
            ),
            (
                "Help".into(),
                vec![menu::Item::Button(
                    "About Cosmix View",
                    Some(icon::from_name("help-about-symbolic").into()),
                    MenuAction::About,
                )],
            ),
        ];

        // Scripts menu (populated by daemon via __scripts__ port command)
        if !self.port_scripts.is_empty() {
            let mut script_items: Vec<menu::Item<MenuAction, &str>> = self.port_scripts
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let name: &str = Box::leak(s.display_name.clone().into_boxed_str());
                    menu::Item::Button(
                        name,
                        Some(icon::from_name("text-x-script-symbolic").into()),
                        MenuAction::RunScript(i),
                    )
                })
                .collect();
            script_items.push(menu::Item::Divider);
            script_items.push(menu::Item::Button(
                "Rescan Scripts",
                Some(icon::from_name("view-refresh-symbolic").into()),
                MenuAction::RescanScripts,
            ));
            menus.push(("Scripts".into(), script_items));
        }

        vec![cosmic::widget::responsive_menu_bar()
            .item_height(ItemHeight::Dynamic(40))
            .item_width(ItemWidth::Uniform(280))
            .spacing(4.0)
            .into_element(
                self.core(),
                &self.keybinds,
                MENU_ID.clone(),
                Msg::Surface,
                menus,
            )]
    }

    fn update(&mut self, message: Self::Message) -> cosmic::app::Task<Self::Message> {
        match message {
            Msg::Open => {
                return cosmic::app::Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .add_filter("All Viewable", VIEWABLE_EXTS)
                            .add_filter("Images", IMAGE_EXTS)
                            .add_filter("Markdown", &["md", "markdown"])
                            .add_filter("SVG", &["svg"])
                            .pick_file()
                            .await
                            .map(|f| f.path().to_path_buf())
                    },
                    |result| cosmic::Action::App(Msg::Opened(result)),
                );
            }
            Msg::Opened(Some(path)) => {
                self.open_file(&path);
            }
            Msg::Opened(None) => {}
            Msg::Save => {
                match self.save_to_screenshots() {
                    Ok(msg) => self.status = msg,
                    Err(e) => self.status = format!("Save error: {e}"),
                }
            }
            Msg::SaveAs => {
                let eng = self.engine.lock().unwrap();
                let default_name = eng.filename();
                drop(eng);
                return cosmic::app::Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_file_name(&default_name)
                            .add_filter("PNG", &["png"])
                            .add_filter("JPEG", &["jpg", "jpeg"])
                            .add_filter("WebP", &["webp"])
                            .save_file()
                            .await
                            .map(|f| f.path().to_path_buf())
                    },
                    |result| cosmic::Action::App(Msg::SaveTo(result)),
                );
            }
            Msg::SaveTo(Some(path)) => {
                if let Some(img) = self.get_flattened_image() {
                    let q: u8 = self.quality.parse().unwrap_or(90);
                    let dyn_img = DynamicImage::ImageRgba8(img);
                    let eng_tmp = ViewEngine {
                        image: Some(dyn_img),
                        path: None,
                        dir_images: Vec::new(),
                        dir_index: 0,
                        modified: false,
                    };
                    match eng_tmp.save(&path, q) {
                        Ok(_) => self.status = format!("Saved: {}", path.display()),
                        Err(e) => self.status = format!("Save error: {e}"),
                    }
                } else {
                    self.status = "No image to save".into();
                }
            }
            Msg::SaveTo(None) => {}
            Msg::CopyClipboard => {
                if let Some(img) = self.get_flattened_image() {
                    let png_bytes = encode_png(&img);
                    return cosmic::app::Task::perform(
                        async move { copy_to_clipboard(png_bytes).await },
                        |result| cosmic::Action::App(Msg::Copied(result)),
                    );
                } else {
                    self.status = "No image to copy".into();
                }
            }
            Msg::Copied(result) => match result {
                Ok(()) => self.status = "Copied to clipboard".into(),
                Err(e) => self.status = format!("Copy error: {e}"),
            },
            Msg::Next => {
                let eng = self.engine.lock().unwrap();
                if eng.dir_images.is_empty() {
                    self.status = "No files in directory".into();
                    return cosmic::app::Task::none();
                }
                let len = eng.dir_images.len();
                let new_idx = (eng.dir_index + 1) % len;
                let path = eng.dir_images[new_idx].clone();
                drop(eng);
                self.open_file(&path);
            }
            Msg::Prev => {
                let eng = self.engine.lock().unwrap();
                if eng.dir_images.is_empty() {
                    self.status = "No files in directory".into();
                    return cosmic::app::Task::none();
                }
                let len = eng.dir_images.len();
                let new_idx = if eng.dir_index == 0 { len - 1 } else { eng.dir_index - 1 };
                let path = eng.dir_images[new_idx].clone();
                drop(eng);
                self.open_file(&path);
            }
            Msg::RotateLeft => {
                self.engine.lock().unwrap().rotate_ccw();
                self.rebuild_handle();
            }
            Msg::RotateRight => {
                self.engine.lock().unwrap().rotate_cw();
                self.rebuild_handle();
            }
            Msg::FlipH => {
                self.engine.lock().unwrap().flip_h();
                self.rebuild_handle();
            }
            Msg::FlipV => {
                self.engine.lock().unwrap().flip_v();
                self.rebuild_handle();
            }
            Msg::ZoomIn => {
                self.zoom = (self.zoom * 1.25).min(10.0);
                self.update_status_from_engine();
                self.canvas_cache.clear();
            }
            Msg::ZoomOut => {
                self.zoom = (self.zoom / 1.25).max(0.1);
                self.update_status_from_engine();
                self.canvas_cache.clear();
            }
            Msg::ZoomFit => {
                self.zoom = 1.0;
                self.update_status_from_engine();
                self.canvas_cache.clear();
            }
            Msg::ToggleCrop => {
                if self.edit_mode == EditMode::Crop {
                    self.edit_mode = EditMode::None;
                } else {
                    self.edit_mode = EditMode::Crop;
                    let eng = self.engine.lock().unwrap();
                    if let Some((w, h)) = eng.dimensions() {
                        self.crop_x = "0".into();
                        self.crop_y = "0".into();
                        self.crop_w = w.to_string();
                        self.crop_h = h.to_string();
                    }
                }
            }
            Msg::ToggleScale => {
                if self.edit_mode == EditMode::Scale {
                    self.edit_mode = EditMode::None;
                } else {
                    self.edit_mode = EditMode::Scale;
                    let eng = self.engine.lock().unwrap();
                    if let Some((w, h)) = eng.dimensions() {
                        self.scale_w = w.to_string();
                        self.scale_h = h.to_string();
                    }
                }
            }
            Msg::CropX(s) => self.crop_x = s,
            Msg::CropY(s) => self.crop_y = s,
            Msg::CropW(s) => self.crop_w = s,
            Msg::CropH(s) => self.crop_h = s,
            Msg::ApplyCrop => {
                let x: u32 = self.crop_x.parse().unwrap_or(0);
                let y: u32 = self.crop_y.parse().unwrap_or(0);
                let w: u32 = self.crop_w.parse().unwrap_or(0);
                let h: u32 = self.crop_h.parse().unwrap_or(0);
                let mut eng = self.engine.lock().unwrap();
                match eng.crop(x, y, w, h) {
                    Ok(_) => {
                        drop(eng);
                        self.edit_mode = EditMode::None;
                        self.annotations.clear();
                        self.rebuild_handle();
                    }
                    Err(e) => self.status = format!("Crop error: {e}"),
                }
            }
            Msg::ScaleW(s) => self.scale_w = s,
            Msg::ScaleH(s) => self.scale_h = s,
            Msg::ApplyScale => {
                let w: u32 = self.scale_w.parse().unwrap_or(0);
                let h: u32 = self.scale_h.parse().unwrap_or(0);
                if w > 0 && h > 0 {
                    self.engine.lock().unwrap().scale(w, h);
                    self.edit_mode = EditMode::None;
                    self.annotations.clear();
                    self.rebuild_handle();
                } else {
                    self.status = "Invalid dimensions".into();
                }
            }
            Msg::CancelEdit => {
                self.edit_mode = EditMode::None;
            }
            Msg::QualityInput(s) => self.quality = s,
            // Screenshot
            Msg::ScreenFull => {
                return cosmic::app::Task::perform(
                    async { capture_screenshot(false).await },
                    |result| cosmic::Action::App(Msg::ScreenCaptured(result)),
                );
            }
            Msg::ScreenInteractive => {
                return cosmic::app::Task::perform(
                    async { capture_screenshot(true).await },
                    |result| cosmic::Action::App(Msg::ScreenCaptured(result)),
                );
            }
            Msg::ScreenCaptured(Some(path)) => {
                self.open_file(&path);
                self.status = format!("Screenshot loaded: {}", path.display());
            }
            Msg::ScreenCaptured(None) => {
                self.status = "Screenshot cancelled or failed".into();
            }
            // Annotations
            Msg::SetTool(tool) => {
                self.current_tool = tool;
            }
            Msg::SetColor(color) => {
                self.ann_style.color = color;
            }
            Msg::SetLineWidth(w) => {
                self.ann_style.width = w;
            }
            Msg::AddAnnotation(mut ann) => {
                // For text annotations, inject the text from the toolbar input
                if let Annotation::Text {
                    ref mut content, ..
                } = ann
                {
                    if content.is_empty() {
                        *content = if self.ann_text.is_empty() {
                            "Text".to_string()
                        } else {
                            self.ann_text.clone()
                        };
                    }
                }
                self.annotations.push(ann);
                self.canvas_cache.clear();
            }
            Msg::Undo => {
                self.annotations.pop();
                self.canvas_cache.clear();
            }
            Msg::ClearAnnotations => {
                self.annotations.clear();
                self.canvas_cache.clear();
            }
            Msg::AnnotationText(s) => self.ann_text = s,
            // EXIF / Gallery / Wallpaper / Markdown
            Msg::ToggleExif => {
                self.show_exif = !self.show_exif;
            }
            Msg::ToggleGallery => {
                if self.content_mode == ContentMode::Gallery {
                    self.content_mode = ContentMode::Image;
                } else {
                    self.build_gallery();
                    self.content_mode = ContentMode::Gallery;
                }
            }
            Msg::GallerySelect(idx) => {
                if let Some(entry) = self.gallery_entries.get(idx) {
                    let path = entry.path.clone();
                    self.content_mode = ContentMode::Image;
                    self.open_file(&path);
                }
            }
            Msg::SetWallpaper => {
                let eng = self.engine.lock().unwrap();
                if let Some(path) = eng.path.clone() {
                    drop(eng);
                    // If there are annotations, save flattened to tmp first
                    let final_path = if !self.annotations.is_empty() {
                        if let Some(img) = self.get_flattened_image() {
                            let tmp = std::env::temp_dir().join("cosmix-wallpaper.png");
                            let dyn_img = DynamicImage::ImageRgba8(img);
                            if dyn_img.save(&tmp).is_ok() {
                                tmp
                            } else {
                                path
                            }
                        } else {
                            path
                        }
                    } else {
                        path
                    };
                    return cosmic::app::Task::perform(
                        async move { set_wallpaper(final_path).await },
                        |result| cosmic::Action::App(Msg::WallpaperResult(result)),
                    );
                } else {
                    self.status = "No image loaded".into();
                }
            }
            Msg::WallpaperResult(result) => match result {
                Ok(()) => self.status = "Wallpaper set".into(),
                Err(e) => self.status = format!("Wallpaper error: {e}"),
            },
            Msg::MarkdownLink(ref url) => {
                let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            }
            Msg::Quit => return cosmic::iced::exit(),
            Msg::About => {}
            Msg::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(a),
                ));
            }
            Msg::RunScript(i) => {
                if let Some(script) = self.port_scripts.get(i) {
                    let path = script.path.clone();
                    return cosmic::app::Task::perform(
                        async move {
                            let _ = tokio::process::Command::new("cosmix")
                                .args(["run-for", &path, "COSMIX-VIEW.1"])
                                .output()
                                .await;
                        },
                        |_| cosmic::Action::App(Msg::SyncFromPort),
                    );
                }
            }
            Msg::RescanScripts => {
                return cosmic::app::Task::perform(
                    async {
                        let _ = tokio::process::Command::new("cosmix")
                            .args(["rescan-scripts", "COSMIX-VIEW.1"])
                            .output()
                            .await;
                    },
                    |_| cosmic::Action::App(Msg::SyncFromPort),
                );
            }
            Msg::ScriptsUpdated(scripts) => {
                self.port_scripts = scripts;
            }
            Msg::SyncFromPort | Msg::PortActivate => {}
        }
        cosmic::app::Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::run(|| {
            stream::channel(16, |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
                let rx = PORT_RX.get().expect("port receiver not initialized");
                loop {
                    match rx.lock().await.recv().await {
                        Some(cosmix_port::PortEvent::Activate) => {
                            let _ = output.send(Msg::PortActivate).await;
                        }
                        Some(cosmix_port::PortEvent::ScriptsUpdated(scripts)) => {
                            let _ = output.send(Msg::ScriptsUpdated(scripts)).await;
                        }
                        Some(cosmix_port::PortEvent::Command { .. }) => {
                            let _ = output.send(Msg::SyncFromPort).await;
                        }
                        None => {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            })
        })
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let mut content = column::with_capacity(5).spacing(4);

        match self.content_mode {
            ContentMode::Gallery => {
                content = content.push(self.view_gallery());
            }
            ContentMode::Markdown => {
                content = content.push(self.view_markdown());
            }
            ContentMode::Image => {
                // Edit mode panel
                match &self.edit_mode {
                    EditMode::Crop => {
                        let crop_row = row::with_capacity(10)
                            .spacing(4)
                            .push(text::body("X:"))
                            .push(num_input("0", &self.crop_x, Msg::CropX))
                            .push(text::body("Y:"))
                            .push(num_input("0", &self.crop_y, Msg::CropY))
                            .push(text::body("W:"))
                            .push(num_input("0", &self.crop_w, Msg::CropW))
                            .push(text::body("H:"))
                            .push(num_input("0", &self.crop_h, Msg::CropH))
                            .push(
                                button::suggested("Apply".to_string()).on_press(Msg::ApplyCrop),
                            )
                            .push(
                                button::standard("Cancel".to_string()).on_press(Msg::CancelEdit),
                            );
                        content = content.push(crop_row);
                    }
                    EditMode::Scale => {
                        let scale_row = row::with_capacity(6)
                            .spacing(4)
                            .push(text::body("W:"))
                            .push(num_input("0", &self.scale_w, Msg::ScaleW))
                            .push(text::body("H:"))
                            .push(num_input("0", &self.scale_h, Msg::ScaleH))
                            .push(
                                button::suggested("Apply".to_string())
                                    .on_press(Msg::ApplyScale),
                            )
                            .push(
                                button::standard("Cancel".to_string()).on_press(Msg::CancelEdit),
                            );
                        content = content.push(scale_row);
                    }
                    EditMode::None => {}
                }

                // Annotation toolbar — always visible in image mode
                content = content.push(self.annotation_toolbar());

                // Image display via Canvas
                let eng = self.engine.lock().unwrap();
                let (img_w, img_h) = eng.dimensions().unwrap_or((0, 0));
                drop(eng);

                let program = AnnotationCanvas {
                    handle: self.handle.as_ref(),
                    annotations: &self.annotations,
                    tool: self.current_tool,
                    style: self.ann_style,
                    img_w,
                    img_h,
                    zoom: self.zoom,
                };

                let canvas_area: Element<'_, Msg> = Canvas::new(program)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into();

                // Optionally show EXIF panel alongside canvas
                if self.show_exif {
                    content = content.push(
                        row::with_capacity(2)
                            .push(canvas_area)
                            .push(self.view_exif_panel())
                            .spacing(4)
                            .height(Length::Fill),
                    );
                } else {
                    content = content.push(canvas_area);
                }
            }
        }

        // Status bar
        let ann_count = if self.annotations.is_empty() {
            String::new()
        } else {
            format!("  [{} annotations]", self.annotations.len())
        };
        content = content.push(text::caption(format!("{}{ann_count}", self.status)));

        content.padding(8).into()
    }
}

fn num_input<'a>(
    placeholder: &'a str,
    value: &'a str,
    on_input: fn(String) -> Msg,
) -> Element<'a, Msg> {
    text_input(placeholder, value)
        .on_input(on_input)
        .width(Length::Fixed(56.0))
        .size(13)
        .into()
}

// ── Cosmix Port Integration ──

fn start_port(
    engine: Arc<Mutex<ViewEngine>>,
    notifier: tokio::sync::mpsc::UnboundedSender<cosmix_port::PortEvent>,
) -> Option<cosmix_port::PortHandle> {
    let e1 = engine.clone();
    let e2 = engine.clone();
    let e3 = engine.clone();
    let e4 = engine.clone();
    let e5 = engine.clone();
    let e6 = engine.clone();
    let e7 = engine.clone();
    let e8 = engine.clone();
    let e9 = engine.clone();
    let e10 = engine.clone();

    let port = cosmix_port::Port::new("cosmix-view")
        .events(notifier)
        .command("open", "Open an image file", move |args| {
            let path = args
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("expected file path string"))?;
            let mut eng = e1.lock().unwrap();
            eng.open(Path::new(path))?;
            let (w, h) = eng.dimensions().unwrap_or((0, 0));
            Ok(serde_json::json!({
                "file": eng.filename(),
                "width": w,
                "height": h,
            }))
        })
        .command("save", "Save the current image", move |args| {
            let obj = args.as_object().ok_or_else(|| {
                anyhow::anyhow!("expected object with 'path' and optional 'quality'")
            })?;
            let path = obj
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing 'path'"))?;
            let quality = obj
                .get("quality")
                .and_then(|v| v.as_u64())
                .unwrap_or(90) as u8;
            let eng = e2.lock().unwrap();
            eng.save(Path::new(path), quality)?;
            Ok(serde_json::json!({ "saved": path, "quality": quality }))
        })
        .command("next", "Navigate to next image in directory", move |_| {
            let mut eng = e3.lock().unwrap();
            eng.navigate(1)?;
            Ok(serde_json::json!({ "file": eng.filename() }))
        })
        .command("prev", "Navigate to previous image in directory", move |_| {
            let mut eng = e4.lock().unwrap();
            eng.navigate(-1)?;
            Ok(serde_json::json!({ "file": eng.filename() }))
        })
        .command("rotate", "Rotate the image (cw or ccw)", move |args| {
            let dir = args.as_str().unwrap_or("cw");
            let mut eng = e5.lock().unwrap();
            match dir {
                "cw" | "right" | "90" => eng.rotate_cw(),
                "ccw" | "left" | "-90" | "270" => eng.rotate_ccw(),
                _ => return Err(anyhow::anyhow!("unknown direction: {dir} (use cw/ccw)")),
            }
            let (w, h) = eng.dimensions().unwrap_or((0, 0));
            Ok(serde_json::json!({ "width": w, "height": h }))
        })
        .command("flip", "Flip the image (h or v)", move |args| {
            let dir = args.as_str().unwrap_or("h");
            let mut eng = e6.lock().unwrap();
            match dir {
                "h" | "horizontal" => eng.flip_h(),
                "v" | "vertical" => eng.flip_v(),
                _ => return Err(anyhow::anyhow!("unknown direction: {dir} (use h/v)")),
            }
            Ok(serde_json::json!("flipped"))
        })
        .command("crop", "Crop the image to x, y, w, h", move |args| {
            let obj = args
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("expected object with x, y, w, h"))?;
            let x = obj.get("x").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let y = obj.get("y").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let w = obj
                .get("w")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing 'w'"))? as u32;
            let h = obj
                .get("h")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing 'h'"))? as u32;
            let mut eng = e7.lock().unwrap();
            eng.crop(x, y, w, h)?;
            let (nw, nh) = eng.dimensions().unwrap_or((0, 0));
            Ok(serde_json::json!({ "width": nw, "height": nh }))
        })
        .command("scale", "Scale the image to w, h", move |args| {
            let obj = args
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("expected object with w, h"))?;
            let w = obj
                .get("w")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing 'w'"))? as u32;
            let h = obj
                .get("h")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("missing 'h'"))? as u32;
            let mut eng = e8.lock().unwrap();
            eng.scale(w, h);
            Ok(serde_json::json!({ "width": w, "height": h }))
        })
        .command("fileinfo", "Get current image file metadata", move |_| {
            let eng = e9.lock().unwrap();
            let (w, h) = eng.dimensions().unwrap_or((0, 0));
            Ok(serde_json::json!({
                "file": eng.filename(),
                "width": w,
                "height": h,
                "modified": eng.modified,
                "index": eng.dir_index,
                "total": eng.dir_images.len(),
                "size": eng.file_size(),
            }))
        })
        .command("capture", "Take a screenshot (full or interactive)", move |args| {
            let mode = args.as_str().unwrap_or("full");
            let interactive = mode != "full";
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| anyhow::anyhow!("tokio: {e}"))?;
            let path = rt
                .block_on(capture_screenshot(interactive))
                .ok_or_else(|| anyhow::anyhow!("capture failed or cancelled"))?;
            let mut eng = e10.lock().unwrap();
            eng.open(&path)?;
            let (w, h) = eng.dimensions().unwrap_or((0, 0));
            Ok(serde_json::json!({
                "file": path.display().to_string(),
                "width": w,
                "height": h,
            }))
        })
        .standard_help()
        .standard_info("Cosmix View", env!("CARGO_PKG_VERSION"))
        .standard_activate();

    match port.start() {
        Ok(handle) => {
            tracing::info!("Cosmix port started at {}", handle.socket_path.display());
            Some(handle)
        }
        Err(e) => {
            tracing::warn!("Failed to start cosmix port: {e}");
            None
        }
    }
}

// ── Main ──

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("cosmix_view=info")
        .init();

    let path = std::env::args().nth(1).map(PathBuf::from);

    let settings = Settings::default().size(Size::new(1100.0, 750.0));

    cosmic::app::run::<ViewApp>(settings, path)?;

    Ok(())
}
