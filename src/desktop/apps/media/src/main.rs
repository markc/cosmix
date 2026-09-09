mod bus;
mod player;
use bevy::{
    asset::RenderAssetUsages,
    feathers::{FeathersPlugins, dark_theme::create_dark_theme, theme::UiTheme},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    window::{MonitorSelection, PrimaryWindow, WindowMode},
    winit::{UpdateMode, WinitSettings},
};
use ctk::prelude::*;
use player::{Action, Player};
use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

#[derive(Resource)]
struct Playback(Player);
#[derive(Resource)]
struct Options {
    directory: PathBuf,
    path: Option<PathBuf>,
    service: String,
}
#[derive(Resource)]
struct View {
    image: Handle<Image>,
    video: Entity,
    centre: Entity,
    status: Entity,
    generation: u64,
    last_status: String,
    chrome: [Entity; 2],
}
fn required_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next().unwrap_or_else(|| {
        eprintln!("cosmix-media: {flag} requires a value");
        std::process::exit(2)
    })
}
#[derive(Resource, Default)]
struct OpenPending(bool);

fn main() {
    let mut options = Options {
        directory: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Downloads"),
        path: None,
        service: "media".into(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => {
                println!("cosmix-media {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" => {
                println!(
                    "cosmix-media [FILE] [--directory DIR] [--service NAME]\nNative Wayland/CTK MP3/MP4 player. Ctrl+O: open; Space: pause; arrows: seek 10s; M: mute; F: fullscreen; Escape: exit fullscreen.\nBus: media.open/play/pause/toggle/stop/seek/volume/mute/fullscreen/fullscreen.toggle/status/props.get/quit.\nRequires GStreamer playbin, appsink, pulsesink and file codecs. Video currently uses CPU RGBA upload."
                );
                return;
            }
            "--directory" => {
                options.directory = PathBuf::from(required_arg(&mut args, "--directory"))
            }
            "--service" => options.service = required_arg(&mut args, "--service"),
            _ if arg.starts_with('-') => {
                eprintln!("unknown option: {arg}");
                std::process::exit(2)
            }
            _ if options.path.is_none() => options.path = Some(PathBuf::from(arg)),
            _ => {
                eprintln!("only one initial media file is supported");
                std::process::exit(2)
            }
        }
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("cosmix-media requires a Wayland session");
        std::process::exit(1)
    }
    let player = Player::start();
    bus::start(&player, options.service.clone());
    if let Some(path) = &options.path {
        player.send(Action::Open(path.clone()));
    }
    let mut app = App::new();
    app.insert_resource(Playback(player))
        .insert_resource(options)
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CosMix Media".into(),
                name: Some("dev.cosmix.media".into()),
                resolution: (1100, 720).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            CtkWidgetsPlugin,
            MenuBarPlugin,
            FileRequesterPlugin,
        ))
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(16)),
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_millis(33)),
        })
        .init_resource::<OpenPending>()
        .add_observer(on_menu)
        .add_systems(Startup, setup)
        .add_systems(Update, open_shortcut.before(FileRequesterSystems))
        .add_systems(Update, (file_results, keyboard).after(FileRequesterSystems))
        .add_systems(Update, refresh)
        .run();
}

fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut state: ResMut<ThemeState>,
    mut images: ResMut<Assets<Image>>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);
    let menus = [
        MenuDef {
            label: "File".into(),
            items: vec![
                MenuItemDef::new("file.open", "Open…"),
                MenuItemDef::new("app.quit", "Quit"),
            ],
        },
        MenuDef {
            label: "Playback".into(),
            items: vec![
                MenuItemDef::new("playback.toggle", "Play / Pause"),
                MenuItemDef::new("playback.stop", "Stop"),
                MenuItemDef::new("playback.back", "Back 10 seconds"),
                MenuItemDef::new("playback.forward", "Forward 10 seconds"),
            ],
        },
        MenuDef {
            label: "Audio".into(),
            items: vec![
                MenuItemDef::new("audio.down", "Decrease volume"),
                MenuItemDef::new("audio.up", "Increase volume"),
                MenuItemDef::new("audio.mute", "Toggle mute"),
            ],
        },
        MenuDef {
            label: "View".into(),
            items: vec![MenuItemDef::new("view.fullscreen", "Toggle fullscreen")],
        },
    ];
    let menu = spawn_menu_bar(&mut commands, &menus);
    let image = images.add(Image::new_fill(
        Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[12, 14, 18, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    ));
    let video = commands
        .spawn((
            ImageNode::new(image.clone()),
            Node {
                max_width: percent(100),
                max_height: percent(100),
                ..default()
            },
        ))
        .id();
    let centre = commands
        .spawn((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                flex_basis: px(0),
                min_width: px(0),
                min_height: px(0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(Color::srgb(0.047, 0.055, 0.07)),
        ))
        .add_child(video)
        .id();
    let status = spawn_status_bar(&mut commands, "File → Open… to choose media · Ctrl+O");
    // The media player deliberately uses a conventional menu layout, without
    // DCS panel furniture, at the user's request.
    commands
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .add_children(&[menu, centre, status.root]);
    commands.insert_resource(View {
        image,
        video,
        centre,
        status: status.text,
        generation: 0,
        last_status: String::new(),
        chrome: [menu, status.root],
    });
}

fn request_open(
    options: &Options,
    pending: &mut OpenPending,
    requests: &mut MessageWriter<FileRequest>,
) {
    if pending.0 {
        return;
    }
    let mut request = FileRequest::open_file(FileRequestId(1), "Open media");
    request.initial_directory = Some(options.directory.clone());
    request.filters = vec![
        FileFilter::new(
            "Audio and video",
            ["mp3", "mp4", "m4a", "wav", "ogg", "webm"],
        ),
        FileFilter::new("All files", std::iter::empty::<String>()),
    ];
    pending.0 = true;
    requests.write(request);
}
fn on_menu(
    event: On<MenuActivated>,
    playback: Res<Playback>,
    options: Res<Options>,
    mut pending: ResMut<OpenPending>,
    mut requests: MessageWriter<FileRequest>,
) {
    if event.id == "file.open" {
        request_open(&options, &mut pending, &mut requests);
        return;
    }
    let action = match event.id {
        "app.quit" => Action::Quit,
        "playback.toggle" => Action::Toggle,
        "playback.stop" => Action::Stop,
        "playback.back" => Action::Relative(-10.0),
        "playback.forward" => Action::Relative(10.0),
        "audio.down" | "audio.up" => {
            let delta = if event.id == "audio.up" { 0.1 } else { -0.1 };
            Action::Volume(
                (playback.0.shared.lock().unwrap().status.volume + delta).clamp(0.0, 1.0),
            )
        }
        "audio.mute" => Action::Mute(!playback.0.shared.lock().unwrap().status.muted),
        "view.fullscreen" => Action::ToggleFullscreen,
        _ => return,
    };
    playback.0.send(action);
}
fn open_shortcut(
    keys: Res<ButtonInput<KeyCode>>,
    capture: Res<ModalCapture>,
    options: Res<Options>,
    mut pending: ResMut<OpenPending>,
    mut requests: MessageWriter<FileRequest>,
) {
    if !capture.is_captured()
        && (keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight))
        && keys.just_pressed(KeyCode::KeyO)
    {
        request_open(&options, &mut pending, &mut requests);
    }
}
fn file_results(
    mut results: MessageReader<FileRequestResult>,
    mut pending: ResMut<OpenPending>,
    mut options: ResMut<Options>,
    playback: Res<Playback>,
) {
    for result in results.read() {
        if result.id != FileRequestId(1) {
            continue;
        }
        pending.0 = false;
        match &result.outcome {
            FileRequestOutcome::Selected(paths) => {
                if let Some(path) = paths.first() {
                    if let Some(parent) = path.parent() {
                        options.directory = parent.to_path_buf();
                    }
                    playback.0.send(Action::Open(path.clone()));
                }
            }
            FileRequestOutcome::Failed(error) => {
                playback.0.shared.lock().unwrap().status.error = Some(error.clone())
            }
            FileRequestOutcome::Cancelled => {}
        }
    }
}
fn keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    capture: Res<ModalCapture>,
    pending: Res<OpenPending>,
    playback: Res<Playback>,
) {
    if capture.is_captured()
        || pending.0
        || keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight)
        || keys.pressed(KeyCode::AltLeft)
        || keys.pressed(KeyCode::AltRight)
    {
        return;
    }
    for (key, action) in [
        (KeyCode::Space, Action::Toggle),
        (KeyCode::ArrowLeft, Action::Relative(-10.0)),
        (KeyCode::ArrowRight, Action::Relative(10.0)),
    ] {
        if keys.just_pressed(key) {
            playback.0.send(action);
        }
    }
    if keys.just_pressed(KeyCode::KeyM) {
        let muted = playback.0.shared.lock().unwrap().status.muted;
        playback.0.send(Action::Mute(!muted));
    }
    if keys.just_pressed(KeyCode::KeyF) {
        playback.0.send(Action::ToggleFullscreen);
    }
    if keys.just_pressed(KeyCode::Escape) {
        playback.0.send(Action::Fullscreen(false));
    }
}
fn refresh(
    playback: Res<Playback>,
    mut view: ResMut<View>,
    mut images: ResMut<Assets<Image>>,
    mut nodes: Query<(&mut Node, &ComputedNode)>,
    mut texts: Query<&mut Text>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut exit: MessageWriter<AppExit>,
) {
    if playback.0.quit.load(Ordering::Relaxed) {
        exit.write(AppExit::Success);
        return;
    }
    let (status, frame) = {
        let mut shared = playback.0.shared.lock().unwrap();
        (shared.status.clone(), shared.frame.take())
    };
    for entity in view.chrome {
        if let Ok((mut node, _)) = nodes.get_mut(entity) {
            let display = if status.fullscreen {
                Display::None
            } else {
                Display::Flex
            };
            if node.display != display {
                node.display = display;
            }
        }
    }
    if let Ok(mut window) = windows.single_mut() {
        let mode = if status.fullscreen {
            WindowMode::BorderlessFullscreen(MonitorSelection::Current)
        } else {
            WindowMode::Windowed
        };
        if window.mode != mode {
            window.mode = mode;
        }
    }
    if view.generation != status.generation {
        view.generation = status.generation;
        if let Some(mut image) = images.get_mut(&view.image) {
            *image = Image::new_fill(
                Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                &[12, 14, 18, 255],
                TextureFormat::Rgba8UnormSrgb,
                RenderAssetUsages::default(),
            );
        }
    }
    if let Some(frame) = frame
        && let Some(mut image) = images.get_mut(&view.image)
    {
        *image = Image::new(
            Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            frame.pixels,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
    }
    let available = nodes
        .get(view.centre)
        .ok()
        .map(|(_, node)| node.size() * node.inverse_scale_factor());
    if status.width > 0
        && status.height > 0
        && let (Some(size), Ok((mut node, _))) = (available, nodes.get_mut(view.video))
    {
        let scale = (size.x / status.width as f32).min(size.y / status.height as f32);
        node.set_if_neq(Node {
            width: px(status.width as f32 * scale),
            height: px(status.height as f32 * scale),
            max_width: percent(100),
            max_height: percent(100),
            ..default()
        });
    }
    let label = if let Some(error) = &status.error {
        format!("{} · {error}", status.phase)
    } else if status.path.is_none() {
        "File → Open… to choose media · Ctrl+O".into()
    } else {
        format!(
            "{} · {:.1} / {:.1} s · volume {:.0}%{} · {}",
            status.phase,
            status.position,
            status.duration,
            status.volume * 100.0,
            if status.muted { " (muted)" } else { "" },
            status
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy())
                .unwrap_or_default()
        )
    };
    if label != view.last_status {
        if let Ok(mut text) = texts.get_mut(view.status) {
            text.0 = label.clone();
        }
        view.last_status = label;
    }
}
