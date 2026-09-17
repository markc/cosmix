//! Opt-in nested runtime probe. No production binary includes this module.
//! Set COSMIX_SCENE_EDIT_GATE to a scene name, wait for READY in the journal,
//! reload that scene through the Bus, then require PASS (including undo).
use super::SceneStore;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx, TextEdit};
use bevy::window::RequestRedraw;
use ctk::text_area::{CtkTextArea, CtkTextAreaUndo};

#[derive(Resource)]
struct Gate {
    scene: String,
    phase: usize,
    frames: usize,
    input: Option<Entity>,
    revision: u64,
    original: String,
    before: String,
}

pub(super) fn install(app: &mut App) {
    if std::env::var_os("COSMIX_SCENE_MEMORY_GATE").is_some() {
        app.add_systems(Last, memory_probe);
    }
    if let Ok(scene) = std::env::var("COSMIX_SCENE_EDIT_GATE") {
        app.insert_resource(Gate {
            scene,
            phase: 0,
            frames: 0,
            input: None,
            revision: 0,
            original: String::new(),
            before: String::new(),
        })
        .add_systems(Last, probe);
    }
}

fn memory_probe(
    time: Res<Time<Real>>,
    atlases: Res<bevy::text::FontAtlasSet>,
    images: Res<Assets<Image>>,
    mut previous: Local<(f64, u64)>,
) {
    if time.elapsed_secs_f64() - previous.0 < 5.0 {
        return;
    }
    let bytes = atlases.total_bytes(&images);
    if bytes != previous.1 {
        eprintln!(
            "SCENE_MEMORY_GATE atlas_bytes={bytes} keys={:?}",
            atlases.keys().collect::<Vec<_>>()
        );
    }
    *previous = (time.elapsed_secs_f64(), bytes);
}

fn snapshot(world: &World, input: Entity) -> String {
    let text = world.get::<EditableText>(input).unwrap();
    format!(
        "focus={:?};value={};selection={:?};compose={:?};state={:?}",
        world.resource::<InputFocus>().get(),
        text.value(),
        text.editor().raw_selection(),
        text.editor().raw_compose(),
        world.get::<CtkTextArea>(input).unwrap()
    )
}

fn probe(world: &mut World) {
    world.resource_scope(|world, mut gate: Mut<Gate>| {
        if gate.phase >= 7 { return; }
        let Some(entry) = world.resource::<SceneStore>().scenes.get(&gate.scene) else { return };
        let Some(input) = entry.mounted.as_ref().and_then(|m| m.input("search").or_else(|| m.input("field"))) else { return };
        let revision = entry.revision;
        gate.frames += 1;
        if gate.frames < 5 { world.write_message(RequestRedraw); return; }
        match gate.phase {
            0 => {
                gate.input = Some(input);
                gate.original = world.get::<EditableText>(input).unwrap().value().to_string();
                *world.resource_mut::<InputFocus>() = InputFocus::from_entity(input);
                let mut editable = world.get_mut::<EditableText>(input).unwrap();
                editable.queue_edit(TextEdit::SelectAll);
                editable.queue_edit(TextEdit::Insert("retained text".into()));
                gate.phase = 1;
            }
            1 => {
                world.resource_scope(|world, mut fonts: Mut<FontCx>| {
                    world.resource_scope(|world, mut layouts: Mut<LayoutCx>| {
                        world.get_mut::<EditableText>(input).unwrap().editor_mut()
                            .driver(&mut fonts.context, &mut layouts.0).select_byte_range(2,7);
                    });
                });
                gate.phase = 2;
            }
            2 => {
                world.get_mut::<EditableText>(input).unwrap().queue_edit(TextEdit::ImeSetCompose { value: "compose".into(), cursor: None });
                gate.phase = 3;
            }
            3 => {
                assert!(world.get::<EditableText>(input).unwrap().is_composing());
                gate.before = snapshot(world,input);
                gate.revision = revision;
                println!("SCENE_RETAINED_GATE READY scene={} revision={revision} focus=true selection=true composing=true", gate.scene);
                gate.phase = 4;
            }
            4 => {
                if revision == gate.revision { return; }
                assert_eq!(gate.input,Some(input),"field entity replaced");
                assert_eq!(gate.before,snapshot(world,input),"reload changed editing state");
                println!("SCENE_RETAINED_GATE reload PASS focus selection history composition");
                world.get_mut::<EditableText>(input).unwrap().queue_edit(TextEdit::clear_ime_compose());
                gate.phase = 5;
            }
            5 => {
                world.trigger(CtkTextAreaUndo { area:input });
                gate.phase = 6;
            }
            6 => {
                assert_eq!(world.get::<EditableText>(input).unwrap().value().to_string(),gate.original,"undo history did not survive");
                println!("SCENE_RETAINED_GATE PASS focus selection undo IME across Bus reload");
                gate.phase = 7;
            }
            _ => unreachable!(),
        }
        gate.frames = 0;
        if gate.phase != 4 && gate.phase != 7 { world.write_message(RequestRedraw); }
    });
}
