// Frozen P0 mapping for geometry differential tests. Do not modernise this
// implementation alongside the renderer: it is the before-P1 reference.
// Source: render::update at 4a63223d582d81e14e914c18bdf9b873fbd3b003.
use super::super::*;

pub(super) fn update(
    world: &mut World,
    tree: &ResolvedScene,
    id: &str,
    node: &SceneNode,
    old: Option<&SceneNode>,
    view: &View,
) {
    let binding = Binding::new(tree, id, node);
    world
        .entity_mut(view.input.unwrap_or(view.root))
        .insert(binding);
    let mut layout = world.get::<Node>(view.root).cloned().unwrap_or_default();
    layout.width = node
        .ports
        .get("width")
        .and_then(Value::as_f64)
        .map_or(Val::Auto, |v| px(v as f32));
    layout.flex_grow = if flag(node, "fill") { 1.0 } else { 0.0 };
    layout.min_width = px(0);
    layout.display = if flag(node, "hidden")
        || (node.family == "list" && flag(node, "hidden_if_empty") && rows(node).is_empty())
    {
        Display::None
    } else {
        Display::Flex
    };
    match node.family.as_str() {
        "column" | "row" => {
            // Clear the height-derived constraint while preserving CTK-owned
            // baseline constraints on other widget families.
            layout.flex_shrink = Node::default().flex_shrink;
            layout.flex_direction = if node.family == "row" {
                FlexDirection::Row
            } else {
                FlexDirection::Column
            };
            layout.row_gap = px(number(node, "gap", 0.0));
            layout.column_gap = layout.row_gap;
            layout.padding = UiRect::all(px(number(node, "padding", 0.0)));
            layout.height = node
                .ports
                .get("height")
                .and_then(Value::as_f64)
                .map_or(Val::Auto, |v| px(v as f32));
            if node.ports.contains_key("height") {
                layout.flex_grow = 0.0;
                layout.flex_shrink = 0.0;
            }
            layout.border_radius = BorderRadius::all(px(number(node, "radius", 0.0)));
            layout.align_items = match text(node, "align") {
                "center" => AlignItems::Center,
                "end" => AlignItems::End,
                "stretch" => AlignItems::Stretch,
                _ => AlignItems::Start,
            };
            let normal = color(text(node, "background"), Color::NONE);
            world.entity_mut(view.root).insert((
                BackgroundColor(normal),
                Hovered::default(),
                SceneHover {
                    normal,
                    hover: color(text(node, "hover"), normal),
                },
            ));
            if node.ports.contains_key("on_click") {
                world.entity_mut(view.root).insert(ClickRow);
            } else {
                world.entity_mut(view.root).remove::<ClickRow>();
            }
        }
        "field" => {
            let input = view.input.unwrap();
            let focus = world.get_resource::<InputFocus>().and_then(InputFocus::get);
            if old.is_some_and(|old| old.ports.get("value") != node.ports.get("value"))
                && focus != Some(input)
                && let Some(mut editable) = world.get_mut::<EditableText>(input)
                && !editable.is_composing()
            {
                editable.editor_mut().set_text(text(node, "value"));
            }
            let hints: Vec<_> = world
                .query::<(Entity, &CtkTextFieldPlaceholder)>()
                .iter(world)
                .filter(|(_, hint)| hint.input == input)
                .map(|(e, _)| e)
                .collect();
            for hint in hints {
                world.get_mut::<Text>(hint).unwrap().0 = text(node, "placeholder").into();
            }
        }
        "button" => {
            if let Some(mut button) = world.get_mut::<ctk::button::CtkButton>(view.root) {
                button.variant = tone(node);
            }
            let children = world
                .get::<Children>(view.root)
                .map(|cs| cs.iter().collect::<Vec<_>>())
                .unwrap_or_default();
            for child in children {
                if let Some(mut label) = world.get_mut::<Text>(child) {
                    label.0 = text(node, "label").into();
                }
            }
        }
        "toggle" => {
            if flag(node, "value") {
                world.entity_mut(view.root).insert(Checked);
            } else {
                world.entity_mut(view.root).remove::<Checked>();
            }
            world.get_mut::<Text>(view.label.unwrap()).unwrap().0 = text(node, "label").into();
        }
        "text" => {
            let label = view.label.unwrap();
            world.get_mut::<TextLayout>(label).unwrap().justify = match text(node, "align") {
                "center" => Justify::Center,
                "right" => Justify::Right,
                _ => Justify::Left,
            };
            // Justification uses the wrapper's authored or flex-allocated width.
            world.get_mut::<Node>(label).unwrap().width =
                if node.ports.contains_key("width") || flag(node, "fill") {
                    percent(100)
                } else {
                    Val::Auto
                };
            if flag(node, "fill")
                && tree.nodes.values().any(|parent| {
                    parent.family == "column"
                        && text(parent, "align") == "stretch"
                        && children(parent).any(|child| child == id.split('@').next().unwrap_or(id))
                })
            {
                layout.align_self = AlignSelf::Stretch;
            } else {
                layout.align_self = AlignSelf::Auto;
            }
            world.entity_mut(label).insert((
                Text::new(text(node, "text")),
                TextFont::from_font_size(number(node, "size", 13.0)).with_font_weight(
                    if flag(node, "bold") {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    },
                ),
                TextColor(color(text(node, "color"), Color::WHITE)),
            ));
            if flag(node, "mono") {
                world.entity_mut(label).insert(ctk::theme::CtkMonospace);
            } else {
                world.entity_mut(label).remove::<ctk::theme::CtkMonospace>();
            }
            if flag(node, "elide") {
                world
                    .entity_mut(label)
                    .insert(ctk::text_elide::MiddleElideText::new(
                        text(node, "text"),
                        view.root,
                    ));
            } else {
                world
                    .entity_mut(label)
                    .remove::<ctk::text_elide::MiddleElideText>();
            }
        }
        "list" => {
            layout.height = px(list_height(node));
            layout.max_height = if node.ports.contains_key("max_rows") {
                layout.height
            } else {
                Val::Auto
            };
        }
        "image" => {
            let src = text(node, "src");
            let image = if src.starts_with('/') {
                icons::load(world, src, number(node, "w", 16.0), number(node, "h", 16.0))
            } else {
                world
                    .get_resource::<AssetServer>()
                    .map(|assets| assets.load::<Image>(src.to_owned()))
            };
            if let Some(image) = image {
                world.entity_mut(view.root).insert(ImageNode::new(image));
            } else if src.starts_with('/') {
                world.entity_mut(view.root).remove::<ImageNode>();
            }
            layout.width = px(number(node, "w", 16.0));
            layout.height = px(number(node, "h", 16.0));
        }
        "spacer" => {
            layout.width = node
                .ports
                .get("size")
                .and_then(Value::as_f64)
                .map_or(Val::Auto, |size| px(size as f32));
            layout.height = layout.width;
            layout.flex_shrink = 0.0;
            layout.flex_grow = if layout.width == Val::Auto { 1.0 } else { 0.0 };
        }
        "window" => {
            layout.width = px(number(node, "w", 0.0));
            layout.height = px(0);
        }
        _ => {}
    }
    world.entity_mut(view.root).insert(layout);
}
