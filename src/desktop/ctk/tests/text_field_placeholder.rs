use bevy::ecs::world::CommandQueue;
use bevy::prelude::*;
use bevy::text::EditableText;
use ctk::text_field::{
    spawn_text_field, CtkTextFieldPlaceholder, CtkTextFieldPlugin, CtkTextFieldProps,
};

#[test]
fn placeholder_is_separate_from_value_and_tracks_empty_input() {
    let mut app = App::new();
    app.add_plugins(CtkTextFieldPlugin);
    let mut queue = CommandQueue::default();
    let field = spawn_text_field(
        &mut Commands::new(&mut queue, app.world()),
        CtkTextFieldProps::new("", "Search").placeholder("search…"),
    );
    queue.apply(app.world_mut());
    app.update();
    assert!(app
        .world()
        .get::<EditableText>(field.input)
        .unwrap()
        .value()
        .to_string()
        .is_empty());
    let hint = app
        .world_mut()
        .query_filtered::<Entity, With<CtkTextFieldPlaceholder>>()
        .single(app.world())
        .unwrap();
    assert_eq!(app.world().get::<Text>(hint).unwrap().0, "search…");
    assert_eq!(
        *app.world().get::<Visibility>(hint).unwrap(),
        Visibility::Inherited
    );

    app.world_mut()
        .entity_mut(field.input)
        .insert(EditableText::new("query"));
    app.update();
    assert_eq!(
        *app.world().get::<Visibility>(hint).unwrap(),
        Visibility::Hidden
    );
    app.world_mut()
        .entity_mut(field.input)
        .insert(EditableText::new(""));
    app.update();
    assert_eq!(
        *app.world().get::<Visibility>(hint).unwrap(),
        Visibility::Inherited
    );
}
