mod bus;
mod player;
use bevy::{
    asset::RenderAssetUsages,
    feathers::{
        FeathersPlugins,
        dark_theme::create_dark_theme,
        theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme},
    },
    picking::Pickable,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    ui_widgets::ScrollArea,
    window::{MonitorSelection, PrimaryWindow, WindowMode},
    winit::{UpdateMode, WinitSettings},
};
use ctk::prelude::*;
use ctk::theme::tokens;
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
}
fn required_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next().unwrap_or_else(|| {
        eprintln!("cosmix-media: {flag} requires a value");
        std::process::exit(2)
    })
}
#[derive(Component, Clone)]
enum Control {
    Media(Action),
    Volume(f64),
    Mute,
    Fullscreen,
}

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
                    "cosmix-media [FILE] [--directory DIR] [--service NAME]\nNative Wayland/CTK MP3/MP4 player. Space: pause; arrows: seek 10s; M: mute; F: fullscreen.\nBus: media.open/play/pause/toggle/stop/seek/volume/mute/status/props.get/quit.\nRequires GStreamer playbin, appsink, pulsesink and file codecs. Video currently uses CPU RGBA upload."
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
            DcsAppShellPlugin,
        ))
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(16)),
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_millis(33)),
        })
        .add_systems(Startup, setup)
        .add_systems(Update, (controls, keyboard, refresh))
        .run();
}
fn button(commands: &mut Commands, label: &str, control: Control) -> Entity {
    let text = commands
        .spawn((
            Text::new(label),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT),
            Pickable::IGNORE,
        ))
        .id();
    commands
        .spawn((
            Button,
            control,
            Node {
                padding: UiRect::axes(px(10), px(8)),
                min_height: px(32),
                flex_shrink: 0.0,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
        ))
        .add_child(text)
        .id()
}
fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut state: ResMut<ThemeState>,
    mut images: ResMut<Assets<Image>>,
    options: Res<Options>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);
    let buttons = [
        ("Play / Pause", Control::Media(Action::Toggle)),
        ("Stop", Control::Media(Action::Stop)),
        ("−10s", Control::Media(Action::Relative(-10.0))),
        ("+10s", Control::Media(Action::Relative(10.0))),
        ("Volume −", Control::Volume(-0.1)),
        ("Volume +", Control::Volume(0.1)),
        ("Mute", Control::Mute),
        ("Fullscreen", Control::Fullscreen),
    ]
    .map(|(name, action)| button(&mut commands, name, action));
    let toolbar = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            column_gap: px(6),
            padding: UiRect {
                left: px(110),
                right: px(90),
                top: px(6),
                bottom: px(6),
            },
            ..default()
        })
        .add_children(&buttons)
        .id();
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
                height: percent(100),
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
    let mut paths: Vec<_> = std::fs::read_dir(&options.directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    ["mp3", "mp4", "m4a", "wav", "ogg", "webm"]
                        .contains(&e.to_ascii_lowercase().as_str())
                })
        })
        .take(128)
        .collect();
    paths.sort();
    let mut items = vec![
        commands
            .spawn((
                Text::new(format!("Files in {}", options.directory.display())),
                TextFont::from_font_size(12.0),
                ThemeTextColor(tokens::TEXT),
                Node {
                    padding: UiRect::all(px(8)),
                    ..default()
                },
            ))
            .id(),
    ];
    for path in paths {
        let label = path.file_name().unwrap().to_string_lossy().into_owned();
        items.push(button(
            &mut commands,
            &label,
            Control::Media(Action::Open(path)),
        ));
    }
    let files = commands
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            row_gap: px(5),
            overflow: Overflow::scroll_y(),
            ..default()
        })
        .add_children(&items)
        .insert(ScrollArea)
        .id();
    let status = spawn_status_bar(
        &mut commands,
        "Choose a file · MP3 / MP4 · Space: play/pause",
    );
    spawn_dcs_app_shell(
        &mut commands,
        DcsAppShellProps::new(DcsShellProps::new(
            toolbar,
            centre,
            vec![DcsPanel::new("media", "Media files", files)],
            vec![],
        ))
        .with_status_bar(status.root),
    );
    commands.insert_resource(View {
        image,
        video,
        centre,
        status: status.text,
        generation: 0,
        last_status: String::new(),
    });
}
fn act(control: &Control, playback: &Playback, _window: &mut Window) {
    match control {
        Control::Media(action) => playback.0.send(action.clone()),
        Control::Volume(delta) => {
            let value = (playback.0.shared.lock().unwrap().status.volume + delta).clamp(0.0, 1.0);
            playback.0.send(Action::Volume(value));
        }
        Control::Mute => {
            let value = !playback.0.shared.lock().unwrap().status.muted;
            playback.0.send(Action::Mute(value));
        }
        Control::Fullscreen => {
            let value = !playback.0.shared.lock().unwrap().status.fullscreen;
            playback.0.send(Action::Fullscreen(value));
        }
    }
}
fn controls(
    query: Query<(&Interaction, &Control), Changed<Interaction>>,
    playback: Res<Playback>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    for (interaction, control) in &query {
        if *interaction == Interaction::Pressed {
            act(control, &playback, &mut window);
        }
    }
}
fn keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    playback: Res<Playback>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    for (key, control) in [
        (KeyCode::Space, Control::Media(Action::Toggle)),
        (KeyCode::ArrowLeft, Control::Media(Action::Relative(-10.0))),
        (KeyCode::ArrowRight, Control::Media(Action::Relative(10.0))),
        (KeyCode::KeyM, Control::Mute),
        (KeyCode::KeyF, Control::Fullscreen),
    ] {
        if keys.just_pressed(key) {
            act(&control, &playback, &mut window);
        }
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
