use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use cosmic::app::Settings;
use cosmic::iced::widget::image as iced_image;
use cosmic::iced::{self, ContentFit, Length, Size};
use cosmic::widget::menu::{self, key_bind::Modifier, ItemHeight, ItemWidth, KeyBind};
use cosmic::widget::{button, column, container, icon, row, text, text_input};
use cosmic::{executor, Core, Element};
use image::{DynamicImage, GenericImageView};

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif"];

static MENU_ID: LazyLock<iced::id::Id> = LazyLock::new(|| iced::id::Id::new("view_menu"));

// ── Menu Actions ──

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Open,
    Save,
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
    About,
}

impl menu::Action for Action {
    type Message = Msg;

    fn message(&self) -> Msg {
        match self {
            Action::Open => Msg::Open,
            Action::Save => Msg::Save,
            Action::Quit => Msg::Quit,
            Action::RotateLeft => Msg::RotateLeft,
            Action::RotateRight => Msg::RotateRight,
            Action::FlipH => Msg::FlipH,
            Action::FlipV => Msg::FlipV,
            Action::Crop => Msg::ToggleCrop,
            Action::Scale => Msg::ToggleScale,
            Action::ZoomIn => Msg::ZoomIn,
            Action::ZoomOut => Msg::ZoomOut,
            Action::ZoomFit => Msg::ZoomFit,
            Action::Prev => Msg::Prev,
            Action::Next => Msg::Next,
            Action::About => Msg::About,
        }
    }
}

fn key_binds() -> HashMap<KeyBind, Action> {
    use iced::keyboard::{key::Named, Key};

    let mut kb = HashMap::new();
    macro_rules! bind {
        ([$($m:ident),+], $key:expr, $action:ident) => {
            kb.insert(KeyBind { modifiers: vec![$(Modifier::$m),+], key: $key }, Action::$action);
        };
        ([], $key:expr, $action:ident) => {
            kb.insert(KeyBind { modifiers: vec![], key: $key }, Action::$action);
        };
    }

    bind!([Ctrl], Key::Character("o".into()), Open);
    bind!([Ctrl], Key::Character("s".into()), Save);
    bind!([Ctrl], Key::Character("q".into()), Quit);
    bind!([Ctrl], Key::Character("=".into()), ZoomIn);
    bind!([Ctrl], Key::Character("+".into()), ZoomIn);
    bind!([Ctrl], Key::Character("-".into()), ZoomIn);
    bind!([Ctrl], Key::Character("0".into()), ZoomFit);
    bind!([], Key::Named(Named::ArrowLeft), Prev);
    bind!([], Key::Named(Named::ArrowRight), Next);

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
        let img = image::open(path)?;
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
                    .is_some_and(|ext| IMAGE_EXTS.contains(&ext.to_lowercase().as_str()))
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
        let img = self.image.as_mut().ok_or_else(|| anyhow::anyhow!("No image"))?;
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
        let img = self.image.as_ref().ok_or_else(|| anyhow::anyhow!("No image"))?;
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
            "png" => {
                img.save(path)?;
            }
            "webp" => {
                img.save(path)?;
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
    SaveTo(Option<PathBuf>),
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
    Quit,
    About,
    Surface(cosmic::surface::Action),
}

// ── App ──

struct ViewApp {
    core: Core,
    engine: Arc<Mutex<ViewEngine>>,
    keybinds: HashMap<KeyBind, Action>,
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
    _port: Option<cosmix_port::PortHandle>,
}

impl ViewApp {
    fn rebuild_handle(&mut self) {
        let eng = self.engine.lock().unwrap();
        self.handle = eng.to_handle();
        let status = Self::build_status(&eng, self.zoom);
        self.status = status;
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
        core.window.header_title = "Cosmix View".into();
        core.window.content_container = true;

        let engine = Arc::new(Mutex::new(ViewEngine::new()));
        let port = start_port(engine.clone());

        let mut app = Self {
            core,
            engine,
            keybinds: key_binds(),
            handle: None,
            status: "No image loaded — Ctrl+O to open".into(),
            zoom: 1.0,
            edit_mode: EditMode::None,
            crop_x: "0".into(),
            crop_y: "0".into(),
            crop_w: String::new(),
            crop_h: String::new(),
            scale_w: String::new(),
            scale_h: String::new(),
            quality: "90".into(),
            _port: port,
        };

        if let Some(path) = flags {
            let mut eng = app.engine.lock().unwrap();
            if eng.open(&path).is_ok() {
                drop(eng);
                app.rebuild_handle();
            }
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
        vec![cosmic::widget::responsive_menu_bar()
            .item_height(ItemHeight::Dynamic(40))
            .item_width(ItemWidth::Uniform(280))
            .spacing(4.0)
            .into_element(
                self.core(),
                &self.keybinds,
                MENU_ID.clone(),
                Msg::Surface,
                vec![
                    (
                        "File".into(),
                        vec![
                            menu::Item::Button("Open", Some(icon::from_name("document-open-symbolic").into()), Action::Open),
                            menu::Item::Button("Save As", Some(icon::from_name("document-save-as-symbolic").into()), Action::Save),
                            menu::Item::Divider,
                            menu::Item::Button("Quit", Some(icon::from_name("window-close-symbolic").into()), Action::Quit),
                        ],
                    ),
                    (
                        "Edit".into(),
                        vec![
                            menu::Item::Button("Rotate Left", Some(icon::from_name("object-rotate-left-symbolic").into()), Action::RotateLeft),
                            menu::Item::Button("Rotate Right", Some(icon::from_name("object-rotate-right-symbolic").into()), Action::RotateRight),
                            menu::Item::Divider,
                            menu::Item::Button("Flip Horizontal", Some(icon::from_name("object-flip-horizontal-symbolic").into()), Action::FlipH),
                            menu::Item::Button("Flip Vertical", Some(icon::from_name("object-flip-vertical-symbolic").into()), Action::FlipV),
                            menu::Item::Divider,
                            menu::Item::Button("Crop", None, Action::Crop),
                            menu::Item::Button("Scale", None, Action::Scale),
                        ],
                    ),
                    (
                        "View".into(),
                        vec![
                            menu::Item::Button("Zoom In", Some(icon::from_name("zoom-in-symbolic").into()), Action::ZoomIn),
                            menu::Item::Button("Zoom Out", Some(icon::from_name("zoom-out-symbolic").into()), Action::ZoomOut),
                            menu::Item::Button("Fit to Window", Some(icon::from_name("zoom-fit-best-symbolic").into()), Action::ZoomFit),
                            menu::Item::Divider,
                            menu::Item::Button("Previous Image", None, Action::Prev),
                            menu::Item::Button("Next Image", None, Action::Next),
                        ],
                    ),
                    (
                        "Help".into(),
                        vec![
                            menu::Item::Button("About Cosmix View", Some(icon::from_name("help-about-symbolic").into()), Action::About),
                        ],
                    ),
                ],
            )]
    }

    fn update(&mut self, message: Self::Message) -> cosmic::app::Task<Self::Message> {
        match message {
            Msg::Open => {
                return cosmic::app::Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .add_filter("Images", IMAGE_EXTS)
                            .pick_file()
                            .await
                            .map(|f| f.path().to_path_buf())
                    },
                    |result| cosmic::Action::App(Msg::Opened(result)),
                );
            }
            Msg::Opened(Some(path)) => {
                let mut eng = self.engine.lock().unwrap();
                if let Err(e) = eng.open(&path) {
                    self.status = format!("Error: {e}");
                    return cosmic::app::Task::none();
                }
                drop(eng);
                self.zoom = 1.0;
                self.rebuild_handle();
            }
            Msg::Opened(None) => {}
            Msg::Save => {
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
                let eng = self.engine.lock().unwrap();
                let q: u8 = self.quality.parse().unwrap_or(90);
                match eng.save(&path, q) {
                    Ok(_) => self.status = format!("Saved: {}", path.display()),
                    Err(e) => self.status = format!("Save error: {e}"),
                }
            }
            Msg::SaveTo(None) => {}
            Msg::Next => {
                let mut eng = self.engine.lock().unwrap();
                if let Err(e) = eng.navigate(1) {
                    self.status = format!("Error: {e}");
                    return cosmic::app::Task::none();
                }
                drop(eng);
                self.zoom = 1.0;
                self.rebuild_handle();
            }
            Msg::Prev => {
                let mut eng = self.engine.lock().unwrap();
                if let Err(e) = eng.navigate(-1) {
                    self.status = format!("Error: {e}");
                    return cosmic::app::Task::none();
                }
                drop(eng);
                self.zoom = 1.0;
                self.rebuild_handle();
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
            }
            Msg::ZoomOut => {
                self.zoom = (self.zoom / 1.25).max(0.1);
                self.update_status_from_engine();
            }
            Msg::ZoomFit => {
                self.zoom = 1.0;
                self.update_status_from_engine();
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
                    self.rebuild_handle();
                } else {
                    self.status = "Invalid dimensions".into();
                }
            }
            Msg::CancelEdit => {
                self.edit_mode = EditMode::None;
            }
            Msg::QualityInput(s) => self.quality = s,
            Msg::Quit => return cosmic::iced::exit(),
            Msg::About => {}
            Msg::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(a),
                ));
            }
        }
        cosmic::app::Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let mut content = column::with_capacity(4).spacing(4);

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
                    .push(button::suggested("Apply".to_string()).on_press(Msg::ApplyCrop))
                    .push(button::standard("Cancel".to_string()).on_press(Msg::CancelEdit));
                content = content.push(crop_row);
            }
            EditMode::Scale => {
                let scale_row = row::with_capacity(6)
                    .spacing(4)
                    .push(text::body("W:"))
                    .push(num_input("0", &self.scale_w, Msg::ScaleW))
                    .push(text::body("H:"))
                    .push(num_input("0", &self.scale_h, Msg::ScaleH))
                    .push(button::suggested("Apply".to_string()).on_press(Msg::ApplyScale))
                    .push(button::standard("Cancel".to_string()).on_press(Msg::CancelEdit));
                content = content.push(scale_row);
            }
            EditMode::None => {}
        }

        // Image display
        if let Some(handle) = &self.handle {
            let eng = self.engine.lock().unwrap();
            let img_widget = if self.zoom == 1.0 {
                iced_image::Image::new(handle.clone())
                    .content_fit(ContentFit::Contain)
                    .width(Length::Fill)
                    .height(Length::Fill)
            } else if let Some((w, h)) = eng.dimensions() {
                let dw = (w as f32 * self.zoom) as f32;
                let dh = (h as f32 * self.zoom) as f32;
                iced_image::Image::new(handle.clone())
                    .content_fit(ContentFit::None)
                    .width(Length::Fixed(dw))
                    .height(Length::Fixed(dh))
            } else {
                iced_image::Image::new(handle.clone())
                    .content_fit(ContentFit::Contain)
                    .width(Length::Fill)
                    .height(Length::Fill)
            };
            drop(eng);

            content = content.push(
                container(img_widget)
                    .width(Length::Fill)
                    .height(Length::Fill),
            );
        } else {
            content = content.push(
                container(text::title4("No image loaded"))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            );
        }

        // Status bar
        content = content.push(text::caption(&self.status));

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

fn start_port(engine: Arc<Mutex<ViewEngine>>) -> Option<cosmix_port::PortHandle> {
    let e1 = engine.clone();
    let e2 = engine.clone();
    let e3 = engine.clone();
    let e4 = engine.clone();
    let e5 = engine.clone();
    let e6 = engine.clone();
    let e7 = engine.clone();
    let e8 = engine.clone();
    let e9 = engine.clone();

    let port = cosmix_port::Port::new("cosmix-view")
        .command("open", move |args| {
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
        .command("save", move |args| {
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
        .command("next", move |_| {
            let mut eng = e3.lock().unwrap();
            eng.navigate(1)?;
            Ok(serde_json::json!({ "file": eng.filename() }))
        })
        .command("prev", move |_| {
            let mut eng = e4.lock().unwrap();
            eng.navigate(-1)?;
            Ok(serde_json::json!({ "file": eng.filename() }))
        })
        .command("rotate", move |args| {
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
        .command("flip", move |args| {
            let dir = args.as_str().unwrap_or("h");
            let mut eng = e6.lock().unwrap();
            match dir {
                "h" | "horizontal" => eng.flip_h(),
                "v" | "vertical" => eng.flip_v(),
                _ => return Err(anyhow::anyhow!("unknown direction: {dir} (use h/v)")),
            }
            Ok(serde_json::json!("flipped"))
        })
        .command("crop", move |args| {
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
        .command("scale", move |args| {
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
        .command("info", move |_| {
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
        });

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

    let settings = Settings::default().size(Size::new(900.0, 650.0));

    cosmic::app::run::<ViewApp>(settings, path)?;

    Ok(())
}
