//! Text-input-v3 for layer-shell windows (the host has no Winit primary window).
use super::RunnerState;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui::{ComputedUiRenderTargetInfo, ComputedUiTargetCamera, UiGlobalTransform};
use bevy::window::{Ime, WindowEvent};
use cosmix_shell::runtime::{ExternalImeEvent, ExternalImeKind, ExternalImeTarget, ImePurpose};
use wayland_client::protocol::{wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, QueueHandle, globals::GlobalList};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ContentHint, ContentPurpose, ZwpTextInputV3},
};

/// One text-input-v3 result for the target that was enabled when it arrived.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Delivery {
    target: Entity,
    window: Entity,
    external: bool,
    event: TextInputEvent,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum TextInputEvent {
    Disabled,
    Delete(u32, u32),
    Commit(String),
    Preedit(String, Option<(usize, usize)>),
}

impl Delivery {
    /// The Bevy `Ime` message for an `EditableText` target.
    fn ime(&self) -> Option<Ime> {
        let window = self.window;
        if self.external {
            return None;
        }
        Some(match &self.event {
            TextInputEvent::Disabled => Ime::Disabled { window },
            // Bevy's text input has no surrounding-text deletion.
            TextInputEvent::Delete(..) => return None,
            TextInputEvent::Commit(value) => Ime::Commit {
                window,
                value: value.clone(),
            },
            TextInputEvent::Preedit(value, cursor) => Ime::Preedit {
                window,
                value: value.clone(),
                cursor: *cursor,
            },
        })
    }

    fn external(&self) -> Option<ExternalImeEvent> {
        if !self.external {
            return None;
        }
        let kind = match &self.event {
            TextInputEvent::Disabled => ExternalImeKind::Disabled,
            TextInputEvent::Delete(before, after) => ExternalImeKind::DeleteSurrounding {
                before: *before,
                after: *after,
            },
            TextInputEvent::Commit(text) => ExternalImeKind::Commit(text.clone()),
            TextInputEvent::Preedit(text, cursor) => ExternalImeKind::Preedit {
                text: text.clone(),
                cursor: *cursor,
            },
        };
        Some(ExternalImeEvent {
            target: self.target,
            kind,
        })
    }
}

pub(super) struct TextInputBridge {
    manager: Option<ZwpTextInputManagerV3>,
    input: Option<ZwpTextInputV3>,
    surface: Option<WlSurface>,
    enabled: Option<(Entity, Entity)>,
    preedit: Option<(String, Option<(usize, usize)>)>,
    committed: Option<String>,
    deleted: Option<(u32, u32)>,
    rectangle: Option<(i32, i32, i32, i32)>,
    /// The enabled target is an `ExternalImeTarget`, not an `EditableText`.
    external: bool,
    purpose: Option<ImePurpose>,
    generation: u64,
    batch_generation: Option<u64>,
    commit_serial: u32,
    // First commit enabling the current focus generation. Rectangle commits
    // extend this generation's serial interval without invalidating input.
    focus_serial: u32,
}

impl TextInputBridge {
    pub(super) fn new(globals: &GlobalList, qh: &QueueHandle<RunnerState>) -> Self {
        Self {
            manager: globals.bind(qh, 1..=1, ()).ok(),
            input: None,
            surface: None,
            enabled: None,
            preedit: None,
            committed: None,
            deleted: None,
            rectangle: None,
            external: false,
            purpose: None,
            generation: 0,
            batch_generation: None,
            commit_serial: 0,
            focus_serial: 0,
        }
    }
    pub(super) fn attach(&mut self, seat: &WlSeat, qh: &QueueHandle<RunnerState>) {
        self.detach();
        self.input = self
            .manager
            .as_ref()
            .map(|manager| manager.get_text_input(seat, qh, ()));
    }
    pub(super) fn detach(&mut self) {
        if let Some(input) = self.input.take() {
            input.destroy();
        }
        self.surface = None;
        self.enabled = None;
        self.preedit = None;
        self.committed = None;
        self.deleted = None;
        self.rectangle = None;
        self.external = false;
        self.purpose = None;
        self.generation = self.generation.wrapping_add(1);
        self.batch_generation = None;
        self.commit_serial = 0;
        self.focus_serial = 0;
    }
}

impl RunnerState {
    pub(super) fn sync_text_input(&mut self) {
        if self.text_input.input.is_none() {
            return;
        }
        let focused = self
            .app
            .world()
            .get_resource::<InputFocus>()
            .and_then(InputFocus::get);
        let target = self.text_input.surface.as_ref().and_then(|surface| {
            self.surface_targets()
                .into_iter()
                .find(|target| target.surface == *surface)
        });
        let world = self.app.world();
        let enabled = focused.zip(target).and_then(|(entity, target)| {
            let external = world.get::<ExternalImeTarget>(entity);
            if world.get::<EditableText>(entity).is_none() && !external.is_some_and(|t| t.enabled) {
                return None;
            }
            let camera = world.get::<ComputedUiTargetCamera>(entity)?.get()?;
            let bevy::camera::RenderTarget::Window(bevy::window::WindowRef::Entity(window)) =
                world.get::<bevy::camera::RenderTarget>(camera)?
            else {
                return None;
            };
            (*window == target.window).then_some((entity, target.window))
        });
        let focus = enabled.map(|(entity, window)| {
            // An EditableText keeps its own path even if it also carries a target.
            match world.get::<EditableText>(entity) {
                Some(editable) => {
                    let rect = world
                        .get::<ComputedNode>(entity)
                        .zip(world.get::<UiGlobalTransform>(entity))
                        .zip(world.get::<ComputedUiRenderTargetInfo>(entity))
                        .map(|((node, transform), target)| {
                            let cursor = editable.editor().ime_cursor_area();
                            let scroll = world
                                .get::<bevy::ui::widget::TextScroll>(entity)
                                .map_or(Vec2::ZERO, |s| s.0);
                            let local = Vec2::new(cursor.x0 as f32, cursor.y1 as f32)
                                + node.content_box().min
                                - scroll;
                            let point =
                                transform.affine().transform_point2(local) / target.scale_factor();
                            (point.x as i32, point.y as i32, 1, 18)
                        });
                    TextInputFocus {
                        target: (entity, window),
                        external: None,
                        rect,
                    }
                }
                None => {
                    let external = world.get::<ExternalImeTarget>(entity).cloned();
                    let rect = external
                        .as_ref()
                        .and_then(|t| t.cursor)
                        .map(cursor_rectangle);
                    TextInputFocus {
                        target: (entity, window),
                        external: external.map(|t| t.purpose),
                        rect,
                    }
                }
            }
        });
        let (requests, notices) = self.text_input.plan(focus);
        let input = self.text_input.input.as_ref().unwrap();
        for request in requests {
            match request {
                TextInputRequest::Disable => input.disable(),
                TextInputRequest::Enable => input.enable(),
                TextInputRequest::ContentType(purpose) => {
                    let (hint, purpose) = content_type(purpose);
                    input.set_content_type(hint, purpose);
                }
                TextInputRequest::CursorRectangle(x, y, w, h) => {
                    input.set_cursor_rectangle(x, y, w, h)
                }
                TextInputRequest::Commit => input.commit(),
            }
        }
        for event in notices {
            self.app.world_mut().write_message(event);
            self.needs_update = true;
        }
    }

    fn emit_text_input(&mut self, delivery: &Delivery) {
        let world = self.app.world_mut();
        if let Some(event) = delivery.external() {
            world.write_message(event);
        } else if let Some(event) = delivery.ime() {
            world.write_message(event.clone());
            world.write_message(WindowEvent::Ime(event));
        }
        self.needs_update = true;
    }
}

fn content_type(purpose: ImePurpose) -> (ContentHint, ContentPurpose) {
    match purpose {
        ImePurpose::Normal => (ContentHint::None, ContentPurpose::Normal),
        ImePurpose::Password => (
            ContentHint::HiddenText | ContentHint::SensitiveData,
            ContentPurpose::Password,
        ),
        ImePurpose::Terminal => (ContentHint::None, ContentPurpose::Terminal),
    }
}

/// Integer surface-local rectangle covering a window-logical caret: the
/// origin rounds down and the far edges round up, so a caret at a fractional
/// position is never cut off.
pub(super) fn cursor_rectangle(rect: bevy::math::Rect) -> (i32, i32, i32, i32) {
    let x = rect.min.x.floor() as i32;
    let y = rect.min.y.floor() as i32;
    let right = rect.max.x.ceil() as i32;
    let bottom = rect.max.y.ceil() as i32;
    (x, y, (right - x).max(1), (bottom - y).max(1))
}

impl Dispatch<ZwpTextInputManagerV3, ()> for RunnerState {
    fn event(
        _: &mut Self,
        _: &ZwpTextInputManagerV3,
        _: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for RunnerState {
    fn event(
        state: &mut Self,
        input: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.text_input.input.as_ref() != Some(input) {
            return;
        }
        let focus = state
            .app
            .world()
            .get_resource::<InputFocus>()
            .and_then(InputFocus::get);
        for delivery in state.text_input.receive(event, focus) {
            state.emit_text_input(&delivery);
        }
        state.needs_update = true;
    }
}

/// What the focused entity wants from text input this update.
pub(super) struct TextInputFocus {
    target: (Entity, Entity),
    /// `Some` for an `ExternalImeTarget`.
    external: Option<ImePurpose>,
    rect: Option<(i32, i32, i32, i32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextInputRequest {
    Disable,
    Enable,
    ContentType(ImePurpose),
    CursorRectangle(i32, i32, i32, i32),
    Commit,
}

impl TextInputBridge {
    /// Protocol requests for the focus change, with commit serials tracked
    /// exactly as the dispatch path expects: enable starts a focus generation,
    /// rectangle and content-type commits extend it.
    fn plan(
        &mut self,
        focus: Option<TextInputFocus>,
    ) -> (Vec<TextInputRequest>, Vec<ExternalImeEvent>) {
        use TextInputRequest as R;
        let mut requests = Vec::new();
        let mut notices = Vec::new();
        let enabled = focus.as_ref().map(|f| f.target);
        let external = focus.as_ref().and_then(|f| f.external);
        fn commit(bridge: &mut TextInputBridge, requests: &mut Vec<TextInputRequest>) {
            requests.push(TextInputRequest::Commit);
            bridge.commit_serial = bridge.commit_serial.wrapping_add(1);
        }
        if enabled != self.enabled {
            if let Some((old, _)) = self.enabled {
                requests.push(R::Disable);
                commit(self, &mut requests);
                if self.external {
                    notices.push(ExternalImeEvent {
                        target: old,
                        kind: ExternalImeKind::Disabled,
                    });
                }
            }
            self.preedit = None;
            self.committed = None;
            self.deleted = None;
            self.rectangle = None;
            self.enabled = enabled;
            self.external = external.is_some();
            self.purpose = external;
            self.generation = self.generation.wrapping_add(1);
            self.batch_generation = None;
            if let Some((entity, _)) = enabled {
                requests.push(R::Enable);
                if let Some(purpose) = external {
                    requests.push(R::ContentType(purpose));
                    notices.push(ExternalImeEvent {
                        target: entity,
                        kind: ExternalImeKind::Enabled,
                    });
                }
                commit(self, &mut requests);
                self.focus_serial = self.commit_serial;
            }
        } else if let Some(purpose) = external
            && external != self.purpose
        {
            requests.push(R::ContentType(purpose));
            commit(self, &mut requests);
            self.purpose = external;
        }
        if let Some(focus) = focus
            && focus.rect != self.rectangle
            && let Some((x, y, w, h)) = focus.rect
        {
            requests.push(R::CursorRectangle(x, y, w, h));
            commit(self, &mut requests);
            self.rectangle = focus.rect;
        }
        (requests, notices)
    }

    fn leave(&mut self) -> Option<Delivery> {
        let disabled = self.enabled.take().map(|(target, window)| Delivery {
            target,
            window,
            external: self.external,
            event: TextInputEvent::Disabled,
        });
        self.surface = None;
        self.preedit = None;
        self.committed = None;
        self.deleted = None;
        self.generation = self.generation.wrapping_add(1);
        self.batch_generation = None;
        disabled
    }

    // This is the protocol dispatch path, shared by the real Wayland callback
    // and regression tests. The done serial identifies the client commit, not
    // whichever field happens to have focus when an old batch arrives.
    fn receive(&mut self, event: zwp_text_input_v3::Event, focus: Option<Entity>) -> Vec<Delivery> {
        let mut events = Vec::new();
        let external = self.external;
        let deliver = |(target, window): (Entity, Entity), event| Delivery {
            target,
            window,
            external,
            event,
        };
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                self.surface = Some(surface);
            }
            zwp_text_input_v3::Event::Leave { .. } => events.extend(self.leave()),
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                let text = text.unwrap_or_default();
                let cursor = (cursor_begin >= 0
                    && cursor_end >= 0
                    && cursor_begin as usize <= text.len()
                    && cursor_end as usize <= text.len())
                .then_some((cursor_begin as usize, cursor_end as usize));
                self.batch_generation.get_or_insert(self.generation);
                self.preedit = Some((text, cursor));
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                self.batch_generation.get_or_insert(self.generation);
                self.committed = text;
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                self.batch_generation.get_or_insert(self.generation);
                self.deleted = Some((before_length, after_length));
            }
            zwp_text_input_v3::Event::Done { serial } => {
                let committed = self.committed.take();
                let preedit = self.preedit.take();
                let deleted = self.deleted.take();
                let generation = self.batch_generation.take();
                if let Some(enabled) = self.enabled
                    && focus == Some(enabled.0)
                    && generation == Some(self.generation)
                    && serial.wrapping_sub(self.focus_serial)
                        <= self.commit_serial.wrapping_sub(self.focus_serial)
                {
                    // Protocol order: delete, commit, then the new preedit.
                    if let Some((before, after)) = deleted {
                        events.push(deliver(enabled, TextInputEvent::Delete(before, after)));
                    }
                    if let Some(value) = committed {
                        events.push(deliver(enabled, TextInputEvent::Commit(value)));
                    }
                    if let Some((value, cursor)) = preedit {
                        events.push(deliver(enabled, TextInputEvent::Preedit(value, cursor)));
                    }
                }
            }
            _ => {}
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ime(deliveries: Vec<Delivery>) -> Vec<Ime> {
        deliveries.iter().filter_map(Delivery::ime).collect()
    }
    #[test]
    fn protocol_bridge_accepts_inflight_input_across_rectangle_commit() {
        let mut world = World::new();
        let field = world.spawn_empty().id();
        let window = world.spawn_empty().id();
        for start in [1, u32::MAX] {
            let mut bridge = TextInputBridge {
                manager: None,
                input: None,
                surface: None,
                enabled: Some((field, window)),
                preedit: None,
                committed: None,
                deleted: None,
                rectangle: None,
                external: false,
                purpose: None,
                generation: 1,
                batch_generation: None,
                commit_serial: start,
                focus_serial: start,
            };
            bridge.receive(
                zwp_text_input_v3::Event::CommitString {
                    text: Some("typed".into()),
                },
                Some(field),
            );
            bridge.commit_serial = start.wrapping_add(1); // cursor rectangle commit
            let events = ime(bridge.receive(
                zwp_text_input_v3::Event::Done { serial: start },
                Some(field),
            ));
            assert!(matches!(&events[..], [Ime::Commit { value, .. }] if value == "typed"));
            // A whole batch can also arrive after the rectangle was committed.
            bridge.receive(
                zwp_text_input_v3::Event::PreeditString {
                    text: Some("compose".into()),
                    cursor_begin: 0,
                    cursor_end: 7,
                },
                Some(field),
            );
            let events = ime(bridge.receive(
                zwp_text_input_v3::Event::Done { serial: start },
                Some(field),
            ));
            assert!(matches!(&events[..], [Ime::Preedit { value, .. }] if value == "compose"));
            bridge.receive(
                zwp_text_input_v3::Event::CommitString {
                    text: Some("future".into()),
                },
                Some(field),
            );
            assert!(
                bridge
                    .receive(
                        zwp_text_input_v3::Event::Done {
                            serial: start.wrapping_add(2)
                        },
                        Some(field),
                    )
                    .is_empty()
            );
        }
    }

    #[test]
    fn protocol_bridge_drops_delayed_batches_after_focus_change() {
        let mut world = World::new();
        let first = world.spawn_empty().id();
        let second = world.spawn_empty().id();
        let window = world.spawn_empty().id();
        let mut bridge = TextInputBridge {
            manager: None,
            input: None,
            surface: None,
            enabled: Some((first, window)),
            preedit: None,
            committed: None,
            deleted: None,
            rectangle: None,
            external: false,
            purpose: None,
            generation: 1,
            batch_generation: None,
            commit_serial: 1,
            focus_serial: 1,
        };
        bridge.receive(
            zwp_text_input_v3::Event::CommitString {
                text: Some("old".into()),
            },
            Some(first),
        );
        bridge.enabled = Some((second, window));
        bridge.generation = 2;
        bridge.commit_serial = 3; // disable + enable commits
        bridge.focus_serial = 3;
        assert!(
            bridge
                .receive(zwp_text_input_v3::Event::Done { serial: 1 }, Some(second))
                .is_empty()
        );
        // An entire old batch can arrive after the new field was enabled.
        bridge.receive(
            zwp_text_input_v3::Event::PreeditString {
                text: Some("stale".into()),
                cursor_begin: 0,
                cursor_end: 0,
            },
            Some(second),
        );
        assert!(
            bridge
                .receive(zwp_text_input_v3::Event::Done { serial: 1 }, Some(second))
                .is_empty()
        );
        bridge.receive(
            zwp_text_input_v3::Event::CommitString {
                text: Some("new".into()),
            },
            Some(second),
        );
        let events =
            ime(bridge.receive(zwp_text_input_v3::Event::Done { serial: 3 }, Some(second)));
        assert!(matches!(&events[..], [Ime::Commit { value, .. }] if value == "new"));
        assert!(
            bridge
                .receive(zwp_text_input_v3::Event::Done { serial: 3 }, Some(second))
                .is_empty()
        );
    }
    fn bridge() -> TextInputBridge {
        TextInputBridge {
            manager: None,
            input: None,
            surface: None,
            enabled: None,
            preedit: None,
            committed: None,
            deleted: None,
            rectangle: None,
            external: false,
            purpose: None,
            generation: 0,
            batch_generation: None,
            commit_serial: 0,
            focus_serial: 0,
        }
    }

    fn external(
        target: (Entity, Entity),
        purpose: ImePurpose,
        rect: Option<(i32, i32, i32, i32)>,
    ) -> Option<TextInputFocus> {
        Some(TextInputFocus {
            target,
            external: Some(purpose),
            rect,
        })
    }

    #[test]
    fn external_target_enable_update_disable_flow() {
        use TextInputRequest as R;
        let mut world = World::new();
        let surface = world.spawn_empty().id();
        let field = world.spawn_empty().id();
        let window = world.spawn_empty().id();
        let mut bridge = bridge();
        let caret = Some((10, 20, 2, 19));

        let (requests, notices) =
            bridge.plan(external((surface, window), ImePurpose::Normal, caret));
        assert_eq!(
            requests,
            [
                R::Enable,
                R::ContentType(ImePurpose::Normal),
                R::Commit,
                R::CursorRectangle(10, 20, 2, 19),
                R::Commit
            ]
        );
        assert_eq!(
            notices,
            [ExternalImeEvent {
                target: surface,
                kind: ExternalImeKind::Enabled
            }]
        );
        // The enable commit starts the generation; the rectangle extends it.
        assert_eq!((bridge.focus_serial, bridge.commit_serial), (1, 2));
        let generation = bridge.generation;

        // Unchanged focus: nothing to send.
        let (requests, notices) =
            bridge.plan(external((surface, window), ImePurpose::Normal, caret));
        assert!(requests.is_empty() && notices.is_empty());

        // Caret moves and purpose changes within the same focus generation.
        let (requests, _) = bridge.plan(external(
            (surface, window),
            ImePurpose::Password,
            Some((30, 20, 2, 19)),
        ));
        assert_eq!(
            requests,
            [
                R::ContentType(ImePurpose::Password),
                R::Commit,
                R::CursorRectangle(30, 20, 2, 19),
                R::Commit
            ]
        );
        assert_eq!(bridge.generation, generation);
        assert_eq!(bridge.focus_serial, 1);

        // In-flight input from the enable serial still lands after those commits.
        bridge.receive(
            zwp_text_input_v3::Event::CommitString {
                text: Some("x".into()),
            },
            Some(surface),
        );
        let delivered = bridge.receive(zwp_text_input_v3::Event::Done { serial: 1 }, Some(surface));
        assert_eq!(delivered.len(), 1);
        assert!(
            ime(delivered.clone()).is_empty(),
            "external input is not a Bevy Ime"
        );
        assert_eq!(
            delivered[0].external(),
            Some(ExternalImeEvent {
                target: surface,
                kind: ExternalImeKind::Commit("x".into())
            })
        );

        // Focus moves to a CTK field: the surface is told, the field is not
        // given a content type, and its generation is new.
        let (requests, notices) = bridge.plan(Some(TextInputFocus {
            target: (field, window),
            external: None,
            rect: Some((5, 5, 1, 18)),
        }));
        assert_eq!(
            requests,
            [
                R::Disable,
                R::Commit,
                R::Enable,
                R::Commit,
                R::CursorRectangle(5, 5, 1, 18),
                R::Commit
            ]
        );
        assert_eq!(
            notices,
            [ExternalImeEvent {
                target: surface,
                kind: ExternalImeKind::Disabled
            }]
        );
        assert!(!bridge.external);
        assert_ne!(bridge.generation, generation);

        // And away entirely.
        let (requests, notices) = bridge.plan(None);
        assert_eq!(requests, [R::Disable, R::Commit]);
        assert!(notices.is_empty());
        assert_eq!(bridge.enabled, None);
    }

    #[test]
    fn external_batches_keep_protocol_order_and_leave_disables() {
        let mut world = World::new();
        let surface = world.spawn_empty().id();
        let window = world.spawn_empty().id();
        let mut bridge = bridge();
        bridge.plan(external((surface, window), ImePurpose::Normal, None));
        for event in [
            zwp_text_input_v3::Event::PreeditString {
                text: Some("ka".into()),
                cursor_begin: 2,
                cursor_end: 2,
            },
            zwp_text_input_v3::Event::CommitString {
                text: Some("か".into()),
            },
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length: 1,
                after_length: 0,
            },
        ] {
            assert!(bridge.receive(event, Some(surface)).is_empty());
        }
        let serial = bridge.focus_serial;
        let kinds: Vec<_> = bridge
            .receive(zwp_text_input_v3::Event::Done { serial }, Some(surface))
            .iter()
            .filter_map(Delivery::external)
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ExternalImeKind::DeleteSurrounding {
                    before: 1,
                    after: 0
                },
                ExternalImeKind::Commit("か".into()),
                ExternalImeKind::Preedit {
                    text: "ka".into(),
                    cursor: Some((2, 2))
                },
            ]
        );
        // Input for a different focus is dropped, as for CTK fields.
        bridge.receive(
            zwp_text_input_v3::Event::CommitString {
                text: Some("late".into()),
            },
            Some(surface),
        );
        assert!(
            bridge
                .receive(zwp_text_input_v3::Event::Done { serial }, Some(window))
                .is_empty()
        );
        let left = bridge.leave();
        assert_eq!(
            left.iter()
                .filter_map(Delivery::external)
                .collect::<Vec<_>>(),
            [ExternalImeEvent {
                target: surface,
                kind: ExternalImeKind::Disabled
            }]
        );
    }

    #[test]
    fn a_ctk_field_never_gets_delete_or_external_events() {
        let mut world = World::new();
        let field = world.spawn_empty().id();
        let window = world.spawn_empty().id();
        let mut bridge = bridge();
        let (requests, notices) = bridge.plan(Some(TextInputFocus {
            target: (field, window),
            external: None,
            rect: None,
        }));
        assert_eq!(
            requests,
            [TextInputRequest::Enable, TextInputRequest::Commit]
        );
        assert!(notices.is_empty());
        bridge.receive(
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length: 3,
                after_length: 0,
            },
            Some(field),
        );
        bridge.receive(
            zwp_text_input_v3::Event::CommitString {
                text: Some("ok".into()),
            },
            Some(field),
        );
        let delivered = bridge.receive(zwp_text_input_v3::Event::Done { serial: 1 }, Some(field));
        assert!(delivered.iter().all(|d| d.external().is_none()));
        assert!(
            matches!(&ime(delivered)[..], [Ime::Commit { value, window: w }] if value == "ok" && *w == window)
        );
    }

    #[test]
    fn caret_rectangle_covers_fractional_positions() {
        use bevy::math::Rect;
        // 250 % scale: a 3 px physical caret at physical (26, 51) is
        // logical (10.4, 20.4) .. (11.6, 30.0).
        let rect = Rect::from_corners(Vec2::new(26.0, 51.0) / 2.5, Vec2::new(29.0, 75.0) / 2.5);
        assert_eq!(cursor_rectangle(rect), (10, 20, 2, 10));
        // A zero-width caret still has a visible rectangle.
        let thin = Rect::from_corners(Vec2::new(4.0, 4.0), Vec2::new(4.0, 20.0));
        assert_eq!(cursor_rectangle(thin), (4, 4, 1, 16));
        // Integer rectangles pass through unchanged.
        let exact = Rect::from_corners(Vec2::new(8.0, 8.0), Vec2::new(10.0, 26.0));
        assert_eq!(cursor_rectangle(exact), (8, 8, 2, 18));
    }
}
