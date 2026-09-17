//! Keyboard and IME for content the compositor draws itself.
//!
//! A Wayland client gets keys through the seat and IME through
//! `zwp_text_input_v3`. In-process content (a scene mounted in comp's own
//! Bevy app) has no client and no surface, so both paths end at the
//! compositor. This module is the bridge that closes them.
//!
//! Everything the content receives — seat keys (already past the binding
//! filter), input-method output (through the vendored sink, see
//! `vendor/README.md`), focus edges, and notice of a drop — rides ONE queue
//! in [`NativeInputBridge`], so the order the render thread reads is the
//! order the compositor produced. The other direction (enable, caret
//! rectangle, surrounding text, content type) goes out through [`NativeIme`].
//!
//! ## The fence
//!
//! The queue crosses two threads and a frame boundary, so every event
//! carries a `generation` and a `seq`:
//!
//! - `seq` is one shared monotonic counter, so a consumer can tell a key
//!   that arrived before a commit from one that arrived after it.
//! - `generation` is bumped on every native focus edge and every IME
//!   enable/disable transition. A drained event whose generation is not the
//!   current one is discarded: the field it was aimed at is gone. An IME
//!   batch is stamped with the generation it STARTED in, so a focus edge
//!   between a preedit and its `done` discards both halves rather than
//!   applying one of them to the next field.
//!
//! Both values are on the [`NativeInput`] message, so content that keeps its
//! own state can fence against them.
//!
//! The bridge is an `Arc<Mutex<..>>` handle like [`crate::native_shell`],
//! because the protocol thread writes it and the render thread reads it.
//!
//! ## Who installs what
//!
//! The `native-input` feature installs [`NativeInputPlugin`] unconditionally
//! (`install`, called from the nested and KMS app builders): the bridges
//! exist, the pump runs, and nothing flows until some content asks for
//! focus. The test probe at the bottom of this file is the only part behind
//! `COSMIX_COMP_NATIVE_INPUT_PROBE=1`. An in-crate consumer (a mounted
//! scene) needs no environment variable: it takes [`NativeKeyboard`] and
//! [`NativeIme`] as resources, claims an owner with
//! [`NativeKeyboard::claim`], and asks with [`NativeKeyboard::request_focus`].
//! Focus is arbitrated by the compositor, so the request is a request: a
//! session lock, an exclusive layer, an override-redirect X11 surface, a
//! non-interactive layer, `comp.window.focus` and xdg-activation all still
//! win.

use bevy::input::{
    ButtonState,
    keyboard::{Key, KeyCode, KeyboardInput, NativeKey, NativeKeyCode},
};
use bevy::prelude::*;
use smithay::input::keyboard::{ModifiersState, xkb};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// The most events kept for the render thread; a scene that stops draining
/// (a stalled frame) loses the newest rather than growing without bound.
const MAX_PENDING_EVENTS: usize = 256;

/// One seat key, as the compositor resolved it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NativeKeyEvent {
    /// evdev code (the XKB keycode minus 8).
    pub(crate) evdev: u32,
    /// The keysym after the layout and modifiers.
    pub(crate) keysym: u32,
    /// The text this key produces, if any.
    pub(crate) text: Option<String>,
    pub(crate) pressed: bool,
    /// A compositor-generated repeat rather than a device event.
    pub(crate) repeat: bool,
    pub(crate) modifiers: NativeModifiers,
}

/// The modifier state at the moment of a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NativeModifiers {
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
    pub(crate) logo: bool,
    pub(crate) caps_lock: bool,
    pub(crate) num_lock: bool,
}

impl From<&ModifiersState> for NativeModifiers {
    fn from(modifiers: &ModifiersState) -> Self {
        Self {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            logo: modifiers.logo,
            caps_lock: modifiers.caps_lock,
            num_lock: modifiers.num_lock,
        }
    }
}

/// Which consumer a focus request came from.
///
/// One bridge serves the whole process, so the keyboard has one owner at a
/// time: the last requester wins and every focus edge names the owner it
/// belongs to, which is how a displaced consumer learns it lost the
/// keyboard rather than silently sharing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NativeOwner(u64);

impl NativeOwner {
    /// Mint one. The Bevy side goes through [`NativeKeyboard::claim`].
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// The owner's number, for a consumer that wants to log it.
    pub(crate) fn id(self) -> u64 {
        self.0
    }
}

/// What the input method produced for in-process content, in the shape the
/// vendored sink hands over.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeImeEvent {
    /// Insert this text at the cursor.
    Commit(String),
    /// Replace the composing text.
    Preedit {
        text: String,
        cursor_begin: i32,
        cursor_end: i32,
    },
    /// Delete around the cursor, in bytes.
    DeleteSurrounding { before: u32, after: u32 },
    /// End of a batch: apply it, or discard it.
    Done { discard: bool },
}

/// What in-process content asks of the input method.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeImeRequest {
    /// The field took or gave up the input method.
    Enable(bool),
    /// The caret rectangle, in comp's GLOBAL logical coordinates — the same
    /// space `comp.windows.list` reports `x`/`y` in, which spans every
    /// output. The compositor anchors the candidate popup under it, and
    /// hands the same rectangle to the input method as the text-input
    /// rectangle, so the two can never disagree.
    Caret {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// `zwp_text_input_v3.set_surrounding_text`.
    SurroundingText {
        text: String,
        cursor: u32,
        anchor: u32,
    },
    /// `zwp_text_input_v3.set_content_type` (hint and purpose as the
    /// protocol numbers them).
    ContentType { hint: u32, purpose: u32 },
    /// End the batch of requests above.
    Done,
}

/// One thing that happened to in-process content, fenced.
///
/// This is the ONLY message the bridge writes: a consumer reads keys, IME
/// and focus from one stream, in arrival order, and can discard anything
/// whose `generation` is not the one its field is living in.
#[derive(Clone, Debug, PartialEq, Message)]
pub(crate) struct NativeInput {
    /// Bumped on every focus edge and IME enable/disable transition.
    pub(crate) generation: u64,
    /// Monotonic across keys AND IME, so arrival order is recoverable.
    pub(crate) seq: u64,
    pub(crate) event: NativeInputEvent,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeInputEvent {
    /// A seat key the binding filter did not take. `modifiers` is the seat's
    /// state at that moment: comp does not write Bevy's own `KeyboardInput`,
    /// so `ButtonInput<KeyCode>` cannot answer "was Ctrl held?" for these.
    Key {
        input: KeyboardInput,
        modifiers: NativeModifiers,
    },
    /// Input-method output.
    Ime(NativeImeEvent),
    /// The compositor granted or took back the keyboard. On `false` the
    /// content must release whatever it thinks is held: the seat's own
    /// releases went to whoever has the keyboard now.
    Focus {
        owner: Option<NativeOwner>,
        focused: bool,
    },
    /// The queue overflowed and this many events never arrived. Whole IME
    /// batches are dropped, never half of one.
    Dropped { events: u64 },
}

/// The IME batch being assembled, so the whole of it shares one fate.
#[derive(Clone, Copy)]
struct OpenBatch {
    id: u64,
    /// The generation the batch started in; every event of it is stamped
    /// with this, so a focus edge mid-batch discards all of it.
    generation: u64,
    /// The batch overflowed: the rest of it is discarded too.
    dropped: bool,
    /// A generation edge happened mid-batch: the batch is abandoned, and
    /// only the `done` that ends it still belongs to it.
    stale: bool,
}

/// One queued event, before the render thread turns it into a message.
/// [`NativeInputEvent`] is the shape a consumer sees; this is what crosses
/// the threads, and what the protocol tests read back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeQueuedEvent {
    Key(NativeKeyEvent),
    Ime(NativeImeEvent),
    Focus {
        owner: Option<NativeOwner>,
        focused: bool,
    },
}

struct Pending {
    generation: u64,
    seq: u64,
    batch: Option<u64>,
    event: NativeQueuedEvent,
}

#[derive(Default)]
struct BridgeState {
    /// Who is asking for the keyboard.
    wanted: Option<NativeOwner>,
    /// Who holds it, once arbitration agreed.
    owner: Option<NativeOwner>,
    focused: bool,
    /// The content claims the input method.
    enabled: bool,
    /// The compositor actually activated an input-method instance for this
    /// field (what `enabled` asked for, once it was granted).
    ime_active: bool,
    /// The compositor accepted this bridge as its one owner.
    installed: bool,
    /// The latest caret rectangle (global logical), for the popup anchor.
    caret: Option<(i32, i32, i32, i32)>,
    generation: u64,
    seq: u64,
    next_batch: u64,
    batch: Option<OpenBatch>,
    queue: VecDeque<Pending>,
    dropped: u64,
}

impl BridgeState {
    /// A focus edge or an IME enable/disable: everything aimed at the old
    /// field is stale from here.
    fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
        if let Some(open) = self.batch.as_mut() {
            open.stale = true;
        }
    }

    /// Start a batch in the current generation.
    fn open_batch(&mut self) -> OpenBatch {
        let open = OpenBatch {
            id: self.next_batch,
            generation: self.generation,
            dropped: false,
            stale: false,
        };
        self.next_batch = self.next_batch.saturating_add(1);
        self.batch = Some(open);
        open
    }

    fn enqueue(&mut self, event: NativeQueuedEvent, generation: u64, batch: Option<u64>) {
        if self.queue.len() >= MAX_PENDING_EVENTS {
            // Drop the NEWEST, not the oldest: what a stalled consumer has
            // not seen yet is still a consistent prefix. An event inside an
            // IME batch takes the whole batch with it — half a batch would
            // leave a preedit on screen with no `done` to end it.
            match batch {
                Some(batch) => {
                    let before = self.queue.len();
                    self.queue.retain(|pending| pending.batch != Some(batch));
                    let removed = (before - self.queue.len()) as u64;
                    if let Some(open) = self.batch.as_mut().filter(|open| open.id == batch) {
                        open.dropped = true;
                    }
                    self.dropped = self.dropped.saturating_add(removed + 1);
                }
                None => self.dropped = self.dropped.saturating_add(1),
            }
            return;
        }
        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        self.queue.push_back(Pending {
            generation,
            seq,
            batch,
            event,
        });
    }
}

/// Keys, IME and focus for in-process content. The protocol thread pushes;
/// the render thread drains.
#[derive(Resource, Clone, Default)]
pub(crate) struct NativeInputBridge(Arc<Mutex<BridgeState>>);

/// What one drain handed over.
struct NativeDrain {
    events: Vec<(u64, u64, NativeQueuedEvent)>,
    dropped: u64,
    generation: u64,
}

impl NativeInputBridge {
    fn state(&self) -> std::sync::MutexGuard<'_, BridgeState> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The standing request for the keyboard, read inside arbitration.
    pub(crate) fn wants_focus(&self) -> bool {
        self.state().wanted.is_some()
    }

    /// What arbitration decided. Keys are only queued while this is set.
    pub(crate) fn focused(&self) -> bool {
        self.state().focused
    }

    /// Whether the content currently claims the input method. This is the
    /// last request the field made, not the compositor's answer — see
    /// [`Self::ime_active`].
    pub(crate) fn enabled(&self) -> bool {
        self.state().enabled
    }

    /// Whether the compositor has an input-method instance activated for
    /// this field right now.
    pub(crate) fn ime_active(&self) -> bool {
        self.state().ime_active
    }

    /// Set by the protocol thread when it activates or deactivates the
    /// input method for this field.
    pub(crate) fn set_ime_active(&self, active: bool) {
        self.state().ime_active = active;
    }

    /// Whether the compositor took this bridge as its owner. A second
    /// bridge is refused, and reports `false` here for ever.
    pub(crate) fn installed(&self) -> bool {
        self.state().installed
    }

    /// The compositor accepted (or released) this bridge. Releasing also
    /// clears the field state, so a re-install never starts with a stale
    /// standing request, caret or enable.
    ///
    /// It deliberately does NOT bump the generation: the focus edge that
    /// announces the release has already bumped, and a second bump here
    /// would discard that very edge at the next drain.
    pub(crate) fn set_installed(&self, installed: bool) {
        let mut state = self.state();
        state.installed = installed;
        if !installed {
            state.wanted = None;
            state.owner = None;
            state.enabled = false;
            state.ime_active = false;
            state.caret = None;
            state.batch = None;
        }
    }

    /// The caret rectangle the content last reported, in comp's global
    /// logical coordinates; the popup anchor uses it when the caret owner
    /// has no client surface.
    pub(crate) fn caret(&self) -> Option<(i32, i32, i32, i32)> {
        self.state().caret
    }

    /// Who holds the keyboard, if anyone.
    pub(crate) fn focus_owner(&self) -> Option<NativeOwner> {
        self.state().owner
    }

    /// Record arbitration's decision and tell the content, on the edge.
    ///
    /// The edge is the keyboard changing hands — INCLUDING between two
    /// in-process owners while the compositor keeps the keyboard. Without
    /// that second case the displaced owner would go on consuming the new
    /// owner's keys, and `focus_owner()` would name the wrong one.
    pub(crate) fn set_focused(&self, focused: bool) -> bool {
        let mut state = self.state();
        let next = focused.then_some(state.wanted).flatten();
        if state.focused == focused && state.owner == next {
            return false;
        }
        if state.owner.is_some() && state.owner != next {
            // A different field from here on: the input method the previous
            // owner had is not this one's, and neither is its caret — an
            // inherited caret would anchor the candidate window on the field
            // that has gone.
            state.enabled = false;
            state.ime_active = false;
            state.caret = None;
        }
        let previous = state.owner;
        state.focused = focused;
        state.owner = next;
        // The edge invalidates everything queued for the old field, and the
        // edge events themselves ride the NEW generation so they survive the
        // drain that discards them.
        state.bump_generation();
        let generation = state.generation;
        if focused && previous.is_some() && previous != next {
            // A handover: the displaced owner is told first, in order.
            state.enqueue(
                NativeQueuedEvent::Focus {
                    owner: previous,
                    focused: false,
                },
                generation,
                None,
            );
        }
        state.enqueue(
            NativeQueuedEvent::Focus {
                owner: next,
                focused,
            },
            generation,
            None,
        );
        true
    }

    /// One seat key for the content.
    pub(crate) fn push_key(&self, event: NativeKeyEvent) {
        let mut state = self.state();
        if !state.focused {
            return;
        }
        let generation = state.generation;
        state.enqueue(NativeQueuedEvent::Key(event), generation, None);
    }

    /// One input-method event, from the vendored sink.
    ///
    /// A field that is not focused, or has not enabled the input method,
    /// receives nothing: the input method is talking to whoever holds it.
    pub(crate) fn push_ime(&self, event: NativeImeEvent) {
        let mut state = self.state();
        if !state.focused || !state.enabled {
            return;
        }
        let ends_batch = matches!(event, NativeImeEvent::Done { .. });
        let open = match state.batch {
            // A generation edge voided this batch. It stays OPEN until its
            // own `done` arrives, and everything in it is discarded: the
            // input method is still finishing a composition aimed at the
            // field that has gone, and starting a fresh batch here would
            // land its commit in the new one.
            Some(open) if open.stale => {
                if ends_batch {
                    state.batch = None;
                }
                return;
            }
            Some(open) => open,
            None => state.open_batch(),
        };
        if ends_batch {
            state.batch = None;
        }
        if open.dropped {
            // The rest of a batch whose head was dropped; delivering it
            // would apply a commit whose preedit the content never saw.
            return;
        }
        state.enqueue(NativeQueuedEvent::Ime(event), open.generation, Some(open.id));
    }

    /// Track what the content told the input method. An enable/disable is a
    /// generation edge for the same reason a focus change is: what the
    /// input method sends next belongs to a different field.
    pub(crate) fn note_request(&self, request: &NativeImeRequest) {
        let mut state = self.state();
        match *request {
            NativeImeRequest::Enable(enabled) => {
                if state.enabled != enabled {
                    state.enabled = enabled;
                    state.bump_generation();
                }
                if !enabled {
                    state.caret = None;
                }
            }
            NativeImeRequest::Caret {
                x,
                y,
                width,
                height,
            } => state.caret = Some((x, y, width, height)),
            _ => {}
        }
    }

    /// Claim or release the keyboard for one owner. Returns whether the
    /// standing request changed (and so needs re-arbitration). The Bevy side
    /// goes through [`NativeKeyboard::request_focus`], which also pokes the
    /// protocol thread.
    pub(crate) fn set_wants_focus(&self, owner: NativeOwner, wanted: bool) -> bool {
        let mut state = self.state();
        match (wanted, state.wanted) {
            (true, Some(current)) if current == owner => false,
            (true, previous) => {
                if let Some(previous) = previous {
                    warn!(
                        previous = previous.id(),
                        owner = owner.id(),
                        "native keyboard focus taken over by another owner"
                    );
                }
                state.wanted = Some(owner);
                true
            }
            // A stale owner cannot give away focus somebody else asked for.
            (false, Some(current)) if current == owner => {
                state.wanted = None;
                true
            }
            (false, _) => false,
        }
    }

    /// Everything queued that still belongs to the current generation.
    fn drain(&self) -> NativeDrain {
        let mut state = self.state();
        let mut dropped = std::mem::take(&mut state.dropped);
        let generation = state.generation;
        let mut events = Vec::new();
        for pending in state.queue.drain(..) {
            if pending.generation == generation {
                events.push((pending.generation, pending.seq, pending.event));
            } else {
                // Aimed at a field that has gone. The consumer never sees
                // it, so it is a drop like any other and is counted: losing
                // a whole generation should not look like a quiet frame.
                dropped = dropped.saturating_add(1);
            }
        }
        NativeDrain {
            events,
            dropped,
            generation,
        }
    }

    /// Everything queued for the current generation, in order.
    #[cfg(test)]
    pub(crate) fn drain_for_test(&self) -> Vec<(u64, u64, NativeQueuedEvent)> {
        self.drain().events
    }

    /// Just the keys, for a test that only cares about those.
    #[cfg(test)]
    pub(crate) fn drain_keys_for_test(&self) -> Vec<NativeKeyEvent> {
        self.drain_for_test()
            .into_iter()
            .filter_map(|(_, _, event)| match event {
                NativeQueuedEvent::Key(key) => Some(key),
                _ => None,
            })
            .collect()
    }
}

/// Registration handle for the scene: ask for the keyboard, read what
/// arbitration decided.
#[derive(Resource, Clone)]
pub(crate) struct NativeKeyboard {
    bridge: NativeInputBridge,
    feed: crate::protocol::ClientSceneFeedHandle,
}

impl NativeKeyboard {
    /// A fresh owner id for one consumer. Keep it: every focus request and
    /// every [`NativeInputEvent::Focus`] is keyed by it.
    pub(crate) fn claim(&self) -> NativeOwner {
        NativeOwner::next()
    }

    /// Ask for (or give up) the keyboard. The compositor arbitrates: the
    /// session lock, an exclusive layer surface, an input-method grab, a
    /// `comp.window.focus` and xdg-activation all take priority, and
    /// [`Self::focused`] reports the outcome.
    ///
    /// The request STANDS until it is given up: being outranked does not
    /// clear it, and the compositor arbitrates again on its own when the
    /// preempting client goes. Calling this every frame is free — only a
    /// change reaches the compositor — and a release from an owner that no
    /// longer holds the request is ignored.
    pub(crate) fn request_focus(&self, owner: NativeOwner, wanted: bool) {
        // ONLY the edge reaches the protocol thread. A repeated request is
        // free, so a consumer may call this every frame, and it must not
        // arbitrate every frame: a standing request survives being outranked
        // and the compositor arbitrates again by itself when the preempting
        // client goes away (or the lock ends). A `false` from an owner that
        // holds nothing is ignored.
        if self.bridge.set_wants_focus(owner, wanted) {
            self.feed.native_focus_changed();
        }
    }

    /// Whether in-process content owns the keyboard right now.
    pub(crate) fn focused(&self) -> bool {
        self.bridge.focused()
    }

    /// Which owner holds it.
    pub(crate) fn focus_owner(&self) -> Option<NativeOwner> {
        self.bridge.focus_owner()
    }

    /// Whether the compositor took this consumer as its input owner. A
    /// second consumer is refused, and everything it does here is a no-op —
    /// check this before believing [`Self::focused`].
    pub(crate) fn installed(&self) -> bool {
        self.bridge.installed()
    }

    /// Give the compositor its input back: the bridge stops receiving, the
    /// input-method sink is unregistered, the IME session ends and the
    /// keyboard goes to whichever client should have it. Call this when the
    /// content unmounts; until then nothing else can install a bridge,
    /// because the compositor keeps a single owner.
    ///
    /// The focus edge is emitted HERE, synchronously, so a consumer that
    /// despawns in the same frame still reads its `Focus{focused:false}`;
    /// the compositor's own teardown follows on the protocol thread.
    pub(crate) fn uninstall(&self) {
        self.bridge.set_focused(false);
        self.feed.native_input_uninstall();
    }
}

/// Registration handle for the scene's text field.
#[derive(Resource, Clone)]
pub(crate) struct NativeIme {
    bridge: NativeInputBridge,
    feed: crate::protocol::ClientSceneFeedHandle,
}

impl NativeIme {
    /// Whether this field CLAIMS the input method — the last `enable`
    /// request it made. It stays true while the field is preempted, which
    /// is what makes it re-arm on the focus edge it is about to read.
    pub(crate) fn enabled(&self) -> bool {
        self.bridge.enabled()
    }

    /// Whether the compositor has actually activated an input-method
    /// instance for this field: `enabled()` is the ask, this is the answer.
    /// False while a client owns the input method, while the content does
    /// not hold the keyboard, and when no input method is connected at all.
    pub(crate) fn active(&self) -> bool {
        self.bridge.ime_active()
    }

    /// Take or give up the input method (`zwp_text_input_v3.enable` for a
    /// client).
    pub(crate) fn enable(&self, enabled: bool) {
        self.request(NativeImeRequest::Enable(enabled));
    }

    /// Report the caret rectangle in comp's global logical coordinates (the
    /// space windows are placed in; it spans every output, so a caret on the
    /// second output anchors its candidate window there).
    pub(crate) fn set_caret(&self, x: i32, y: i32, width: i32, height: i32) {
        self.request(NativeImeRequest::Caret {
            x,
            y,
            width,
            height,
        });
    }

    pub(crate) fn set_surrounding_text(&self, text: String, cursor: u32, anchor: u32) {
        self.request(NativeImeRequest::SurroundingText {
            text,
            cursor,
            anchor,
        });
    }

    pub(crate) fn set_content_type(&self, hint: u32, purpose: u32) {
        self.request(NativeImeRequest::ContentType { hint, purpose });
    }

    /// End a batch of the requests above (`commit` for a client).
    pub(crate) fn done(&self) {
        self.request(NativeImeRequest::Done);
    }

    fn request(&self, request: NativeImeRequest) {
        self.bridge.note_request(&request);
        self.feed.native_ime_request(request);
    }
}

/// Keys and IME for in-process content. The `native-input` feature installs
/// it; it does nothing until some content asks for focus.
pub(crate) struct NativeInputPlugin;

impl Plugin for NativeInputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NativeInputBridge>()
            .add_message::<NativeInput>()
            .add_systems(Startup, attach_protocol)
            // Before `InputSystems` so a consumer that mirrors these into
            // `ButtonInput` does it in the same frame Bevy's own input runs.
            .add_systems(PreUpdate, native_input_pump.before(bevy::input::InputSystems));
    }
}

/// Install the bridges for in-crate consumers. Idempotent.
pub(crate) fn install(app: &mut App) {
    if !app.is_plugin_added::<NativeInputPlugin>() {
        app.add_plugins(NativeInputPlugin);
    }
}

/// An exclusive system, so the registration handles exist for the rest of
/// Startup rather than after the schedule's command flush.
fn attach_protocol(world: &mut World) {
    // A build without the compositor feed (a bare test app) gets the
    // resources but no protocol: asking for focus is then a no-op rather
    // than a panic.
    let Some(feed) = world.get_resource::<crate::protocol::ClientSceneFeed>() else {
        warn!("native input: no compositor feed in this app; keys and IME stay idle");
        return;
    };
    let handle = feed.handle();
    let bridge = world.resource::<NativeInputBridge>().clone();
    feed.install_native_input(bridge.clone());
    world.insert_resource(NativeKeyboard {
        bridge: bridge.clone(),
        feed: handle.clone(),
    });
    world.insert_resource(NativeIme {
        bridge,
        feed: handle,
    });
}

fn native_input_pump(
    bridge: Res<NativeInputBridge>,
    windows: Query<Entity, With<Window>>,
    mut messages: MessageWriter<NativeInput>,
) {
    let drained = bridge.drain();
    if drained.dropped > 0 {
        warn!(
            dropped = drained.dropped,
            "native input events dropped before the frame"
        );
        messages.write(NativeInput {
            generation: drained.generation,
            seq: 0,
            event: NativeInputEvent::Dropped {
                events: drained.dropped,
            },
        });
    }
    if drained.events.is_empty() {
        return;
    }
    // In-process content has no window of its own; the compositor's own
    // window (nested) or the placeholder entity (kms) is the only target.
    let window = windows.iter().next().unwrap_or(Entity::PLACEHOLDER);
    for (generation, seq, event) in drained.events {
        let event = match event {
            NativeQueuedEvent::Key(key) => NativeInputEvent::Key {
                input: keyboard_input(&key, window),
                modifiers: key.modifiers,
            },
            NativeQueuedEvent::Ime(event) => NativeInputEvent::Ime(event),
            NativeQueuedEvent::Focus { owner, focused } => NativeInputEvent::Focus { owner, focused },
        };
        messages.write(NativeInput {
            generation,
            seq,
            event,
        });
    }
}

/// One seat key as Bevy sees it.
pub(crate) fn keyboard_input(event: &NativeKeyEvent, window: Entity) -> KeyboardInput {
    KeyboardInput {
        key_code: key_code(event.evdev),
        logical_key: logical_key(event.keysym, event.text.as_deref()),
        state: if event.pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        text: event
            .text
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(Into::into),
        repeat: event.repeat,
        window,
    }
}

/// The logical key: the text it produced, else the named key, else the
/// keysym so a binding UI can still identify it.
fn logical_key(keysym: u32, text: Option<&str>) -> Key {
    if let Some(named) = named_key(keysym) {
        return named;
    }
    match text.filter(|text| !text.is_empty() && !text.chars().any(char::is_control)) {
        Some(text) => Key::Character(text.into()),
        None => Key::Unidentified(NativeKey::Xkb(keysym)),
    }
}

fn named_key(keysym: u32) -> Option<Key> {
    use xkb::keysyms as sym;
    Some(match keysym {
        sym::KEY_Return | sym::KEY_KP_Enter => Key::Enter,
        sym::KEY_BackSpace => Key::Backspace,
        sym::KEY_Tab | sym::KEY_ISO_Left_Tab => Key::Tab,
        sym::KEY_Escape => Key::Escape,
        sym::KEY_Delete | sym::KEY_KP_Delete => Key::Delete,
        sym::KEY_Insert => Key::Insert,
        sym::KEY_Home | sym::KEY_KP_Home => Key::Home,
        sym::KEY_End | sym::KEY_KP_End => Key::End,
        sym::KEY_Page_Up | sym::KEY_KP_Page_Up => Key::PageUp,
        sym::KEY_Page_Down | sym::KEY_KP_Page_Down => Key::PageDown,
        sym::KEY_Left | sym::KEY_KP_Left => Key::ArrowLeft,
        sym::KEY_Right | sym::KEY_KP_Right => Key::ArrowRight,
        sym::KEY_Up | sym::KEY_KP_Up => Key::ArrowUp,
        sym::KEY_Down | sym::KEY_KP_Down => Key::ArrowDown,
        sym::KEY_Shift_L | sym::KEY_Shift_R => Key::Shift,
        sym::KEY_Control_L | sym::KEY_Control_R => Key::Control,
        sym::KEY_Alt_L | sym::KEY_Alt_R => Key::Alt,
        sym::KEY_ISO_Level3_Shift => Key::AltGraph,
        sym::KEY_Super_L | sym::KEY_Super_R => Key::Super,
        sym::KEY_Caps_Lock => Key::CapsLock,
        sym::KEY_Num_Lock => Key::NumLock,
        sym::KEY_F1 => Key::F1,
        sym::KEY_F2 => Key::F2,
        sym::KEY_F3 => Key::F3,
        sym::KEY_F4 => Key::F4,
        sym::KEY_F5 => Key::F5,
        sym::KEY_F6 => Key::F6,
        sym::KEY_F7 => Key::F7,
        sym::KEY_F8 => Key::F8,
        sym::KEY_F9 => Key::F9,
        sym::KEY_F10 => Key::F10,
        sym::KEY_F11 => Key::F11,
        sym::KEY_F12 => Key::F12,
        _ => return None,
    })
}

/// The physical key. This is the inverse of the nested backend's
/// `evdev_keycode` — in BOTH directions, which
/// `native_key_codes_are_the_inverse_of_evdev_keycode` holds it to: a key
/// named there and missing here would arrive at the content as an
/// unidentified XKB code.
pub(crate) fn key_code(evdev: u32) -> KeyCode {
    use KeyCode::*;
    match evdev {
        1 => Escape,
        2 => Digit1,
        3 => Digit2,
        4 => Digit3,
        5 => Digit4,
        6 => Digit5,
        7 => Digit6,
        8 => Digit7,
        9 => Digit8,
        10 => Digit9,
        11 => Digit0,
        12 => Minus,
        13 => Equal,
        14 => Backspace,
        15 => Tab,
        16 => KeyQ,
        17 => KeyW,
        18 => KeyE,
        19 => KeyR,
        20 => KeyT,
        21 => KeyY,
        22 => KeyU,
        23 => KeyI,
        24 => KeyO,
        25 => KeyP,
        26 => BracketLeft,
        27 => BracketRight,
        28 => Enter,
        29 => ControlLeft,
        30 => KeyA,
        31 => KeyS,
        32 => KeyD,
        33 => KeyF,
        34 => KeyG,
        35 => KeyH,
        36 => KeyJ,
        37 => KeyK,
        38 => KeyL,
        39 => Semicolon,
        40 => Quote,
        41 => Backquote,
        42 => ShiftLeft,
        43 => Backslash,
        44 => KeyZ,
        45 => KeyX,
        46 => KeyC,
        47 => KeyV,
        48 => KeyB,
        49 => KeyN,
        50 => KeyM,
        51 => Comma,
        52 => Period,
        53 => Slash,
        54 => ShiftRight,
        55 => NumpadMultiply,
        56 => AltLeft,
        57 => Space,
        58 => CapsLock,
        59 => F1,
        60 => F2,
        61 => F3,
        62 => F4,
        63 => F5,
        64 => F6,
        65 => F7,
        66 => F8,
        67 => F9,
        68 => F10,
        69 => NumLock,
        70 => ScrollLock,
        71 => Numpad7,
        72 => Numpad8,
        73 => Numpad9,
        74 => NumpadSubtract,
        75 => Numpad4,
        76 => Numpad5,
        77 => Numpad6,
        78 => NumpadAdd,
        79 => Numpad1,
        80 => Numpad2,
        81 => Numpad3,
        82 => Numpad0,
        83 => NumpadDecimal,
        // The ISO key next to the left shift, present on every European
        // layout.
        86 => IntlBackslash,
        87 => F11,
        88 => F12,
        // The Japanese keys: a JIS keyboard sends these constantly and an
        // IME is exactly what is listening for them.
        92 => Convert,
        93 => KanaMode,
        94 => NonConvert,
        96 => NumpadEnter,
        97 => ControlRight,
        98 => NumpadDivide,
        99 => PrintScreen,
        100 => AltRight,
        102 => Home,
        103 => ArrowUp,
        104 => PageUp,
        105 => ArrowLeft,
        106 => ArrowRight,
        107 => End,
        108 => ArrowDown,
        109 => PageDown,
        110 => Insert,
        111 => Delete,
        113 => AudioVolumeMute,
        114 => AudioVolumeDown,
        115 => AudioVolumeUp,
        116 => Power,
        117 => NumpadEqual,
        119 => Pause,
        125 => SuperLeft,
        126 => SuperRight,
        127 => ContextMenu,
        // Everything else keeps its XKB keycode, which is what the nested
        // backend's mapping accepts back.
        other => Unidentified(NativeKeyCode::Xkb(other + 8)),
    }
}

/// A test-only text field drawn by the compositor itself.
///
/// It is the gate client for this module: no Wayland client can prove that
/// in-process content receives keys and IME, because a client has both by
/// definition. `COSMIX_COMP_NATIVE_INPUT_PROBE=1` installs it; it takes the
/// keyboard, enables the input method, reports a caret, and prints one
/// `NATIVE_INPUT_PROBE` line whenever its state changes.
#[derive(Resource)]
struct NativeProbeField {
    owner: NativeOwner,
    text: String,
    preedit: String,
    focused: bool,
    commits: u32,
    dropped: u64,
}

/// Install the bridges, and the probe field when its variable is set.
pub(crate) fn install_from_environment(app: &mut App) {
    install(app);
    if std::env::var("COSMIX_COMP_NATIVE_INPUT_PROBE").as_deref() != Ok("1") {
        return;
    }
    app.add_systems(Startup, probe_take_focus.after(attach_protocol))
        .add_systems(Update, probe_edit.after(native_input_pump));
}

fn probe_take_focus(mut commands: Commands, keyboard: Res<NativeKeyboard>, ime: Res<NativeIme>) {
    let owner = keyboard.claim();
    keyboard.request_focus(owner, true);
    ime.enable(true);
    // A caret an input method can place its candidate window under.
    ime.set_caret(40, 80, 2, 18);
    ime.done();
    commands.insert_resource(NativeProbeField {
        owner,
        text: String::new(),
        preedit: String::new(),
        focused: false,
        commits: 0,
        dropped: 0,
    });
    info!(
        owner = owner.id(),
        "NATIVE_INPUT_PROBE requested focus and enabled the input method"
    );
}

fn probe_edit(
    field: Option<ResMut<NativeProbeField>>,
    keyboard: Res<NativeKeyboard>,
    ime_field: Res<NativeIme>,
    mut input: MessageReader<NativeInput>,
) {
    let Some(mut field) = field else {
        return;
    };
    let mut changed = false;
    for message in input.read() {
        match &message.event {
            NativeInputEvent::Focus { owner, focused } => {
                // The message says WHEN the edge happened; the resource says
                // whether this owner is the one holding the keyboard now.
                if *owner == Some(field.owner) || !*focused {
                    field.focused = *focused && keyboard.focused();
                    // A real field releases its own held keys here: the
                    // seat's releases go to whoever has the keyboard now.
                    changed = true;
                }
            }
            NativeInputEvent::Dropped { events } => {
                field.dropped += events;
                changed = true;
            }
            NativeInputEvent::Key { input: key, .. } => {
                if key.state != ButtonState::Pressed {
                    continue;
                }
                match &key.logical_key {
                    // The probe's stand-in for unmounting: give the
                    // compositor its input back. A scene does this when it
                    // goes away; until it does, nothing else can install.
                    Key::Escape => {
                        keyboard.uninstall();
                        info!("NATIVE_INPUT_PROBE uninstalled");
                        changed = true;
                    }
                    Key::Backspace => {
                        field.text.pop();
                        changed = true;
                    }
                    Key::Character(text) => {
                        let text = text.to_string();
                        field.text.push_str(&text);
                        changed = true;
                    }
                    _ => {}
                }
            }
            NativeInputEvent::Ime(NativeImeEvent::Commit(text)) => {
                let text = text.clone();
                field.text.push_str(&text);
                field.commits += 1;
                changed = true;
            }
            NativeInputEvent::Ime(NativeImeEvent::Preedit { text, .. }) => {
                field.preedit.clone_from(text);
                changed = true;
            }
            NativeInputEvent::Ime(NativeImeEvent::DeleteSurrounding { before, .. }) => {
                let keep = field.text.len().saturating_sub(*before as usize);
                field.text.truncate(keep);
                changed = true;
            }
            NativeInputEvent::Ime(NativeImeEvent::Done { discard }) => {
                if *discard {
                    field.preedit.clear();
                }
                changed = true;
            }
        }
    }
    if changed {
        // What a real field reports back to the input method after every
        // edit: the text around the cursor, what kind of field it is, and
        // where the caret now sits.
        // Re-enable on every edit: an input method that connects after the
        // field did would otherwise never hear the activate. `enable(true)`
        // when it is already enabled is not a generation edge, so this does
        // not disturb a composition in flight.
        ime_field.enable(true);
        let cursor = field.text.len() as u32;
        ime_field.set_surrounding_text(field.text.clone(), cursor, cursor);
        ime_field.set_content_type(0, 0);
        ime_field.set_caret(40, 80, 2, 18);
        ime_field.done();
        info!(
            "NATIVE_INPUT_PROBE focused={} owner_holds={} installed={} text={:?} preedit={:?} \
             commits={} dropped={} ime_enabled={} ime_active={}",
            field.focused,
            keyboard.focus_owner() == Some(field.owner),
            keyboard.installed(),
            field.text,
            field.preedit,
            field.commits,
            field.dropped,
            ime_field.enabled(),
            ime_field.active(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::reflect::{
        FromReflect, TypeInfo, Typed,
        enums::{DynamicEnum, DynamicVariant, VariantInfo},
    };

    fn key(evdev: u32) -> NativeKeyEvent {
        NativeKeyEvent {
            evdev,
            keysym: xkb::keysyms::KEY_a,
            text: Some("a".into()),
            pressed: true,
            repeat: false,
            modifiers: NativeModifiers::default(),
        }
    }

    fn focused_bridge() -> NativeInputBridge {
        let bridge = NativeInputBridge::default();
        bridge.set_wants_focus(NativeOwner::next(), true);
        bridge.set_focused(true);
        bridge.note_request(&NativeImeRequest::Enable(true));
        // Drain the focus edge so each test starts from an empty queue.
        let _ = bridge.drain_for_test();
        bridge
    }

    /// Every `KeyCode` the seat path can name, so the two tables cannot
    /// drift apart in either direction.
    fn all_key_codes() -> Vec<KeyCode> {
        let TypeInfo::Enum(info) = KeyCode::type_info() else {
            panic!("KeyCode is an enum");
        };
        info.iter()
            .filter(|variant| matches!(variant, VariantInfo::Unit(_)))
            .filter_map(|variant| {
                KeyCode::from_reflect(&DynamicEnum::new(variant.name(), DynamicVariant::Unit))
            })
            .collect()
    }

    /// The physical mapping is the inverse of the nested backend's, so a
    /// key injected through the seat is the key the scene sees — and a key
    /// the backend can produce is never delivered as "unidentified".
    #[test]
    fn native_key_codes_are_the_inverse_of_evdev_keycode() {
        for evdev in 1..=127_u32 {
            let code = key_code(evdev);
            assert_eq!(
                crate::evdev_keycode(code),
                Some(evdev),
                "evdev {evdev} maps to {code:?}, which maps back elsewhere"
            );
        }
        let codes = all_key_codes();
        assert!(codes.len() > 100, "reflection found {} key codes", codes.len());
        for code in codes {
            let Some(evdev) = crate::evdev_keycode(code) else {
                continue;
            };
            assert_eq!(
                key_code(evdev),
                code,
                "evdev_keycode names {code:?} as {evdev}, key_code does not name it back"
            );
        }
    }

    #[test]
    fn logical_keys_prefer_named_then_text() {
        assert_eq!(logical_key(xkb::keysyms::KEY_Return, Some("\r")), Key::Enter);
        assert_eq!(
            logical_key(xkb::keysyms::KEY_a, Some("a")),
            Key::Character("a".into())
        );
        assert_eq!(
            logical_key(xkb::keysyms::KEY_XF86AudioPlay, None),
            Key::Unidentified(NativeKey::Xkb(xkb::keysyms::KEY_XF86AudioPlay))
        );
        // A control character is not text.
        assert_eq!(
            logical_key(0x1234_5678, Some("\u{1}")),
            Key::Unidentified(NativeKey::Xkb(0x1234_5678))
        );
    }

    #[test]
    fn keys_are_only_kept_while_focused_and_the_edge_is_announced() {
        let bridge = NativeInputBridge::default();
        let owner = NativeOwner::next();
        bridge.push_key(key(30));
        assert!(
            bridge.drain_for_test().is_empty(),
            "unfocused content hears nothing"
        );

        bridge.set_wants_focus(owner, true);
        bridge.set_focused(true);
        bridge.push_key(key(30));
        let drained = bridge.drain_for_test();
        assert_eq!(
            drained[0].2,
            NativeQueuedEvent::Focus {
                owner: Some(owner),
                focused: true
            }
        );
        assert_eq!(drained[1].2, NativeQueuedEvent::Key(key(30)));

        // A4: the falling edge is delivered, and what was never drained
        // dies with the generation it belonged to.
        bridge.push_key(key(31));
        bridge.set_focused(false);
        let drained = bridge.drain_for_test();
        assert_eq!(
            drained.iter().map(|(_, _, event)| event.clone()).collect::<Vec<_>>(),
            vec![NativeQueuedEvent::Focus {
                owner: None,
                focused: false
            }],
            "the stale key is discarded, the edge is not"
        );
    }

    /// Only the owner that holds the request can give it back.
    #[test]
    fn focus_requests_are_owner_keyed() {
        let bridge = NativeInputBridge::default();
        let first = NativeOwner::next();
        let second = NativeOwner::next();
        assert!(bridge.set_wants_focus(first, true));
        assert!(!bridge.set_wants_focus(first, true), "no change, no edge");
        assert!(bridge.set_wants_focus(second, true), "last writer wins");
        assert!(
            !bridge.set_wants_focus(first, false),
            "a displaced owner cannot drop the new owner's request"
        );
        assert!(bridge.wants_focus());
        bridge.set_focused(true);
        assert_eq!(bridge.focus_owner(), Some(second));
        // NO POLL: only a change reaches the protocol thread, so content
        // may ask every frame — including while it is outranked, which is
        // the state a per-frame arbitration loop would be born in.
        bridge.set_focused(false);
        assert!(!bridge.focused());
        assert!(
            !bridge.set_wants_focus(second, true),
            "a standing request must not re-arbitrate"
        );

        assert!(bridge.set_wants_focus(second, false));
        assert!(!bridge.wants_focus());
    }

    /// Two owners, one keyboard. Handing over while the COMPOSITOR keeps the
    /// keyboard is still an edge: without it the displaced owner would go on
    /// consuming the new owner's keys.
    #[test]
    fn an_owner_handover_is_announced_to_both() {
        let bridge = NativeInputBridge::default();
        let first = NativeOwner::next();
        let second = NativeOwner::next();
        bridge.set_wants_focus(first, true);
        bridge.set_focused(true);
        assert_eq!(bridge.focus_owner(), Some(first));
        let _ = bridge.drain_for_test();
        bridge.push_key(key(30));

        // The second owner takes the standing request; arbitration still
        // says "in-process content", so `focused` does not change.
        bridge.set_wants_focus(second, true);
        bridge.set_focused(true);
        assert_eq!(bridge.focus_owner(), Some(second));
        let drained: Vec<_> = bridge
            .drain_for_test()
            .into_iter()
            .map(|(_, _, event)| event)
            .collect();
        assert_eq!(
            drained,
            vec![
                NativeQueuedEvent::Focus {
                    owner: Some(first),
                    focused: false,
                },
                NativeQueuedEvent::Focus {
                    owner: Some(second),
                    focused: true,
                },
            ],
            "the first owner's key must die with its generation and both owners hear the handover"
        );
    }

    /// A generation edge voids the batch in flight for good: the input
    /// method finishes a composition aimed at a field that has gone, and
    /// none of it may land in the next one.
    #[test]
    fn a_voided_batch_never_leaks_into_the_next_field() {
        let bridge = focused_bridge();
        bridge.push_ime(NativeImeEvent::Preedit {
            text: "compo".into(),
            cursor_begin: 0,
            cursor_end: 5,
        });
        // The keyboard goes to a client and comes back.
        bridge.set_focused(false);
        bridge.set_focused(true);
        let _ = bridge.drain_for_test();

        // The rest of that batch arrives after the edge.
        bridge.push_ime(NativeImeEvent::Commit("sition".into()));
        bridge.push_ime(NativeImeEvent::Done { discard: false });
        assert!(
            bridge.drain_for_test().is_empty(),
            "the tail of a voided batch reached the new field"
        );

        // The batch AFTER it is ordinary traffic again.
        bridge.push_ime(NativeImeEvent::Commit("fresh".into()));
        bridge.push_ime(NativeImeEvent::Done { discard: false });
        assert_eq!(bridge.drain_for_test().len(), 2);
    }

    /// Keys and IME share one clock, so the order the compositor produced
    /// is the order the content reads — both ways round.
    #[test]
    fn keys_and_ime_keep_their_arrival_order() {
        let bridge = focused_bridge();
        bridge.push_key(key(30));
        bridge.push_ime(NativeImeEvent::Commit("x".into()));
        bridge.push_ime(NativeImeEvent::Done { discard: false });
        bridge.push_key(key(31));
        let drained = bridge.drain_for_test();
        let seqs: Vec<u64> = drained.iter().map(|(_, seq, _)| *seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "seq is monotonic across both kinds");
        assert!(matches!(drained[0].2, NativeQueuedEvent::Key(_)));
        assert!(matches!(drained[1].2, NativeQueuedEvent::Ime(NativeImeEvent::Commit(_))));
        assert!(matches!(
            drained[2].2,
            NativeQueuedEvent::Ime(NativeImeEvent::Done { .. })
        ));
        assert!(matches!(drained[3].2, NativeQueuedEvent::Key(_)));

        // IME first, then a key: same guarantee.
        bridge.push_ime(NativeImeEvent::Preedit {
            text: "y".into(),
            cursor_begin: 0,
            cursor_end: 1,
        });
        bridge.push_key(key(32));
        let drained = bridge.drain_for_test();
        assert!(matches!(
            drained[0].2,
            NativeQueuedEvent::Ime(NativeImeEvent::Preedit { .. })
        ));
        assert!(matches!(drained[1].2, NativeQueuedEvent::Key(_)));
    }

    /// A field that never enabled the input method is not the input
    /// method's field, whatever the sink hands over.
    #[test]
    fn a_disabled_field_receives_no_ime() {
        let bridge = NativeInputBridge::default();
        bridge.set_wants_focus(NativeOwner::next(), true);
        bridge.set_focused(true);
        let _ = bridge.drain_for_test();
        bridge.push_ime(NativeImeEvent::Commit("x".into()));
        assert!(bridge.drain_for_test().is_empty());
        bridge.note_request(&NativeImeRequest::Enable(true));
        bridge.push_ime(NativeImeEvent::Commit("x".into()));
        assert_eq!(bridge.drain_for_test().len(), 1);
    }

    /// A focus edge in the middle of a composition discards the WHOLE
    /// batch, so the next field never receives half of one.
    #[test]
    fn a_focus_edge_discards_the_batch_it_split() {
        let bridge = focused_bridge();
        bridge.push_ime(NativeImeEvent::Preedit {
            text: "composing".into(),
            cursor_begin: 0,
            cursor_end: 9,
        });
        // The keyboard goes to a client and comes back: two edges, and the
        // input method finishes the batch it had started.
        bridge.set_focused(false);
        bridge.set_focused(true);
        bridge.push_ime(NativeImeEvent::Done { discard: false });
        let drained = bridge.drain_for_test();
        assert!(
            drained
                .iter()
                .all(|(_, _, event)| matches!(event, NativeQueuedEvent::Focus { .. })),
            "only the focus edges survive: {drained:?}"
        );
    }

    /// An enable transition is a generation edge too: what the input method
    /// says next belongs to the field that just enabled.
    #[test]
    fn an_enable_transition_fences_the_queue() {
        let bridge = focused_bridge();
        bridge.push_ime(NativeImeEvent::Commit("old".into()));
        bridge.note_request(&NativeImeRequest::Enable(false));
        bridge.note_request(&NativeImeRequest::Enable(true));
        // The batch the edge voided stays closed until its own `done`
        // arrives — anything before that still belongs to the old field.
        bridge.push_ime(NativeImeEvent::Commit("still old".into()));
        bridge.push_ime(NativeImeEvent::Done { discard: false });
        bridge.push_ime(NativeImeEvent::Commit("new".into()));
        let drained = bridge.drain_for_test();
        assert_eq!(
            drained
                .iter()
                .map(|(_, _, event)| event.clone())
                .collect::<Vec<_>>(),
            vec![NativeQueuedEvent::Ime(NativeImeEvent::Commit("new".into()))]
        );
        // Re-enabling an already-enabled field is not an edge.
        let generation = bridge.state().generation;
        bridge.note_request(&NativeImeRequest::Enable(true));
        assert_eq!(bridge.state().generation, generation);
    }

    /// Overflow drops the newest whole batch and says so; it never delivers
    /// a preedit without its done.
    #[test]
    fn overflow_never_splits_a_batch() {
        let bridge = focused_bridge();
        for _ in 0..MAX_PENDING_EVENTS - 1 {
            bridge.push_key(key(30));
        }
        // One slot left, and a three-event batch to put in it.
        bridge.push_ime(NativeImeEvent::Preedit {
            text: "a".into(),
            cursor_begin: 0,
            cursor_end: 1,
        });
        bridge.push_ime(NativeImeEvent::Commit("a".into()));
        bridge.push_ime(NativeImeEvent::Done { discard: false });
        let drained = bridge.drain().events;
        assert!(
            drained
                .iter()
                .all(|(_, _, event)| matches!(event, NativeQueuedEvent::Key(_))),
            "the split batch is gone entirely"
        );
        // The head made it into the queue and was pulled back out again,
        // which is what the drop count reports.
        assert_eq!(drained.len(), MAX_PENDING_EVENTS - 1);

        // A key overflowing drops only itself.
        let bridge = focused_bridge();
        for _ in 0..MAX_PENDING_EVENTS + 5 {
            bridge.push_key(key(30));
        }
        let drain = bridge.drain();
        assert_eq!(drain.events.len(), MAX_PENDING_EVENTS);
        assert_eq!(drain.dropped, 5);
        assert_eq!(bridge.drain().dropped, 0, "the count is taken once");
    }

    #[test]
    fn ime_requests_track_the_caret_and_enable_state() {
        let bridge = NativeInputBridge::default();
        assert!(!bridge.enabled());
        assert_eq!(bridge.caret(), None);
        bridge.note_request(&NativeImeRequest::Enable(true));
        bridge.note_request(&NativeImeRequest::Caret {
            x: 10,
            y: 20,
            width: 2,
            height: 16,
        });
        assert!(bridge.enabled());
        assert_eq!(bridge.caret(), Some((10, 20, 2, 16)));
        bridge.note_request(&NativeImeRequest::Enable(false));
        assert!(!bridge.enabled());
        assert_eq!(bridge.caret(), None, "a disabled field has no caret");
    }
}
