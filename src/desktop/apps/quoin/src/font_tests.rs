use super::*;
use bevy::text::{FontCx, FontSize};
use ctk::theme::{CtkTextRole, CtkTypography};

mod font_probe {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/font_probe.rs"
    ));
}

fn labels(fonts: FontCx) -> App {
    let mut app = App::new();
    app.insert_resource(fonts)
        .insert_resource(CtkTypography::without_environment())
        .add_plugins(CtkThemePlugin::isolated())
        .add_systems(Startup, |mut commands: Commands| {
            bottom_launcher(&mut commands);
            placeholder(&mut commands, "Panel heading", "Secondary label", false);
            ctk::menu::spawn_menu_bar(
                &mut commands,
                &[ctk::menu::MenuDef {
                    label: "Menu title".into(),
                    items: vec![ctk::menu::MenuItemDef::new("font-sample", "Menu item")],
                }],
            );
            ctk::menu::spawn_context_menu(
                &mut commands,
                &[ctk::menu::MenuItemDef::new("font-sample", "Context item")],
                Vec2::ZERO,
                None,
            );
        });
    app.update();
    app
}

fn verify(app: &mut App, sf: bool, suffix: &str) {
    let clock_font = app
        .world_mut()
        .query_filtered::<&TextFont, With<QuoinClock>>()
        .single(app.world())
        .unwrap()
        .clone();
    assert_eq!(clock_font.font_size, FontSize::Px(16.0));
    for scale in [1.0, 2.5] {
        font_probe::assert_face_and_render(
            &mut app.world_mut().resource_mut::<FontCx>(),
            &clock_font,
            "12:34:56 UTC",
            if sf { "SF Mono" } else { "Fira Mono" },
            if sf { 300 } else { 500 }, // Bevy's Fira fixture is Medium
            scale,
            &format!("quoin-clock-{suffix}"),
        );
    }
    let samples: Vec<_> = app
        .world_mut()
        .query::<(&Text, &TextFont, &CtkTextRole)>()
        .iter(app.world())
        .filter(|(text, _, _)| {
            matches!(
                text.0.as_str(),
                "Panel heading" | "Secondary label" | "Menu title" | "Menu item" | "Context item"
            )
        })
        .map(|(text, font, role)| (text.0.clone(), font.clone(), *role))
        .collect();
    assert_eq!(
        samples.len(),
        5,
        "exercise actual panel, bar, dropdown, context and small spawners"
    );
    for (sample, font, role) in samples {
        let small = role == CtkTextRole::Small;
        assert_eq!(
            font.font_size,
            FontSize::Px(if small { 32.0 / 3.0 } else { 44.0 / 3.0 })
        );
        assert_eq!(font.weight.0, if sf && !small { 300 } else { 400 });
        for scale in [1.0, 2.5] {
            font_probe::assert_face_and_render(
                &mut app.world_mut().resource_mut::<FontCx>(),
                &font,
                &sample,
                if sf { "SF Pro Text" } else { "DejaVu Sans" },
                if sf && !small { 300 } else { 400 },
                scale,
                &format!("quoin-{}-{suffix}", sample.replace(' ', "-").to_lowercase()),
            );
        }
    }
}

#[test]
fn panel_menu_and_small_shape_installed_sf_faces_and_render() {
    if !font_probe::sf_installed() {
        return;
    }
    let mut app = labels(FontCx::default());
    verify(&mut app, true, "sf");
    app.update();
    verify(&mut app, true, "sf-second-frame"); // exact sizes must not compound
}

#[test]
fn panel_menu_and_small_without_sf_use_explicit_free_family() {
    let mut app = labels(font_probe::free_fonts_only());
    verify(&mut app, false, "free");
}

#[test]
fn role_sizes_and_weights_survive_theme_reload_without_proportional_scaling() {
    let mut app = labels(font_probe::free_fonts_only());
    let mut spec = ThemeSpec::builtin();
    spec.typography.body_px = 26.0;
    spec.typography.weight = 500;
    app.world_mut().write_message(ctk::theme::ApplyTheme(spec));
    app.update();
    app.update();
    for (font, role) in app
        .world_mut()
        .query::<(&TextFont, &CtkTextRole)>()
        .iter(app.world())
    {
        if *role == CtkTextRole::Mono {
            assert_eq!(font.font_size, FontSize::Px(16.0));
            assert_eq!(font.weight.0, 400);
            continue;
        }
        let small = *role == CtkTextRole::Small;
        assert_eq!(
            font.font_size,
            FontSize::Px(if small { 32.0 / 3.0 } else { 26.0 })
        );
        assert_eq!(font.weight.0, if small { 400 } else { 500 });
    }
}
