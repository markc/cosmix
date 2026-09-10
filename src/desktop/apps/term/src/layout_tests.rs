use super::*;
use bevy::camera::{CameraPlugin, ComputedCameraValues, RenderTargetInfo, Viewport};
use bevy::ui::{UiPlugin, UiSystems};

#[test]
fn physical_borders_preserve_exact_cell_allocations_at_fractional_scales() {
    for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
        let painter = raster::Raster::new(scale, 13.0, config::Cursor::Underline).unwrap();
        // Asymmetric resolved borders ensure all four insets are respected.
        let border = BorderRect {
            min_inset: Vec2::new(1.0, 2.0),
            max_inset: Vec2::new(2.0, 1.0),
        };
        for (cols, rows) in [(2, 1), (79, 23), (80, 24), (137, 61)] {
            for delta in [-1.0, 0.0, 1.0] {
                let node = ComputedNode {
                    size: Vec2::new(
                        cols as f32 * painter.width as f32 + 3.0 + delta,
                        rows as f32 * painter.height as f32 + 3.0 + delta,
                    ),
                    border,
                    inverse_scale_factor: 1.0 / scale,
                    ..default()
                };
                let size = pane_interior(&node);
                let actual = (
                    (size.x / painter.logical_width()) as u16,
                    (size.y / painter.logical_height()) as u16,
                );
                let expected = if delta < 0.0 {
                    (cols - 1, rows - 1)
                } else {
                    (cols, rows)
                };
                assert_eq!(actual, expected, "scale={scale}, delta={delta}");
            }
        }
        assert_eq!(
            pane_interior(&ComputedNode {
                size: Vec2::ONE,
                border,
                ..default()
            }),
            Vec2::ZERO
        );
    }
}

#[derive(Resource, Default)]
struct BetweenSchedules(Option<Mutation>);

enum Mutation {
    Split,
    Close,
    PaneSelect(u64),
    TabSelect(u64),
}

// A separate thread uses the same shared Arc/Mutex and TabSet methods as Bus.
// Joining fixes the interleaving after Update and before layout/refresh.
fn bus_mutation(core: Res<Core>, mut pending: ResMut<BetweenSchedules>) {
    let Some(mutation) = pending.0.take() else {
        return;
    };
    let shared = core.0.clone();
    let removed = std::thread::spawn(move || {
        let mut tabs = shared.lock().unwrap();
        match mutation {
            Mutation::Split => {
                tabs.split_active(panes::SplitDir::Vertical).unwrap();
                None
            }
            Mutation::Close => tabs.close_active().1,
            Mutation::PaneSelect(id) => {
                assert!(tabs.focus(id));
                None
            }
            Mutation::TabSelect(id) => {
                assert!(tabs.select(id));
                None
            }
        }
    })
    .join()
    .unwrap();
    core.1.submit(removed.into_iter().collect());
}

fn layout_app() -> App {
    assert!(
        std::path::Path::new("/opt/cosmix/bin/mix").is_file(),
        "scheduled PTY test requires Mix"
    );
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        bevy::asset::AssetPlugin::default(),
        bevy::transform::TransformPlugin,
        CameraPlugin,
        ImagePlugin::default(),
        bevy::image::TextureAtlasPlugin,
        bevy::mesh::MeshPlugin,
        bevy::input::InputPlugin,
        bevy::picking::PickingPlugin,
        bevy::picking::InteractionPlugin,
        bevy::text::TextPlugin,
        UiPlugin,
    ));
    app.world_mut().spawn((
        Camera2d,
        Camera {
            computed: ComputedCameraValues {
                target_info: Some(RenderTargetInfo {
                    physical_size: UVec2::new(1000, 700),
                    scale_factor: 1.25,
                }),
                ..default()
            },
            viewport: Some(Viewport {
                physical_size: UVec2::new(1000, 700),
                ..default()
            }),
            ..default()
        },
    ));
    let centre = app
        .world_mut()
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            ..default()
        })
        .id();
    let (cleanup, _) = tabs::Cleanup::start().unwrap();
    let settings = config::Settings {
        config: config::Config::default(),
        term: "xterm-256color",
    };
    app.insert_resource(Core(
        Arc::new(Mutex::new(TabSet::with_settings(settings).unwrap())),
        cleanup,
    ))
    .insert_resource(Painter(Mutex::new(
        raster::Raster::new(1.0, settings.config.font_px, settings.config.cursor).unwrap(),
    )))
    .insert_resource(settings)
    .init_resource::<InputFocus>()
    .init_resource::<BetweenSchedules>()
    .insert_resource(View {
        terminal: centre,
        pane_views: vec![],
        pane_root: None,
        tree_state: None,
        centre,
        menu: centre,
        dropdowns: vec![],
        menu_ids: vec![],
        menu_item: 0,
        tab_bar: centre,
        tab_buttons: vec![],
        tab_state: vec![],
        open_menu: None,
        scale: 1.0,
        last_frame: Instant::now(),
    })
    .add_systems(Update, sync_panes)
    .add_systems(PostUpdate, bus_mutation.before(UiSystems::Layout))
    .add_systems(PostUpdate, refresh.after(UiSystems::Layout));
    app.world_mut()
        .resource_mut::<InputFocus>()
        .set(centre, FocusCause::Navigated);
    app.finish();
    app.cleanup();
    app
}

fn assert_synced(app: &App) {
    let view = app.world().resource::<View>();
    let tabs = app.world().resource::<Core>().0.lock().unwrap();
    assert_eq!(
        view.tree_state,
        Some((tabs.active_id(), tabs.active_tab().revision))
    );
    assert!(tabs.pane_by_id(tabs.active_tab().active_pane).is_some());
    assert_eq!(view.pane_views.len(), tabs.leaves().len());
    assert!(app.world().get_entity(view.terminal).is_ok());
    assert_eq!(
        app.world().resource::<InputFocus>().get(),
        Some(view.terminal)
    );
    for pane in &view.pane_views {
        assert!(app.world().get_entity(pane.entity).is_ok());
        assert!(app.world().get_entity(pane.container).is_ok());
        assert!(pane.rendered, "real refresh must render every leaf");
        assert_eq!(pane.active, pane.id == tabs.active_tab().active_pane);
        let info = tabs
            .leaves()
            .into_iter()
            .find(|info| info.id == pane.id)
            .unwrap();
        assert!(
            info.geometry.w > 0.0 && info.geometry.h > 0.0,
            "real UI layout must run"
        );
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&pane.image)
            .unwrap();
        let painter = app.world().resource::<Painter>().0.lock().unwrap();
        assert_eq!(image.width(), pane.cols as u32 * painter.width);
        assert_eq!(image.height(), pane.rows as u32 * painter.height);
    }
}

#[test]
fn scheduled_refresh_survives_bus_mutations_between_update_and_post_update() {
    let mut app = layout_app();
    let shared = app.world().resource::<Core>().0.clone();
    let (first_tab, first_pane, other_tab) = {
        let mut tabs = shared.lock().unwrap();
        let first_tab = tabs.active_id();
        let first_pane = tabs.active_tab().active_pane;
        let other_tab = tabs.open().unwrap();
        assert!(tabs.select(first_tab));
        (first_tab, first_pane, other_tab)
    };
    app.update();
    assert_synced(&app);
    for mutation in [
        Mutation::Split,
        Mutation::PaneSelect(first_pane),
        Mutation::Close,
        Mutation::TabSelect(other_tab),
        Mutation::TabSelect(first_tab),
    ] {
        let old_state = app.world().resource::<View>().tree_state;
        let old_root = app.world().resource::<View>().pane_root.unwrap();
        let old_entities: Vec<_> = app
            .world()
            .resource::<View>()
            .pane_views
            .iter()
            .flat_map(|pane| [pane.container, pane.entity])
            .collect();
        app.world_mut().resource_mut::<BetweenSchedules>().0 = Some(mutation);
        app.update();
        // Update saw the old state. Structural changes must defer refresh.
        assert_eq!(app.world().resource::<View>().tree_state, old_state);
        let tabs = shared.lock().unwrap();
        assert!(tabs.pane_by_id(tabs.active_tab().active_pane).is_some());
        let changed = old_state != Some((tabs.active_id(), tabs.active_tab().revision));
        if !changed {
            // Pane selection is observed by real refresh in this same frame.
            for pane in &app.world().resource::<View>().pane_views {
                assert_eq!(pane.active, pane.id == tabs.active_tab().active_pane);
            }
        }
        drop(tabs);
        app.update();
        assert_synced(&app);
        if changed {
            assert!(app.world().get_entity(old_root).is_err());
            assert!(
                old_entities
                    .iter()
                    .all(|entity| app.world().get_entity(*entity).is_err())
            );
        }
        for pane in &app.world().resource::<View>().pane_views {
            let token = app
                .world()
                .get::<bevy::feathers::theme::ThemeBorderColor>(pane.container)
                .unwrap()
                .0.clone();
            assert_eq!(
                token,
                if pane.active {
                    ctk::theme::tokens::CONTROL_ACTIVE
                } else {
                    ctk::theme::tokens::BORDER
                }
            );
        }
    }
    let removed = shared.lock().unwrap().shutdown();
    app.world().resource::<Core>().1.submit(removed);
}
