//! `ResolvedScene` → iced widgets, mirroring the CTK adapter
//! (`cosmix-scene-bevy/src/render.rs`) family by family.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use cosmix_iced_host::core::alignment::Vertical;
use cosmix_iced_host::core::font::{Family, Weight};
use cosmix_iced_host::core::text::Wrapping;
use cosmix_iced_host::core::widget::Id;
use cosmix_iced_host::core::{Background, Border, Color, Font, Length, mouse};
use cosmix_iced_host::widget::{
    button, container, keyed_column, mouse_area, row, scrollable, space, text, toggler,
};
use cosmix_iced_host::{Element, Program, Theme};
use cosmix_iced_widgets::{TextField, Tokens};
use cosmix_scene::{Node, ResolvedScene};
use serde_json::{Value, json};

use super::submit::on_submit;

/// A handler call the scene asks the host to send, as the CTK adapter's
/// `Binding` would: `citizen handler {scene, node, kind, value?, item?}`.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneAction {
    pub scene: String,
    pub citizen: String,
    pub node: String,
    pub kind: &'static str,
    pub handler: String,
    pub value: Option<Value>,
    pub item: Option<Value>,
}

pub type Outbox = Arc<Mutex<Vec<SceneAction>>>;

#[derive(Clone, Debug)]
pub enum Msg {
    Click { node: String, item: Option<Value> },
    Input { node: String, value: String },
    Submit { node: String },
    Toggle { node: String, value: bool },
    Hover { key: String, inside: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Row,
    Column,
}

/// Look shared by every node of one scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub tokens: Tokens,
    pub font: Font,
    pub text_px: f32,
}

pub struct SceneProgram {
    tree: ResolvedScene,
    templates: BTreeSet<String>,
    /// Field text owned by the surface, as CTK's `EditableText` is.
    fields: HashMap<String, String>,
    toggles: HashMap<String, bool>,
    hovered: HashSet<String>,
    look: Look,
    outbox: Outbox,
}

impl SceneProgram {
    pub fn new(look: Look, outbox: Outbox) -> Self {
        Self {
            tree: ResolvedScene {
                name: String::new(),
                citizen: String::new(),
                window: None,
                subscribe: None,
                nodes: Default::default(),
                templates: Vec::new(),
            },
            templates: BTreeSet::new(),
            fields: HashMap::new(),
            toggles: HashMap::new(),
            hovered: HashSet::new(),
            look,
            outbox,
        }
    }

    pub fn tree(&self) -> &ResolvedScene {
        &self.tree
    }

    pub fn field_value(&self, id: &str) -> Option<&str> {
        self.fields.get(id).map(String::as_str)
    }

    pub fn set_look(&mut self, look: Look) {
        self.look = look;
    }

    /// Accepts a new revision. A changed `value` port replaces a field's text
    /// only while the field is not focused, as CTK does; toggles follow their
    /// port whenever it changes.
    pub fn set_scene(&mut self, tree: &ResolvedScene, focused: &HashSet<String>) {
        for (id, node) in &tree.nodes {
            let old = self
                .tree
                .nodes
                .get(id)
                .filter(|old| old.family == node.family);
            let changed = old.is_none_or(|old| old.ports.get("value") != node.ports.get("value"));
            match node.family.as_str() {
                "field" if changed && !focused.contains(id) => {
                    self.fields
                        .insert(id.clone(), text_port(node, "value").to_owned());
                }
                "field" => {
                    self.fields
                        .entry(id.clone())
                        .or_insert_with(|| text_port(node, "value").to_owned());
                }
                "toggle" if changed => {
                    self.toggles.insert(id.clone(), flag(node, "value"));
                }
                _ => {}
            }
        }
        self.fields
            .retain(|id, _| tree.nodes.get(id).is_some_and(|n| n.family == "field"));
        self.toggles
            .retain(|id, _| tree.nodes.get(id).is_some_and(|n| n.family == "toggle"));
        self.templates = template_ids(tree);
        self.tree = tree.clone();
    }

    fn act(&self, node: &str, kind: &'static str, value: Option<Value>, item: Option<Value>) {
        let port = match kind {
            "click" => "on_click",
            "change" => "on_change",
            _ => "on_submit",
        };
        let Some(handler) = self.tree.nodes.get(node).and_then(|n| string(n, port)) else {
            return;
        };
        self.outbox.lock().unwrap().push(SceneAction {
            scene: self.tree.name.clone(),
            citizen: self.tree.citizen.clone(),
            node: node.to_owned(),
            kind,
            handler,
            value,
            item,
        });
    }

    fn node_view<'a>(
        &'a self,
        id: &str,
        parent: Axis,
        item: Option<&'a Value>,
    ) -> Element<'a, Msg> {
        let Some(node) = self.tree.nodes.get(id) else {
            return space().into();
        };
        // Template instances are keyed like CTK's `{id}@{row id}` names.
        let key = match item {
            Some(item) => format!("{id}@{}", item["id"].as_str().unwrap_or_default()),
            None => id.to_owned(),
        };
        let hidden = flag(node, "hidden")
            || (node.family == "list" && flag(node, "hidden_if_empty") && rows(node).is_empty());
        if hidden {
            return space().width(0.0).height(0.0).into();
        }
        let fill = flag(node, "fill");
        let boxed = matches!(node.family.as_str(), "row" | "column" | "list");
        let mut width = match number(node, "width") {
            Some(w) => Length::Fixed(w),
            None if fill && parent == Axis::Row => Length::Fill,
            // CTK columns stretch their children across.
            None if parent == Axis::Column && boxed => Length::Fill,
            None => Length::Shrink,
        };
        let mut height = if fill && parent == Axis::Column {
            Length::Fill
        } else {
            Length::Shrink
        };
        let look = self.look;
        let content: Element<'a, Msg> = match node.family.as_str() {
            "column" => {
                let children = children(node)
                    .map(|child| (hash(child), self.node_view(child, Axis::Column, item)));
                keyed_column(children)
                    .spacing(number(node, "gap").unwrap_or(0.0))
                    .padding(number(node, "padding").unwrap_or(0.0))
                    .width(Length::Fill)
                    .into()
            }
            "row" => {
                if let Some(h) = number(node, "height") {
                    height = Length::Fixed(h);
                }
                let align = match text_port(node, "align") {
                    "center" => Vertical::Center,
                    "end" => Vertical::Bottom,
                    _ => Vertical::Top,
                };
                let inner = row(children(node).map(|child| self.node_view(child, Axis::Row, item)))
                    .spacing(number(node, "gap").unwrap_or(0.0))
                    .padding(number(node, "padding").unwrap_or(0.0))
                    .align_y(align)
                    .width(width)
                    .height(height);
                let normal = colour(text_port(node, "background"));
                let hover = colour(text_port(node, "hover")).or(normal);
                let background = if self.hovered.contains(&key) {
                    hover
                } else {
                    normal
                };
                let radius = number(node, "radius").unwrap_or(0.0);
                let styled = container(inner).style(move |_| container::Style {
                    background: background.map(Background::Color),
                    border: Border {
                        radius: radius.into(),
                        ..Border::default()
                    },
                    ..container::Style::default()
                });
                let clickable = node.ports.contains_key("on_click");
                if clickable || node.ports.contains_key("hover") {
                    let mut area = mouse_area(styled)
                        .on_enter(Msg::Hover {
                            key: key.clone(),
                            inside: true,
                        })
                        .on_exit(Msg::Hover {
                            key: key.clone(),
                            inside: false,
                        });
                    if clickable && item.is_none() {
                        area = area
                            .on_press(Msg::Click {
                                node: id.to_owned(),
                                item: None,
                            })
                            .interaction(mouse::Interaction::Pointer);
                    }
                    area.into()
                } else {
                    styled.into()
                }
            }
            "text" => {
                let mut content = text_port(node, "text").to_owned();
                if let Some(item) = item {
                    content = substitute_cells(&content, &item["cells"]);
                }
                let font = Font {
                    family: if flag(node, "mono") {
                        Family::Monospace
                    } else {
                        look.font.family
                    },
                    weight: if flag(node, "bold") {
                        Weight::Bold
                    } else {
                        Weight::Normal
                    },
                    ..look.font
                };
                let label = text(content)
                    .size(number(node, "size").unwrap_or(13.0))
                    .font(font)
                    .wrapping(Wrapping::None)
                    .color(colour(text_port(node, "color")).unwrap_or(Color::WHITE));
                // No middle elision in iced 0.14: an elided label is clipped.
                container(label).clip(flag(node, "elide")).into()
            }
            "field" => {
                let node_id = id.to_owned();
                let value = self.fields.get(id).map_or("", String::as_str);
                let tokens = look.tokens;
                let field = TextField::new(text_port(node, "placeholder"), value)
                    .id(Id::from(key.clone()))
                    .secure(flag(node, "password"))
                    .size(look.text_px)
                    .style(move |_, status| tokens.text_input(status))
                    .on_input(move |value| Msg::Input {
                        node: node_id.clone(),
                        value,
                    });
                let field = match number(node, "width") {
                    Some(w) => field.width(w),
                    None => field,
                };
                on_submit(
                    field.into(),
                    Msg::Submit {
                        node: id.to_owned(),
                    },
                    Id::from(key.clone()),
                )
            }
            "button" => {
                let style: fn(&Theme, button::Status) -> button::Style =
                    match text_port(node, "tone") {
                        "primary" => button::primary,
                        "danger" => button::danger,
                        _ => button::secondary,
                    };
                let mut b = button(text(text_port(node, "label").to_owned()).size(look.text_px))
                    .on_press(Msg::Click {
                        node: id.to_owned(),
                        item: item.cloned(),
                    })
                    .style(style);
                if let Some(w) = number(node, "width").filter(|w| *w > 0.0) {
                    b = b.width(w);
                }
                b.into()
            }
            "toggle" => {
                let node_id = id.to_owned();
                toggler(self.toggles.get(id).copied().unwrap_or(false))
                    .label(text_port(node, "label").to_owned())
                    .text_size(look.text_px)
                    .on_toggle(move |value| Msg::Toggle {
                        node: node_id.clone(),
                        value,
                    })
                    .into()
            }
            "list" => {
                let row_height = number(node, "row_height").unwrap_or(24.0);
                let gap = number(node, "gap").unwrap_or(0.0);
                height = if fill && parent == Axis::Column {
                    Length::Fill
                } else {
                    Length::Fixed(list_height(node))
                };
                let template = text_port(node, "row");
                let instances = rows(node).iter().map(|row_item| {
                    let content = container(self.node_view(template, Axis::Column, Some(row_item)))
                        .width(Length::Fill)
                        .height(row_height);
                    let area = mouse_area(content).on_press(Msg::Click {
                        node: id.to_owned(),
                        item: Some(row_item.clone()),
                    });
                    (
                        hash(row_item["id"].as_str().unwrap_or_default()),
                        area.into(),
                    )
                });
                scrollable(keyed_column(instances).spacing(gap).width(Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
            "image" => {
                // Not rendered: iced_widget's image feature is off. Keeps the box.
                width = Length::Fixed(number(node, "w").unwrap_or(16.0));
                height = Length::Fixed(number(node, "h").unwrap_or(16.0));
                space().into()
            }
            "spacer" => match number(node, "size") {
                Some(size) => {
                    width = Length::Fixed(size);
                    height = Length::Fixed(size);
                    space().into()
                }
                None => {
                    match parent {
                        Axis::Row => width = Length::Fill,
                        Axis::Column => height = Length::Fill,
                    }
                    space().into()
                }
            },
            "window" => {
                width = Length::Fixed(number(node, "w").unwrap_or(0.0));
                height = Length::Fixed(0.0);
                space().into()
            }
            _ => space().into(),
        };
        container(content)
            .id(node_id(&key))
            .width(width)
            .height(height)
            .into()
    }
}

/// The id of the container wrapping scene node `key` (`id` or `id@row`).
pub fn node_id(key: &str) -> Id {
    Id::from(format!("node:{key}"))
}

impl Program for SceneProgram {
    type Message = Msg;

    fn update(&mut self, message: Msg) {
        match message {
            Msg::Click { node, item } => self.act(&node, "click", None, item),
            Msg::Input { node, value } => {
                self.act(&node, "change", Some(json!(value)), None);
                self.fields.insert(node, value);
            }
            Msg::Submit { node } => {
                let value = self.fields.get(&node).cloned().unwrap_or_default();
                self.act(&node, "submit", Some(json!(value)), None);
            }
            Msg::Toggle { node, value } => {
                self.toggles.insert(node.clone(), value);
                self.act(&node, "change", Some(json!(value)), None);
            }
            Msg::Hover { key, inside } => {
                if inside {
                    self.hovered.insert(key);
                } else {
                    self.hovered.remove(&key);
                }
            }
        }
    }

    fn view(&self) -> Element<'_, Msg> {
        let tokens = self.look.tokens;
        let root = if self.tree.nodes.contains_key("root") && !self.templates.contains("root") {
            self.node_view("root", Axis::Column, None)
        } else {
            space().into()
        };
        container(root)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(tokens.surface)),
                text_color: Some(tokens.text),
                ..container::Style::default()
            })
            .into()
    }
}

fn hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn template_ids(tree: &ResolvedScene) -> BTreeSet<String> {
    fn visit(tree: &ResolvedScene, id: &str, ids: &mut BTreeSet<String>) {
        if !ids.insert(id.into()) {
            return;
        }
        if let Some(node) = tree.nodes.get(id) {
            for child in children(node) {
                visit(tree, child, ids);
            }
        }
    }
    let mut ids = BTreeSet::new();
    for id in &tree.templates {
        visit(tree, id, &mut ids);
    }
    ids
}

fn children(node: &Node) -> impl Iterator<Item = &str> {
    node.ports
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

fn rows(node: &Node) -> &[Value] {
    node.ports
        .get("rows")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn string(node: &Node, port: &str) -> Option<String> {
    node.ports
        .get(port)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn text_port<'a>(node: &'a Node, port: &str) -> &'a str {
    node.ports
        .get(port)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn number(node: &Node, port: &str) -> Option<f32> {
    node.ports
        .get(port)
        .and_then(Value::as_f64)
        .map(|v| v as f32)
}

fn flag(node: &Node, port: &str) -> bool {
    node.ports
        .get(port)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn list_height(node: &Node) -> f32 {
    let max_rows = number(node, "max_rows").unwrap_or(8.0) as usize;
    (rows(node).len().min(max_rows).max(1) as f32)
        * (number(node, "row_height").unwrap_or(24.0) + number(node, "gap").unwrap_or(0.0))
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`, as Bevy's `Srgba::hex` accepts.
pub(crate) fn colour(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    let nibble = |i: usize| {
        u8::from_str_radix(hex.get(i..i + 1)?, 16)
            .ok()
            .map(|v| v * 17)
    };
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    let [r, g, b, a] = match hex.len() {
        3 => [nibble(0)?, nibble(1)?, nibble(2)?, 255],
        4 => [nibble(0)?, nibble(1)?, nibble(2)?, nibble(3)?],
        6 => [byte(0)?, byte(2)?, byte(4)?, 255],
        8 => [byte(0)?, byte(2)?, byte(4)?, byte(6)?],
        _ => return None,
    };
    Some(Color::from_rgba8(r, g, b, f32::from(a) / 255.0))
}

fn substitute_cells(mut source: &str, cells: &Value) -> String {
    let mut out = String::new();
    while let Some(start) = source.find("{cells[") {
        out.push_str(&source[..start]);
        let token = &source[start + 7..];
        let Some(end) = token.find("]}") else {
            out.push_str(&source[start..]);
            return out;
        };
        match token[..end]
            .parse::<usize>()
            .ok()
            .and_then(|index| cells.get(index))
            .and_then(Value::as_str)
        {
            Some(value) => out.push_str(value),
            None => out.push_str(&source[start..start + 7 + end + 2]),
        }
        source = &token[end + 2..];
    }
    out.push_str(source);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours_match_bevy_forms() {
        assert_eq!(colour("#fff"), Some(Color::from_rgb8(255, 255, 255)));
        assert_eq!(colour("#20242d"), Some(Color::from_rgb8(0x20, 0x24, 0x2d)));
        assert_eq!(colour("#00000080").map(|c| c.a), Some(128.0 / 255.0));
        assert_eq!(colour(""), None);
        assert_eq!(colour("#12"), None);
    }

    #[test]
    fn cells_substitute_literally() {
        assert_eq!(
            substitute_cells("{cells[0]} / {cells[1]}", &json!(["{cells[1]}", "literal"])),
            "{cells[1]} / literal"
        );
        assert_eq!(substitute_cells("{cells[9]}", &json!(["a"])), "{cells[9]}");
    }
}
