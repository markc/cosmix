//! Tower toolbar and DCS panel construction/rendering.

use std::collections::BTreeMap;

use bevy::a11y::AccessibilityNode;
use bevy::ecs::observer::On;
use bevy::ecs::system::{Commands, Query};
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeBorderColor, ThemeTextColor};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::prelude::{
    default, AlignItems, BorderColor, BorderRadius, Button, Color, Component, Entity,
    FlexDirection, Node, Res, ResMut, Resource, Text, TextFont, TextLayout, Time, Timer, TimerMode,
    With,
};
use bevy::text::{EditableText, EditableTextFilter, LineBreak, TextCursorStyle};
use bevy::ui::widget::TextScroll;
use bevy::ui::{percent, px, InteractionDisabled, Overflow, OverflowAxis, UiRect};
use bevy::ui_widgets::Activate;
use ctk::prelude::{
    spawn_toolbar_row_with_icons, spawn_tree_disclosure, spawn_tree_view, sync_tree_view, Icon,
    IconSet, StatusText, ToolbarButtonDef, ToolbarItem, TreeItem, TreeViewChanged,
};
use ctk::theme::tokens;

use crate::atlas::RefreshMesh;
use crate::config::SavedFilters;
use crate::confirm::ConfirmIntent;
use crate::inspector::{
    truncate_text, ControlDescriptor, InspectCitizen, InspectorMutation, ProcessIdentity,
    MAX_INSPECTOR_ENTITIES,
};
use crate::lifecycle::{LifecycleCommand, LifecycleVerb};
use crate::model::{AtlasState, Reachability, ServiceInfo};
use crate::traffic::{TrafficBody, TrafficIntent, TrafficState, MAX_RENDERED_ROWS};

#[derive(Clone, Copy)]
pub(crate) struct ToolbarEntities {
    pub root: Entity,
    pub refresh: Entity,
}

#[derive(Clone, Debug)]
enum ObservedAgeFormat {
    Single {
        suffix: String,
    },
    Pair {
        between: String,
        second_observed_at_ms: Option<u64>,
        suffix: String,
    },
}

#[derive(Component, Clone, Debug)]
pub(crate) struct ObservedAgeText {
    observed_at_ms: Option<u64>,
    prefix: String,
    format: ObservedAgeFormat,
    max_chars: usize,
    single_line: bool,
}

impl ObservedAgeText {
    fn single(
        prefix: impl Into<String>,
        observed_at_ms: Option<u64>,
        suffix: impl Into<String>,
    ) -> Self {
        Self {
            observed_at_ms,
            prefix: prefix.into(),
            format: ObservedAgeFormat::Single {
                suffix: suffix.into(),
            },
            max_chars: MAX_UI_TEXT_CHARS,
            single_line: false,
        }
    }

    fn pair(
        prefix: impl Into<String>,
        first_observed_at_ms: Option<u64>,
        between: impl Into<String>,
        second_observed_at_ms: Option<u64>,
        suffix: impl Into<String>,
    ) -> Self {
        Self {
            observed_at_ms: first_observed_at_ms,
            prefix: prefix.into(),
            format: ObservedAgeFormat::Pair {
                between: between.into(),
                second_observed_at_ms,
                suffix: suffix.into(),
            },
            max_chars: MAX_UI_TEXT_CHARS,
            single_line: false,
        }
    }

    pub(crate) fn status(
        prefix: impl Into<String>,
        observed_at_ms: Option<u64>,
        suffix: impl Into<String>,
    ) -> Self {
        let mut marker = Self::single(prefix, observed_at_ms, suffix);
        marker.max_chars = 512;
        marker.single_line = true;
        marker
    }

    fn render_at(&self, now_ms: u64) -> String {
        let mut rendered = format!(
            "{}{}",
            self.prefix,
            AtlasState::observed_label_at(self.observed_at_ms, now_ms)
        );
        match &self.format {
            ObservedAgeFormat::Single { suffix } => rendered.push_str(suffix),
            ObservedAgeFormat::Pair {
                between,
                second_observed_at_ms,
                suffix,
            } => {
                rendered.push_str(between);
                rendered.push_str(&AtlasState::observed_label_at(
                    *second_observed_at_ms,
                    now_ms,
                ));
                rendered.push_str(suffix);
            }
        }
        let rendered = ascii_ui_text_bounded(&rendered, self.max_chars);
        if self.single_line {
            rendered.replace(['\n', '\t'], " ")
        } else {
            rendered
        }
    }
}

#[derive(Resource)]
pub(crate) struct ObservedAgeRefresh(Timer);

impl Default for ObservedAgeRefresh {
    fn default() -> Self {
        Self(Timer::from_seconds(1.0, TimerMode::Repeating))
    }
}

pub(crate) fn refresh_observed_age_texts(
    time: Res<Time>,
    mut refresh: ResMut<ObservedAgeRefresh>,
    mut ages: Query<(&ObservedAgeText, &mut Text, Option<&mut StatusText>)>,
) {
    if !refresh.0.tick(time.delta()).just_finished() {
        return;
    }
    let now_ms = crate::model::now_unix_ms();
    for (marker, mut text, status) in &mut ages {
        let rendered = marker.render_at(now_ms);
        if text.0 == rendered {
            continue;
        }
        text.0.clone_from(&rendered);
        if let Some(mut status) = status {
            status.set(rendered);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PaneEntities {
    pub nodes: Entity,
    pub filters: Entity,
    pub overview: Entity,
    pub citizens: Entity,
    pub inspector: Entity,
    pub properties: Entity,
    pub traffic: Entity,
}

#[derive(Component)]
struct RefreshButton;

#[derive(Component)]
struct NodeChoice(String);

#[derive(Component)]
struct CitizenChoice(String);

#[derive(Component)]
struct InspectorMutationButton(InspectorMutation);

#[derive(Component)]
struct LifecycleButton(LifecycleCommand);

#[derive(Component)]
struct TrafficButton(TrafficIntent);

#[derive(Component)]
struct SaveFilterButton {
    input: Entity,
}

#[derive(Component)]
pub(crate) struct FilterNameInput;

#[derive(Component)]
pub(crate) struct PropertyBranch(String);

#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) enum TowerFocusKey {
    Node(String),
    Topology(String),
    Citizen(String),
    Inspector(String),
    Lifecycle(String),
    Property(String),
    Traffic(String),
    Filter(String),
}

#[derive(Resource, Default)]
pub(crate) struct PropertyDisclosureState(BTreeMap<String, bool>);

pub(crate) fn spawn_toolbar(
    commands: &mut Commands,
    icons: &IconSet,
    theme: &bevy::feathers::theme::UiTheme,
) -> ToolbarEntities {
    let row = spawn_toolbar_row_with_icons(
        commands,
        [ToolbarItem::Button(
            ToolbarButtonDef::new("Refresh mesh").with_icon(Icon::Refresh),
        )],
        [],
        icons,
        theme,
    );
    let refresh = row.left[0];
    commands
        .entity(refresh)
        .insert(RefreshButton)
        .observe(on_refresh);
    ToolbarEntities {
        root: row.root,
        refresh,
    }
}

pub(crate) fn spawn_panels(commands: &mut Commands) -> PaneEntities {
    PaneEntities {
        nodes: panel(commands),
        filters: panel(commands),
        overview: panel(commands),
        citizens: panel(commands),
        inspector: panel(commands),
        properties: panel(commands),
        traffic: panel(commands),
    }
}

pub(crate) fn render_status(
    commands: &mut Commands,
    statuses: &mut Query<&mut StatusText>,
    ages: &mut Query<&mut ObservedAgeText>,
    status_text: Entity,
    toolbar: ToolbarEntities,
    atlas: &AtlasState,
) {
    let epoch = atlas
        .inventory
        .as_ref()
        .and_then(|inventory| inventory.epoch)
        .map_or_else(|| "-".into(), |epoch| epoch.to_string());
    let refresh = if atlas.refreshing {
        format!(
            " - refreshing ({})",
            atlas
                .last_refresh_reason
                .map_or("unknown", crate::model::RefreshReason::label)
        )
    } else {
        String::new()
    };
    let reason = atlas
        .mesh_reason
        .as_deref()
        .map_or_else(String::new, |reason| format!(" - {reason}"));
    let notice = atlas
        .notice
        .as_deref()
        .map_or_else(String::new, |notice| format!(" - {notice}"));
    let marker = ObservedAgeText::status(
        format!(
            "Bus {:?} - Mesh {} - epoch {epoch} - ",
            atlas.connection, atlas.mesh_posture
        ),
        atlas.inventory_observed_at_ms,
        format!("{reason}{refresh}{notice}"),
    );
    let status_line = marker.render_at(crate::model::now_unix_ms());
    if let Ok(mut age) = ages.get_mut(status_text) {
        *age = marker;
    }
    if let Ok(mut status) = statuses.get_mut(status_text) {
        status.set(status_line);
    }
    if atlas.refreshing {
        commands.entity(toolbar.refresh).insert(InteractionDisabled);
    } else {
        commands
            .entity(toolbar.refresh)
            .remove::<InteractionDisabled>();
    }
}

pub(crate) fn render_nodes(
    commands: &mut Commands,
    panel: Entity,
    atlas: &AtlasState,
) -> Vec<(TowerFocusKey, Entity)> {
    commands.entity(panel).despawn_children();
    let mut focus = Vec::new();
    for node in atlas.nodes.values() {
        let selected = atlas.selected.as_deref() == Some(node.member.name.as_str());
        let label = observed_age_pair_text(
            commands,
            format!(
                "{}\n{} - info {} (",
                node.member.name,
                node.status_label(),
                reachability_word(node.info_reachability)
            ),
            node.info_observed_at_ms,
            format!(
                ") - citizens {} (",
                reachability_word(node.citizens_reachability)
            ),
            node.citizens_observed_at_ms,
            ")",
            12.0,
            false,
        );
        if selected {
            // The selection bar is a solid accent knocked out of the panel, so
            // the resting foreground is illegible on it. Rebuilt on every
            // selection change, so setting this at spawn is enough.
            commands
                .entity(label)
                .insert(ThemeTextColor(tokens::ROW_SELECTED_TEXT));
        }
        let row = node_button(commands, &node.member.name, label, selected);
        focus.push((TowerFocusKey::Node(node.member.name.clone()), row));
        commands.entity(panel).add_child(row);
    }
    focus
}

pub(crate) fn render_filters(
    commands: &mut Commands,
    panel: Entity,
    atlas: &AtlasState,
    saved: &SavedFilters,
) -> Vec<(TowerFocusKey, Entity)> {
    commands.entity(panel).despawn_children();
    let mut focus = Vec::new();
    let summary = text(
        commands,
        &format!(
            "Declared members: {}\nActive Bus members: {}\n\nEdges remain limited to local routes reported by noded.peers. Saved filters may refer to absent services and then simply match nothing.",
            atlas.nodes.len(),
            atlas
                .nodes
                .values()
                .filter(|node| node.member.active_bus())
                .count()
        ),
        11.0,
        true,
    );
    commands.entity(panel).add_child(summary);

    let mut editable_name = EditableText::new(ascii_ui_text_bounded(&saved.draft_name, 64));
    editable_name.max_characters = Some(64);
    let name_input = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(30),
                padding: UiRect::axes(px(7), px(4)),
                border: UiRect::all(px(1)),
                margin: UiRect::bottom(px(5)),
                ..default()
            },
            editable_name,
            EditableTextFilter::new(|character| {
                character.is_ascii() && !character.is_ascii_control()
            }),
            TextLayout::no_wrap(),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT),
            TextCursorStyle::default(),
            TextScroll::default(),
            ThemeBackgroundColor(tokens::TRACK),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::BORDER),
            TabIndex(0),
            FilterNameInput,
        ))
        .id();
    let name_key = TowerFocusKey::Filter("name".into());
    commands.entity(name_input).insert(name_key.clone());
    focus.push((name_key, name_input));
    commands.entity(panel).add_child(name_input);

    let save_label = text(commands, "Save current traffic filter", 10.0, false);
    let save = commands
        .spawn((
            Button,
            SaveFilterButton { input: name_input },
            TabIndex(0),
            compact_button_node(),
            ThemeBackgroundColor(tokens::CONTROL),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::BORDER),
        ))
        .add_child(save_label)
        .observe(on_save_filter)
        .id();
    let save_key = TowerFocusKey::Filter("save".into());
    commands.entity(save).insert(save_key.clone());
    focus.push((save_key, save));
    commands.entity(panel).add_child(save);

    let heading = text(
        commands,
        &format!("Saved filters - {} of 32", saved.filters.len()),
        12.0,
        false,
    );
    commands.entity(panel).add_child(heading);
    if saved.filters.is_empty() {
        let empty = text(commands, "No saved traffic filters", 11.0, true);
        commands.entity(panel).add_child(empty);
    }
    for (name, filter) in &saved.filters {
        let active = saved.active.as_deref() == Some(name.as_str());
        let use_label = format!(
            "{}{} - {} - {} - {}",
            if active { "* " } else { "" },
            name,
            filter.verb_glob,
            filter.service.as_deref().unwrap_or("any service"),
            filter.direction.label(),
        );
        let select = traffic_button(
            commands,
            &use_label,
            TrafficIntent::SelectNamed(name.clone()),
        );
        let select_key = TowerFocusKey::Filter(format!("use:{name}"));
        commands.entity(select).insert(select_key.clone());
        focus.push((select_key, select));
        commands.entity(panel).add_child(select);

        let delete = traffic_button(
            commands,
            &format!("Delete \"{name}\""),
            TrafficIntent::DeleteNamed(name.clone()),
        );
        let delete_key = TowerFocusKey::Filter(format!("delete:{name}"));
        commands.entity(delete).insert(delete_key.clone());
        focus.push((delete_key, delete));
        commands.entity(panel).add_child(delete);
    }
    focus
}

pub(crate) fn render_overview(
    commands: &mut Commands,
    panel: Entity,
    atlas: &AtlasState,
) -> Vec<(TowerFocusKey, Entity)> {
    commands.entity(panel).despawn_children();
    let Some(selected) = atlas
        .selected
        .as_ref()
        .and_then(|name| atlas.nodes.get(name))
    else {
        replace_panel_text(commands, panel, "Select a node".into());
        return Vec::new();
    };
    let info = selected.info.as_ref();
    let summary = observed_age_pair_text(
        commands,
        format!(
            "Name: {}\nMesh address: {}\nInventory status: {}\nAMP: {}\nInfo: {} - ",
            selected.member.name,
            selected.member.mesh_ip,
            selected.member.status,
            selected.member.bus,
            reachability_word(selected.info_reachability),
        ),
        selected.info_observed_at_ms,
        format!(
            "\nCitizens: {} - ",
            reachability_word(selected.citizens_reachability)
        ),
        selected.citizens_observed_at_ms,
        format!(
            "\nBroker version: {}\nUptime: {}",
            info.and_then(|info| info.noded.as_ref())
                .and_then(|service| service.version.as_deref())
                .unwrap_or("unknown"),
            info.and_then(|info| info.uptime_s)
                .map_or_else(|| "unknown".into(), |seconds| format!("{seconds}s"))
        ),
        12.0,
        true,
    );
    commands.entity(panel).add_child(summary);
    let heading = text(commands, "Daemon lifecycle", 13.0, false);
    commands.entity(panel).add_child(heading);
    let mut focus = Vec::new();
    let daemons = atlas.daemons.get(&selected.member.name);
    if daemons.is_none_or(|state| state.loading && state.units.is_empty()) {
        let loading = text(commands, "Loading cosmix-*.service units...", 11.0, true);
        commands.entity(panel).add_child(loading);
    }
    if let Some(state) = daemons {
        if let Some(error) = &state.error {
            let error = observed_age_text(
                commands,
                format!("Units unknown: {error}\n"),
                state.observed_at_ms,
                "",
                11.0,
                true,
            );
            commands.entity(panel).add_child(error);
        }
        for unit in &state.units {
            let label = text(
                commands,
                &format!("{} - {}", unit.unit, unit.status.label()),
                11.0,
                false,
            );
            commands.entity(panel).add_child(label);
            let row = commands
                .spawn(Node {
                    width: percent(100),
                    flex_direction: FlexDirection::Row,
                    column_gap: px(5),
                    margin: UiRect::bottom(px(7)),
                    ..default()
                })
                .id();
            for verb in [
                LifecycleVerb::Start,
                LifecycleVerb::Stop,
                LifecycleVerb::Restart,
            ] {
                let command = LifecycleCommand {
                    node: selected.member.name.clone(),
                    unit: unit.unit.clone(),
                    verb,
                };
                let button = lifecycle_button(commands, command.clone());
                focus.push((
                    TowerFocusKey::Lifecycle(format!(
                        "{}:{}:{}",
                        command.node,
                        command.unit,
                        command.verb.as_str()
                    )),
                    button,
                ));
                commands.entity(row).add_child(button);
            }
            commands.entity(panel).add_child(row);
        }
        if let Some(result) = &state.result {
            let result = observed_age_text(
                commands,
                format!("{result}\n"),
                state.result_observed_at_ms,
                "",
                11.0,
                true,
            );
            commands.entity(panel).add_child(result);
        }
    }
    focus
}

pub(crate) fn render_citizens(
    commands: &mut Commands,
    panel: Entity,
    atlas: &AtlasState,
) -> Vec<(TowerFocusKey, Entity)> {
    let Some(selected) = atlas
        .selected
        .as_ref()
        .and_then(|name| atlas.nodes.get(name))
    else {
        replace_panel_text(commands, panel, "No node selected".into());
        return Vec::new();
    };
    render_citizens_tree(
        commands,
        panel,
        &selected.citizens,
        atlas.selected_citizen.as_deref(),
    )
}

struct InspectorEntityBudget {
    remaining: usize,
}

impl InspectorEntityBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_INSPECTOR_ENTITIES,
        }
    }

    fn consume(&mut self, count: usize) {
        assert!(
            self.take(count),
            "fixed inspector chrome exceeds entity budget"
        );
    }

    fn take(&mut self, count: usize) -> bool {
        if self.remaining < count {
            return false;
        }
        self.remaining -= count;
        true
    }

    fn take_reserving(&mut self, count: usize, reserve: usize) -> bool {
        if self.remaining < count.saturating_add(reserve) {
            return false;
        }
        self.remaining -= count;
        true
    }
}

pub(crate) fn render_inspector(
    commands: &mut Commands,
    panel: Entity,
    atlas: &AtlasState,
) -> Vec<(TowerFocusKey, Entity)> {
    commands.entity(panel).despawn_children();
    let Some(inspector) = &atlas.inspector else {
        replace_panel_text(commands, panel, "Select a local citizen to inspect".into());
        return Vec::new();
    };
    let mut focus = Vec::new();
    let mut budget = InspectorEntityBudget::new();
    budget.consume(1);
    let heading = text(commands, &inspector.service, 14.0, false);
    commands.entity(panel).add_child(heading);
    if let Some(description) = &inspector.description {
        budget.consume(1);
        let description = observed_age_text(
            commands,
            format!(
                "{}\nview {} - engine {} - pid {}\n",
                description.title,
                description.view,
                description.engine,
                description
                    .pid
                    .map_or_else(|| "unknown".into(), |pid| pid.to_string())
            ),
            inspector.description_observed_at_ms,
            "",
            12.0,
            true,
        );
        commands.entity(panel).add_child(description);
    } else {
        budget.consume(1);
        let status = inspector
            .description_error
            .as_deref()
            .unwrap_or("app.describe pending");
        let status = observed_age_text(
            commands,
            format!("{status}\n"),
            inspector.description_observed_at_ms,
            "",
            11.0,
            true,
        );
        commands.entity(panel).add_child(status);
    }

    budget.consume(1);
    let actions_heading = text(commands, "Actions", 13.0, false);
    commands.entity(panel).add_child(actions_heading);
    if let Some(error) = &inspector.actions_error {
        budget.consume(1);
        let error = observed_age_text(
            commands,
            format!("Actions unknown: {error}\n"),
            inspector.actions_observed_at_ms,
            "",
            11.0,
            true,
        );
        commands.entity(panel).add_child(error);
    } else if inspector.actions_observed_at_ms.is_some() {
        budget.consume(1);
        let observed = observed_age_text(
            commands,
            "",
            inspector.actions_observed_at_ms,
            "",
            11.0,
            true,
        );
        commands.entity(panel).add_child(observed);
    }
    let mut groups: BTreeMap<&str, Vec<_>> = BTreeMap::new();
    for action in inspector.actions.values() {
        groups
            .entry(action.category.as_deref().unwrap_or("other"))
            .or_default()
            .push(action);
    }
    let total_actions = inspector.actions.len() + inspector.actions_omitted;
    let mut rendered_actions = 0usize;
    'action_groups: for (category, actions) in groups {
        let mut category_spawned = false;
        for action in actions {
            let has_button = action.can_invoke_without_args();
            // A mutation affordance owns three ECS entities: button, label,
            // and the entity-scoped activation observer.
            let cost = usize::from(!category_spawned) + 1 + usize::from(has_button) * 3;
            if !budget.take_reserving(cost, 1) {
                break 'action_groups;
            }
            if !category_spawned {
                let category = text(commands, category, 12.0, false);
                commands.entity(panel).add_child(category);
                category_spawned = true;
            }
            let shortcut = action
                .shortcut
                .as_ref()
                .map(|value| format!(" - shortcut {}", compact_json(value)))
                .unwrap_or_default();
            let icon = action
                .icon_name
                .as_deref()
                .map(|icon| format!(" - icon {icon}"))
                .unwrap_or_default();
            let label = text(
                commands,
                &format!(
                    "{} ({}){icon}{shortcut}\n{}",
                    action.label,
                    action.id,
                    action.description.as_deref().unwrap_or("No description")
                ),
                11.0,
                true,
            );
            commands.entity(panel).add_child(label);
            rendered_actions += 1;
            if has_button {
                let mutation = InspectorMutation::InvokeAction {
                    service: inspector.service.clone(),
                    action: action.id.clone(),
                    identity: inspector.identity.clone(),
                };
                let busy = atlas.active_mutations.contains(&mutation.target());
                let button = inspector_mutation_button(
                    commands,
                    if busy { "In progress..." } else { "Invoke..." },
                    mutation,
                    busy,
                );
                if !busy {
                    focus.push((
                        TowerFocusKey::Inspector(format!("action:{}", action.id)),
                        button,
                    ));
                }
                commands.entity(panel).add_child(button);
            }
        }
    }
    let hidden_actions = total_actions.saturating_sub(rendered_actions);
    if hidden_actions > 0 && budget.take(1) {
        let omitted = text(
            commands,
            &format!("+{hidden_actions} more actions"),
            11.0,
            true,
        );
        commands.entity(panel).add_child(omitted);
    }

    budget.consume(1);
    let controls_heading = text(commands, "Controls", 13.0, false);
    commands.entity(panel).add_child(controls_heading);
    if let Some(error) = &inspector.controls_error {
        budget.consume(1);
        let error = observed_age_text(
            commands,
            format!("Controls unknown: {error}\n"),
            inspector.controls_observed_at_ms,
            "",
            11.0,
            true,
        );
        commands.entity(panel).add_child(error);
    } else if inspector.controls_observed_at_ms.is_some() {
        budget.consume(1);
        let observed = observed_age_text(
            commands,
            "",
            inspector.controls_observed_at_ms,
            "",
            11.0,
            true,
        );
        commands.entity(panel).add_child(observed);
    }
    let total_controls = inspector.controls.len() + inspector.controls_omitted;
    let mut rendered_controls = 0usize;
    for control in inspector.controls.values() {
        let mutations = control_mutations(&inspector.identity, control);
        let cost = 1 + mutations.len() * 3;
        if !budget.take_reserving(cost, 2) {
            break;
        }
        let value = control
            .value
            .as_ref()
            .map(compact_json)
            .unwrap_or_else(|| "unknown".into());
        let unit = control
            .unit
            .as_deref()
            .map(|unit| format!(" {unit}"))
            .unwrap_or_default();
        let error = control
            .value_error
            .as_deref()
            .map(|error| format!(" - {error}"))
            .unwrap_or_default();
        let label = observed_age_text(
            commands,
            format!("{} - {} = {value}{unit}\n", control.id, control.kind),
            control.value_observed_at_ms,
            error,
            11.0,
            true,
        );
        commands.entity(panel).add_child(label);
        rendered_controls += 1;
        for (label, mutation) in mutations {
            let busy = atlas.active_mutations.contains(&mutation.target());
            let button = inspector_mutation_button(
                commands,
                if busy { "In progress..." } else { label },
                mutation,
                busy,
            );
            if !busy {
                focus.push((
                    TowerFocusKey::Inspector(format!("control:{}:{label}", control.id)),
                    button,
                ));
            }
            commands.entity(panel).add_child(button);
        }
    }
    let hidden_controls = total_controls.saturating_sub(rendered_controls);
    if hidden_controls > 0 && budget.take(1) {
        let omitted = text(
            commands,
            &format!("+{hidden_controls} more controls"),
            11.0,
            true,
        );
        commands.entity(panel).add_child(omitted);
    }
    if let Some(result) = &inspector.result {
        if !budget.take(1) {
            return focus;
        }
        let result = observed_age_text(
            commands,
            format!(
                "{}: {}\n{}\n",
                if result.ok { "OK" } else { "Error" },
                result.summary,
                result
                    .body
                    .as_ref()
                    .map(compact_json)
                    .unwrap_or_else(|| "no response body".into())
            ),
            Some(result.observed_at_ms),
            "",
            11.0,
            true,
        );
        commands.entity(panel).add_child(result);
    }
    focus
}

pub(crate) fn render_properties(
    commands: &mut Commands,
    panel: Entity,
    atlas: &AtlasState,
    disclosure: &PropertyDisclosureState,
) -> Vec<(TowerFocusKey, Entity)> {
    let Some(selected) = atlas
        .selected
        .as_ref()
        .and_then(|name| atlas.nodes.get(name))
    else {
        replace_panel_text(commands, panel, "No node selected".into());
        return Vec::new();
    };
    if atlas.local_node() == Some(selected.member.name.as_str()) {
        render_properties_tree(commands, panel, atlas, disclosure)
    } else {
        replace_panel_observed_age_text(
            commands,
            panel,
            "Remote properties are not polled in P1.\n",
            selected.citizens_observed_at_ms,
            "",
        );
        Vec::new()
    }
}

pub(crate) fn render_traffic(
    commands: &mut Commands,
    panel: Entity,
    traffic: &TrafficState,
) -> Vec<(TowerFocusKey, Entity)> {
    commands.entity(panel).despawn_children();
    let mut focus = Vec::new();
    let status = text(
        commands,
        &format!(
            "{}\nServer dropped: {} - client dropped: {} - paused buffer: {}",
            traffic.status,
            traffic.server_dropped,
            traffic.client_dropped,
            traffic.paused_len()
        ),
        11.0,
        true,
    );
    commands.entity(panel).add_child(status);

    let controls = [
        (
            if traffic.paused { "Resume" } else { "Pause" }.to_string(),
            TrafficIntent::TogglePause,
            "pause",
        ),
        (
            format!("Verb: {}", traffic.filter.verb_glob),
            TrafficIntent::CycleVerb,
            "verb",
        ),
        (
            format!(
                "Service: {}",
                traffic.filter.service.as_deref().unwrap_or("any")
            ),
            TrafficIntent::CycleService,
            "service",
        ),
        (
            format!("Direction: {}", traffic.filter.direction.label()),
            TrafficIntent::CycleDirection,
            "direction",
        ),
        (
            format!("Bodies: {}", traffic.filter.body.label()),
            TrafficIntent::ToggleBody,
            "body",
        ),
    ];
    for (label, intent, key) in controls {
        let button = traffic_button(commands, &label, intent);
        focus.push((TowerFocusKey::Traffic(key.into()), button));
        commands.entity(panel).add_child(button);
    }

    let heading = text(
        commands,
        &format!(
            "Live events - showing newest {} of {}",
            traffic.rows.len().min(MAX_RENDERED_ROWS),
            traffic.rows.len()
        ),
        12.0,
        false,
    );
    commands.entity(panel).add_child(heading);
    for event in traffic.rows.iter().rev().take(MAX_RENDERED_ROWS) {
        let label = format!(
            "#{:<6} {} {:<14} {} -> {} - {} - {}B{}",
            event.seq,
            event.direction,
            event.verb.as_deref().unwrap_or("-"),
            event.from.as_deref().unwrap_or("-"),
            event.to.as_deref().unwrap_or("-"),
            event.outcome,
            event.size,
            if event.dropped_count > 0 {
                format!(" - dropped +{}", event.dropped_count)
            } else {
                String::new()
            }
        );
        let button = traffic_button(commands, &label, TrafficIntent::Select(event.seq));
        focus.push((
            TowerFocusKey::Traffic(format!("event:{}", event.seq)),
            button,
        ));
        commands.entity(panel).add_child(button);
    }

    if let Some(event) = traffic.selected() {
        let mut payload = if traffic.filter.body == TrafficBody::Redacted {
            event.payload.as_ref().map(compact_json).unwrap_or_else(|| {
                format!(
                    "omitted: {}",
                    event.payload_omitted.as_deref().unwrap_or("not supplied")
                )
            })
        } else {
            "not requested (Bodies: off)".into()
        };
        truncate_text(&mut payload, 4096);
        let mut detail_text = format!(
            "\nSelected #{}\nts: {}\ntype: {}\ndirection: {}\noutcome: {}\nfrom: {}\nto: {}\nverb: {}\nsize: {}\ncorrelation: {}\nrc: {}\ndropped before event: {}\npayload: {}",
            event.seq,
            event.ts,
            event.message_type,
            event.direction,
            event.outcome,
            event.from.as_deref().unwrap_or("-"),
            event.to.as_deref().unwrap_or("-"),
            event.verb.as_deref().unwrap_or("-"),
            event.size,
            event.correlation_id.as_deref().unwrap_or("-"),
            event.rc.map_or_else(|| "-".into(), |rc| rc.to_string()),
            event.dropped_count,
            payload,
        );
        truncate_text(&mut detail_text, 8192);
        let detail = text(commands, &detail_text, 11.0, true);
        commands.entity(panel).add_child(detail);
    }
    focus
}

fn on_refresh(
    _activate: On<Activate>,
    mut refresh: bevy::ecs::message::MessageWriter<RefreshMesh>,
) {
    refresh.write(RefreshMesh);
}

fn on_node_choice(
    activate: On<Activate>,
    choices: Query<&NodeChoice>,
    mut atlas: bevy::prelude::ResMut<AtlasState>,
) {
    if let Ok(choice) = choices.get(activate.entity) {
        atlas.select(choice.0.clone());
    }
}

fn on_citizen_choice(
    activate: On<Activate>,
    choices: Query<&CitizenChoice>,
    mut requests: bevy::ecs::message::MessageWriter<InspectCitizen>,
) {
    if let Ok(choice) = choices.get(activate.entity) {
        requests.write(InspectCitizen {
            service: choice.0.clone(),
        });
    }
}

fn on_inspector_mutation(
    activate: On<Activate>,
    buttons: Query<&InspectorMutationButton>,
    mut confirms: bevy::ecs::message::MessageWriter<ConfirmIntent>,
) {
    if let Ok(button) = buttons.get(activate.entity) {
        confirms.write(ConfirmIntent::Inspector(button.0.clone()));
    }
}

fn on_lifecycle(
    activate: On<Activate>,
    buttons: Query<&LifecycleButton>,
    mut confirms: bevy::ecs::message::MessageWriter<ConfirmIntent>,
    mut lifecycle: bevy::ecs::message::MessageWriter<LifecycleCommand>,
) {
    if let Ok(button) = buttons.get(activate.entity) {
        if button.0.verb == LifecycleVerb::Start {
            lifecycle.write(button.0.clone());
        } else {
            confirms.write(ConfirmIntent::Lifecycle(button.0.clone()));
        }
    }
}

fn on_traffic(
    activate: On<Activate>,
    buttons: Query<&TrafficButton>,
    mut intents: bevy::ecs::message::MessageWriter<TrafficIntent>,
) {
    if let Ok(button) = buttons.get(activate.entity) {
        intents.write(button.0.clone());
    }
}

fn on_save_filter(
    activate: On<Activate>,
    buttons: Query<&SaveFilterButton>,
    inputs: Query<&EditableText, With<FilterNameInput>>,
    mut intents: bevy::ecs::message::MessageWriter<TrafficIntent>,
) {
    let Ok(button) = buttons.get(activate.entity) else {
        return;
    };
    let Ok(input) = inputs.get(button.input) else {
        return;
    };
    intents.write(TrafficIntent::SaveNamed(input.value().to_string()));
}

pub(crate) fn on_property_disclosure_changed(
    changed: On<TreeViewChanged>,
    branches: Query<&PropertyBranch>,
    mut state: bevy::prelude::ResMut<PropertyDisclosureState>,
) {
    if let Ok(branch) = branches.get(changed.item) {
        state.0.insert(branch.0.clone(), changed.expanded);
    }
}

fn inspector_mutation_button(
    commands: &mut Commands,
    label: &str,
    mutation: InspectorMutation,
    busy: bool,
) -> Entity {
    let label_entity = text(commands, label, 11.0, false);
    let mut entity = commands.spawn((
        Button,
        InspectorMutationButton(mutation),
        TabIndex(0),
        compact_button_node(),
        ThemeBackgroundColor(tokens::CONTROL),
        BorderColor::all(Color::NONE),
        ThemeBorderColor(tokens::BORDER),
    ));
    if busy {
        entity.insert(InteractionDisabled);
    }
    entity
        .add_child(label_entity)
        .observe(on_inspector_mutation)
        .id()
}

fn lifecycle_button(commands: &mut Commands, command: LifecycleCommand) -> Entity {
    let label = command.verb.as_str();
    let label_entity = text(commands, label, 11.0, false);
    commands
        .spawn((
            Button,
            LifecycleButton(command),
            TabIndex(0),
            compact_button_node(),
            ThemeBackgroundColor(tokens::CONTROL),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::BORDER),
        ))
        .add_child(label_entity)
        .observe(on_lifecycle)
        .id()
}

fn traffic_button(commands: &mut Commands, label: &str, intent: TrafficIntent) -> Entity {
    let label_entity = text(commands, label, 10.0, false);
    commands
        .spawn((
            Button,
            TrafficButton(intent),
            TabIndex(0),
            compact_button_node(),
            ThemeBackgroundColor(tokens::CONTROL),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::BORDER),
        ))
        .add_child(label_entity)
        .observe(on_traffic)
        .id()
}

fn compact_button_node() -> Node {
    Node {
        max_width: percent(100),
        min_width: px(0),
        min_height: px(26),
        padding: UiRect::axes(px(8), px(4)),
        border: UiRect::all(px(1)),
        border_radius: BorderRadius::all(px(4)),
        margin: UiRect::bottom(px(5)),
        ..default()
    }
}

fn node_button(commands: &mut Commands, name: &str, label: Entity, selected: bool) -> Entity {
    let mut accessibility = accesskit::Node::new(accesskit::Role::Button);
    accessibility.set_label(format!("Select node {name}"));
    commands
        .spawn((
            Button,
            NodeChoice(name.into()),
            TowerFocusKey::Node(name.into()),
            TabIndex(0),
            AccessibilityNode::from(accessibility),
            Node {
                width: percent(100),
                min_width: px(0),
                min_height: px(48),
                padding: UiRect::all(px(8)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(5)),
                margin: UiRect::bottom(px(5)),
                ..default()
            },
            ThemeBackgroundColor(if selected {
                tokens::ROW_SELECTED
            } else {
                tokens::CONTROL
            }),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(if selected {
                tokens::CONTROL_ACTIVE
            } else {
                tokens::BORDER
            }),
        ))
        .add_child(label)
        .observe(on_node_choice)
        .id()
}

fn panel(commands: &mut Commands) -> Entity {
    commands
        .spawn((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                min_width: px(0),
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                overflow: Overflow {
                    x: OverflowAxis::Clip,
                    y: OverflowAxis::Scroll,
                },
                padding: UiRect::all(px(10)),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
        ))
        .id()
}

fn replace_panel_text(commands: &mut Commands, panel: Entity, content: String) {
    commands.entity(panel).despawn_children();
    let text = text(commands, &content, 12.0, true);
    commands.entity(panel).add_child(text);
}

fn replace_panel_observed_age_text(
    commands: &mut Commands,
    panel: Entity,
    prefix: impl Into<String>,
    observed_at_ms: Option<u64>,
    suffix: impl Into<String>,
) {
    commands.entity(panel).despawn_children();
    let text = observed_age_text(commands, prefix, observed_at_ms, suffix, 12.0, true);
    commands.entity(panel).add_child(text);
}

fn render_citizens_tree(
    commands: &mut Commands,
    panel: Entity,
    citizens: &[ServiceInfo],
    selected: Option<&str>,
) -> Vec<(TowerFocusKey, Entity)> {
    commands.entity(panel).despawn_children();
    if citizens.is_empty() {
        replace_panel_text(commands, panel, "No citizens observed".into());
        return Vec::new();
    }
    let mut focus = Vec::new();
    for service in citizens {
        let label = format!(
            "{} - {} - pid {}",
            service.name,
            service.version.as_deref().unwrap_or("version unknown"),
            service
                .pid
                .map_or_else(|| "unknown".into(), |pid| pid.to_string())
        );
        let label_entity = text(commands, &label, 12.0, false);
        if selected == Some(service.name.as_str()) {
            // Same reason as the node rows: the resting foreground does not
            // survive the selection bar.
            commands
                .entity(label_entity)
                .insert(ThemeTextColor(tokens::ROW_SELECTED_TEXT));
        }
        let mut accessibility = accesskit::Node::new(accesskit::Role::Button);
        accessibility.set_label(format!("Inspect citizen {}", service.name));
        let row = commands
            .spawn((
                Button,
                CitizenChoice(service.name.clone()),
                TowerFocusKey::Citizen(service.name.clone()),
                TabIndex(0),
                AccessibilityNode::from(accessibility),
                Node {
                    width: percent(100),
                    min_height: px(34),
                    padding: UiRect::all(px(6)),
                    border: UiRect::all(px(1)),
                    border_radius: BorderRadius::all(px(4)),
                    margin: UiRect::bottom(px(4)),
                    ..default()
                },
                ThemeBackgroundColor(if selected == Some(service.name.as_str()) {
                    tokens::ROW_SELECTED
                } else {
                    tokens::CONTROL
                }),
                BorderColor::all(Color::NONE),
                ThemeBorderColor(if selected == Some(service.name.as_str()) {
                    tokens::CONTROL_ACTIVE
                } else {
                    tokens::BORDER
                }),
            ))
            .add_child(label_entity)
            .observe(on_citizen_choice)
            .id();
        focus.push((TowerFocusKey::Citizen(service.name.clone()), row));
        commands.entity(panel).add_child(row);
    }
    focus
}

fn control_mutations(
    identity: &ProcessIdentity,
    control: &ControlDescriptor,
) -> Vec<(&'static str, InspectorMutation)> {
    if !control.writable {
        return Vec::new();
    }
    match control.value_type.as_str() {
        "bool" => control
            .value
            .as_ref()
            .and_then(serde_json::Value::as_bool)
            .map(|current| {
                vec![(
                    if current { "Set off..." } else { "Set on..." },
                    InspectorMutation::SetControl {
                        service: identity.service.clone(),
                        control: control.id.clone(),
                        value: serde_json::Value::Bool(!current),
                        identity: identity.clone(),
                    },
                )]
            })
            .unwrap_or_default(),
        "number" => {
            let Some(current) = control.value.as_ref().and_then(serde_json::Value::as_f64) else {
                return Vec::new();
            };
            let step = control.step.unwrap_or(1.0).abs().max(f64::EPSILON);
            let lower = (current - step).max(control.min.unwrap_or(f64::NEG_INFINITY));
            let upper = (current + step).min(control.max.unwrap_or(f64::INFINITY));
            vec![
                (
                    "Decrease...",
                    InspectorMutation::SetControl {
                        service: identity.service.clone(),
                        control: control.id.clone(),
                        value: serde_json::json!(lower),
                        identity: identity.clone(),
                    },
                ),
                (
                    "Increase...",
                    InspectorMutation::SetControl {
                        service: identity.service.clone(),
                        control: control.id.clone(),
                        value: serde_json::json!(upper),
                        identity: identity.clone(),
                    },
                ),
            ]
        }
        "action" => vec![(
            "Press...",
            InspectorMutation::SetControl {
                service: identity.service.clone(),
                control: control.id.clone(),
                value: serde_json::Value::Null,
                identity: identity.clone(),
            },
        )],
        _ => Vec::new(),
    }
}

fn render_properties_tree(
    commands: &mut Commands,
    panel: Entity,
    atlas: &AtlasState,
    disclosure_state: &PropertyDisclosureState,
) -> Vec<(TowerFocusKey, Entity)> {
    commands.entity(panel).despawn_children();
    if atlas.properties.is_empty() {
        replace_panel_text(
            commands,
            panel,
            "No local property surfaces observed".into(),
        );
        return Vec::new();
    }
    let view = spawn_tree_view(commands);
    commands.entity(panel).add_child(view);
    let mut focus = Vec::new();
    for (service, surface) in &atlas.properties {
        let expandable = !surface.paths.is_empty();
        let expanded = disclosure_state.0.get(service).copied().unwrap_or(true);
        let parent = commands.spawn_empty().id();
        let disclosure = spawn_tree_disclosure(commands, parent, 0, expandable, expanded);
        if expandable {
            let key = TowerFocusKey::Property(service.clone());
            commands.entity(disclosure).insert(key.clone());
            focus.push((key, disclosure));
        }
        let label = observed_age_text(
            commands,
            format!("{service} - {} - ", surface.status_label()),
            surface.observed_at_ms,
            "",
            12.0,
            false,
        );
        let item = if expandable {
            TreeItem::branch(view, None, expanded)
        } else {
            TreeItem::leaf(view, None)
        };
        commands.entity(parent).insert((
            tree_row_node(),
            item,
            PropertyBranch(service.clone()),
            ThemeBackgroundColor(tokens::PANEL),
        ));
        commands.entity(parent).add_children(&[disclosure, label]);
        commands.entity(view).add_child(parent);

        for path in &surface.paths {
            let row = commands.spawn_empty().id();
            let disclosure = spawn_tree_disclosure(commands, row, 1, false, false);
            let value = surface
                .snapshot
                .as_ref()
                .and_then(|snapshot| value_at_path(snapshot, path))
                .map(compact_json)
                .unwrap_or_else(|| "unknown".into());
            let description = surface
                .descriptions
                .get(path)
                .and_then(|description| description.get("description"))
                .and_then(serde_json::Value::as_str)
                .map(|description| format!(" - {description}"))
                .unwrap_or_default();
            let label = text(
                commands,
                &format!("{path} = {value}{description}"),
                11.0,
                true,
            );
            commands.entity(row).insert((
                tree_row_node(),
                TreeItem::leaf(view, Some(parent)),
                ThemeBackgroundColor(tokens::PANEL),
            ));
            commands.entity(row).add_children(&[disclosure, label]);
            commands.entity(view).add_child(row);
        }
    }
    sync_tree_view(commands, view);
    focus
}

fn tree_row_node() -> Node {
    Node {
        width: percent(100),
        min_height: px(25),
        flex_direction: FlexDirection::Row,
        align_items: AlignItems::Center,
        ..default()
    }
}

fn reachability_word(reachability: Reachability) -> &'static str {
    match reachability {
        Reachability::Observed => "observed",
        Reachability::Unknown | Reachability::Stale => "unknown",
    }
}

fn value_at_path<'a>(snapshot: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(snapshot, |value, segment| value.get(segment))
}

fn compact_json(value: &serde_json::Value) -> String {
    let mut rendered = match value {
        serde_json::Value::String(value) => value.clone(),
        _ => value.to_string(),
    };
    const MAX: usize = 96;
    if rendered.len() > MAX {
        let mut boundary = MAX;
        while !rendered.is_char_boundary(boundary) {
            boundary -= 1;
        }
        rendered.truncate(boundary);
        rendered.push_str("...");
    }
    rendered
}

const MAX_UI_TEXT_CHARS: usize = 8_192;

pub(crate) fn ascii_ui_text(content: &str) -> String {
    ascii_ui_text_bounded(content, MAX_UI_TEXT_CHARS)
}

pub(crate) fn ascii_ui_text_bounded(content: &str, limit: usize) -> String {
    let limit = limit.max(3);
    let mut rendered = String::with_capacity(content.len().min(limit));
    let mut truncated = false;
    for character in content.chars() {
        if rendered.len() >= limit.saturating_sub(3) {
            truncated = true;
            break;
        }
        match character {
            '\n' | '\t' => rendered.push(character),
            character if character.is_ascii() && !character.is_ascii_control() => {
                rendered.push(character);
            }
            _ => rendered.push('?'),
        }
    }
    if truncated {
        rendered.push_str("...");
    }
    rendered
}

fn text(commands: &mut Commands, content: &str, size: f32, dim: bool) -> Entity {
    commands
        .spawn((
            Text::new(ascii_ui_text(content)),
            TextFont::from_font_size(size),
            TextLayout {
                linebreak: LineBreak::WordOrCharacter,
                ..default()
            },
            ThemeTextColor(if dim { tokens::TEXT_DIM } else { tokens::TEXT }),
        ))
        .id()
}

fn observed_age_text(
    commands: &mut Commands,
    prefix: impl Into<String>,
    observed_at_ms: Option<u64>,
    suffix: impl Into<String>,
    size: f32,
    dim: bool,
) -> Entity {
    let marker = ObservedAgeText::single(prefix, observed_at_ms, suffix);
    let entity = text(
        commands,
        &marker.render_at(crate::model::now_unix_ms()),
        size,
        dim,
    );
    commands.entity(entity).insert(marker);
    entity
}

#[allow(clippy::too_many_arguments)]
fn observed_age_pair_text(
    commands: &mut Commands,
    prefix: impl Into<String>,
    first_observed_at_ms: Option<u64>,
    between: impl Into<String>,
    second_observed_at_ms: Option<u64>,
    suffix: impl Into<String>,
    size: f32,
    dim: bool,
) -> Entity {
    let marker = ObservedAgeText::pair(
        prefix,
        first_observed_at_ms,
        between,
        second_observed_at_ms,
        suffix,
    );
    let entity = text(
        commands,
        &marker.render_at(crate::model::now_unix_ms()),
        size,
        dim,
    );
    commands.entity(entity).insert(marker);
    entity
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::change_detection::DetectChanges;
    use bevy::ecs::system::RunSystemOnce;

    #[derive(Resource)]
    struct InspectorTestPanel(Entity);

    #[derive(Resource)]
    struct FilterTestPanel(Entity);

    fn render_test_inspector(
        mut commands: Commands,
        panel: bevy::prelude::Res<InspectorTestPanel>,
        atlas: bevy::prelude::Res<AtlasState>,
    ) {
        let _ = render_inspector(&mut commands, panel.0, &atlas);
    }

    fn render_test_filters(
        mut commands: Commands,
        panel: bevy::prelude::Res<FilterTestPanel>,
        atlas: bevy::prelude::Res<AtlasState>,
        saved: bevy::prelude::Res<SavedFilters>,
    ) {
        let _ = render_filters(&mut commands, panel.0, &atlas, &saved);
    }

    #[test]
    fn property_path_resolves_nested_snapshot_values() {
        let snapshot = serde_json::json!({
            "lifecycle": {"health": "ok"},
            "services": {"count": 7}
        });
        assert_eq!(
            value_at_path(&snapshot, "lifecycle.health"),
            Some(&serde_json::json!("ok"))
        );
        assert_eq!(
            value_at_path(&snapshot, "services.count"),
            Some(&serde_json::json!(7))
        );
        assert_eq!(value_at_path(&snapshot, "missing.path"), None);
    }

    #[test]
    fn panel_clips_horizontal_overflow_and_scrolls_vertically() {
        let mut world = bevy::ecs::world::World::new();
        let panel = world
            .run_system_once(|mut commands: Commands| panel(&mut commands))
            .unwrap();
        let node = world.entity(panel).get::<Node>().unwrap();
        assert_eq!(node.min_width, px(0));
        assert_eq!(node.overflow.x, OverflowAxis::Clip);
        assert_eq!(node.overflow.y, OverflowAxis::Scroll);
    }

    #[test]
    fn observed_age_refresh_is_timer_gated_and_skips_unchanged_text() {
        let mut app = bevy::app::App::new();
        app.init_resource::<Time>()
            .init_resource::<ObservedAgeRefresh>()
            .add_systems(bevy::app::Update, refresh_observed_age_texts);
        let entity = app
            .world_mut()
            .spawn((Text::new("stale"), ObservedAgeText::single("", None, "")))
            .id();

        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_millis(999));
        app.update();
        assert_eq!(app.world().entity(entity).get::<Text>().unwrap().0, "stale");

        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_millis(1));
        app.update();
        assert_eq!(
            app.world().entity(entity).get::<Text>().unwrap().0,
            "never observed"
        );

        app.world_mut().clear_trackers();
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_secs(1));
        app.update();
        assert!(
            !app.world()
                .entity(entity)
                .get_ref::<Text>()
                .unwrap()
                .is_changed(),
            "unchanged rendered ages must not write Text"
        );
    }

    #[test]
    fn compact_values_are_bounded_and_utf8_safe() {
        let rendered = compact_json(&serde_json::json!("x".repeat(200)));
        assert!(rendered.len() <= 99);
        assert!(rendered.ends_with("..."));

        let multibyte = compact_json(&serde_json::json!("界".repeat(40)));
        assert!(multibyte.is_char_boundary(multibyte.len()));
        assert!(multibyte.len() <= 99);
        assert!(multibyte.ends_with("..."));
    }

    #[test]
    fn rendered_ui_text_is_ascii_and_bounded() {
        let rendered = ascii_ui_text(&format!("alpha→界\n{}", "x".repeat(9_000)));
        assert!(rendered.is_ascii());
        assert!(rendered.len() <= MAX_UI_TEXT_CHARS);
        assert!(rendered.starts_with("alpha??\n"));
        assert!(rendered.ends_with("..."));
        assert_eq!(ascii_ui_text_bounded("abcdef", 5), "ab...");
    }

    #[test]
    fn ctk_action_controls_offer_a_confirmed_press() {
        let controls =
            crate::inspector::parse_controls_list(include_str!("fixtures/controls_list.json"))
                .unwrap();
        let action = controls
            .controls
            .iter()
            .find(|control| control.value_type == "action")
            .unwrap();
        let identity = ProcessIdentity {
            service: "studio-bevy-4242".into(),
            pid: Some(4242),
            started_at: None,
        };
        let mutations = control_mutations(&identity, action);
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].0, "Press...");
        assert!(matches!(
            &mutations[0].1,
            InspectorMutation::SetControl {
                service,
                control,
                value: serde_json::Value::Null,
                ..
            } if service == "studio-bevy-4242" && control == "transport.toggle"
        ));
    }

    #[test]
    fn saved_filter_controls_carry_stable_focus_components() {
        let mut saved = SavedFilters::default();
        saved
            .save_current("mesh mail", &crate::traffic::TrafficFilter::default())
            .unwrap();
        let mut app = bevy::app::App::new();
        let panel = app.world_mut().spawn(Node::default()).id();
        app.insert_resource(FilterTestPanel(panel))
            .insert_resource(AtlasState::default())
            .insert_resource(saved)
            .add_systems(bevy::app::Update, render_test_filters);
        app.update();
        let mut query = app.world_mut().query::<&TowerFocusKey>();
        let count = query
            .iter(app.world())
            .filter(|key| matches!(key, TowerFocusKey::Filter(_)))
            .count();
        assert_eq!(count, 4, "name, save, select and delete are keyed");
    }

    #[test]
    fn inspector_render_never_exceeds_the_entity_budget() {
        let identity = ProcessIdentity {
            service: "tower-load-test-4242".into(),
            pid: Some(4242),
            started_at: Some("fixture".into()),
        };
        let mut inspector = crate::inspector::CitizenInspector::pending(identity);
        let action_template =
            crate::inspector::parse_actions_list(include_str!("fixtures/actions_list.json"))
                .unwrap()
                .actions
                .remove(1);
        let control_template =
            crate::inspector::parse_controls_list(include_str!("fixtures/controls_list.json"))
                .unwrap()
                .controls
                .remove(0);
        for index in 0..crate::inspector::MAX_INSPECTOR_ITEMS {
            let mut action = action_template.clone();
            action.id = format!("action-{index}");
            action.category = Some(format!("category-{index}"));
            inspector.actions.insert(action.id.clone(), action);

            let mut control = control_template.clone();
            control.id = format!("control-{index}");
            control.value = Some(serde_json::Value::Bool(false));
            inspector.controls.insert(control.id.clone(), control);
        }
        inspector.actions_observed_at_ms = Some(1);
        inspector.controls_observed_at_ms = Some(1);

        let atlas = AtlasState {
            inspector: Some(inspector),
            ..Default::default()
        };
        let mut app = bevy::app::App::new();
        let panel = app.world_mut().spawn(Node::default()).id();
        app.insert_resource(InspectorTestPanel(panel))
            .insert_resource(atlas)
            .add_systems(bevy::app::Update, render_test_inspector);
        let before = app.world().iter_entities().count();
        app.update();
        let spawned = app.world().iter_entities().count() - before;
        assert!(
            spawned <= MAX_INSPECTOR_ENTITIES,
            "render spawned {spawned} inspector entities"
        );
    }

    #[test]
    fn property_disclosure_state_survives_a_panel_rebuild() {
        let mut app = bevy::app::App::new();
        app.init_resource::<PropertyDisclosureState>()
            .add_observer(on_property_disclosure_changed);
        let item = app.world_mut().spawn(PropertyBranch("noded".into())).id();
        app.world_mut().trigger(TreeViewChanged {
            item,
            expanded: false,
        });
        assert_eq!(
            app.world()
                .resource::<PropertyDisclosureState>()
                .0
                .get("noded"),
            Some(&false)
        );
    }
}
