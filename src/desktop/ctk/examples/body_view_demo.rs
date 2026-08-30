//! Human-facing Stage A exercise for CTK's sanitised message-body view.

use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme};
use bevy::feathers::{dark_theme::create_dark_theme, FeathersPlugins};
use bevy::picking::events::{Click, Pointer};
use bevy::picking::Pickable;
use bevy::prelude::*;
use ctk::prelude::*;
use ctk::theme::tokens;

const HOSTILE: &str = include_str!("../tests/fixtures/html/script-injection.html");
const TRACKING: &str = include_str!("../tests/fixtures/html/remote-tracking-pixel.html");

#[derive(Clone, Copy)]
struct Fixture {
    name: &'static str,
    source: fn() -> BodySource,
}

const FIXTURES: [Fixture; 4] = [
    Fixture {
        name: "Plain",
        source: || {
            BodySource::Plain(
                "Hello Mark,\n\nThis is an ordinary plain-text message.\nIt remains the simplest reading path.\n\nRegards,\nCTK"
                    .to_owned(),
            )
        },
    },
    Fixture {
        name: "Simple HTML",
        source: || {
            BodySource::Html(
                r#"<h2>Meeting notes</h2>
                   <p>Hello <strong>team</strong>, the <em>revised</em> notes are ready.</p>
                   <ul><li>First decision</li><li>Second decision</li></ul>
                   <blockquote><p>CTK emits links; the application decides what follows.</p></blockquote>
                   <p><a href="https://example.com/notes">Read the full notes</a>.</p>"#
                    .to_owned(),
            )
        },
    },
    Fixture {
        name: "Newsletter",
        source: || {
            BodySource::Html(format!(
                r#"<article>
                     <h1>Cosmix Weekly</h1>
                     <p><strong>Native tools, quieter infrastructure.</strong></p>
                     {TRACKING}
                     <h2>This week</h2>
                     <ol><li>Virtual lists</li><li>Compose text</li><li>Safe message reading</li></ol>
                     <table><tr><th>Package</th><th>Status</th></tr>
                       <tr><td>body_view</td><td>Stage A</td></tr></table>
                     <pre>RenderArm::Engine
    -> Text fallback</pre>
                     <p><a href="https://example.com/unsubscribe">Unsubscribe</a></p>
                   </article>"#
            ))
        },
    },
    Fixture {
        name: "Hostile",
        source: || BodySource::Html(HOSTILE.to_owned()),
    },
];

#[derive(Component)]
struct FixtureButton(usize);

#[derive(Component)]
struct ArmButton;

#[derive(Resource)]
struct Demo {
    holder: Entity,
    status: Entity,
    view: Entity,
    fixture: usize,
    arm: RenderArm,
    refs: usize,
    last_link: Option<String>,
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CTK body view — sanitised text arm".into(),
                resolution: (980, 760).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            CtkBodyViewPlugin,
        ))
        .add_systems(Startup, setup)
        .add_observer(on_control_click)
        .add_observer(report_link)
        .run();
}

fn setup(mut commands: Commands, mut theme: ResMut<UiTheme>, mut state: ResMut<ThemeState>) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let mut controls = Vec::new();
    for (index, fixture) in FIXTURES.iter().enumerate() {
        let button = demo_button(&mut commands, fixture.name);
        commands.entity(button).insert(FixtureButton(index));
        controls.push(button);
    }
    let arm_button = demo_button(&mut commands, "Toggle render arm");
    commands.entity(arm_button).insert(ArmButton);
    controls.push(arm_button);

    let toolbar = commands
        .spawn(Node {
            width: percent(100),
            height: px(36),
            align_items: AlignItems::Center,
            column_gap: px(7),
            ..default()
        })
        .add_children(&controls)
        .id();
    let help = commands
        .spawn((
            Text::new(
                "Tab to the document to copy the whole message, or click a block to copy that block; activate links with click, Enter or Space.",
            ),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id();
    let status = commands
        .spawn((
            Text::new(""),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id();
    let holder = commands
        .spawn(Node {
            width: percent(100),
            min_height: px(0),
            flex_grow: 1.0,
            ..default()
        })
        .id();

    let body = (FIXTURES[0].source)().sanitize();
    let refs = body.remote_refs().count();
    let view = spawn_body_view(
        &mut commands,
        CtkBodyViewProps::new(body, FIXTURES[0].name).viewport_height(620.0),
    );
    commands.entity(holder).add_child(view.root);

    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                padding: UiRect::all(px(16)),
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
        ))
        .add_children(&[toolbar, help, status, holder]);

    commands.insert_resource(Demo {
        holder,
        status,
        view: view.root,
        fixture: 0,
        arm: RenderArm::Text,
        refs,
        last_link: None,
    });
    commands.queue(update_status);
}

fn demo_button(commands: &mut Commands, label: &str) -> Entity {
    let text = commands
        .spawn((
            Text::new(label),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT),
            Pickable::IGNORE,
        ))
        .id();
    commands
        .spawn((
            Button,
            Pickable::default(),
            Node {
                height: px(30),
                padding: UiRect::horizontal(px(9)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
        ))
        .add_child(text)
        .id()
}

fn on_control_click(
    click: On<Pointer<Click>>,
    fixtures: Query<&FixtureButton>,
    arms: Query<(), With<ArmButton>>,
    mut demo: ResMut<Demo>,
    mut commands: Commands,
) {
    if let Ok(button) = fixtures.get(click.entity) {
        demo.fixture = button.0;
        let body = (FIXTURES[button.0].source)().sanitize();
        demo.refs = body.remote_refs().count();
        demo.last_link = None;
        commands.entity(demo.view).despawn();
        let view = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, FIXTURES[button.0].name)
                .render_arm(demo.arm)
                .viewport_height(620.0),
        );
        commands.entity(demo.holder).add_child(view.root);
        demo.view = view.root;
        commands.queue(update_status);
    } else if arms.contains(click.entity) {
        demo.arm = match demo.arm {
            RenderArm::Text => RenderArm::Engine,
            RenderArm::Engine => RenderArm::Text,
        };
        set_body_render_arm(&mut commands, demo.view, demo.arm);
        commands.queue(update_status);
    }
}

fn report_link(event: On<LinkActivated>, mut demo: ResMut<Demo>, mut commands: Commands) {
    println!("LinkActivated: {}", event.href);
    demo.last_link = Some(event.href.clone());
    commands.queue(update_status);
}

fn update_status(world: &mut World) {
    let (status, value) = {
        let demo = world.resource::<Demo>();
        let effective = world
            .get::<CtkBodyView>(demo.view)
            .map_or(RenderArm::Text, CtkBodyView::effective_arm);
        (
            demo.status,
            format!(
                "{} · RemoteRefs={} · requested={:?} effective={effective:?}{}",
                FIXTURES[demo.fixture].name,
                demo.refs,
                demo.arm,
                demo.last_link
                    .as_ref()
                    .map(|href| format!(" · last LinkActivated={href}"))
                    .unwrap_or_default()
            ),
        )
    };
    if let Some(mut text) = world.get_mut::<Text>(status) {
        text.0 = value;
    }
}
