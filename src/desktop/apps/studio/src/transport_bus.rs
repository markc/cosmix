//! The Bus `app.transport.*` verbs: start / stop / pause / state.
//!
//! Transport control is session-agnostic (song, stems, benchmark) and benign
//! (no filesystem access), so unlike `app.song.load` it needs no path policy.
//! Each control verb writes an `ActionRequest { .., source: Bus }` onto studio's
//! broker spine — the SAME command path the keyboard's Space uses — so remote
//! and local transport control converge on one desired-state reducer
//! (`apply_audio_intents`). `state` is a read of the CTK transport projection.
//!
//! Deliberately NOT gated by `ModalCapture`: the action pipeline
//! (`ActionProduce/Route/Apply`) carries no modal run-condition, so a remote
//! peer can drive transport even while a LOCAL modal (file dialog) owns the
//! keyboard. Remote control should not block on a local operator's UI state.

use bevy::prelude::*;
use cosmix_actions::{studio as ids, ActionId};
use ctk::prelude::{
    transport_is_playing, transport_length_secs, ActionRequest, AppPortReply, AppPortRequest,
    MusicdMixerState, Source, TransportPosition, TransportState,
};
use serde_json::json;

pub const TRANSPORT_START_VERB: &str = "app.transport.start";
pub const TRANSPORT_STOP_VERB: &str = "app.transport.stop";
pub const TRANSPORT_PAUSE_VERB: &str = "app.transport.pause";
pub const TRANSPORT_STATE_VERB: &str = "app.transport.state";

/// A control verb's ack: `rc=0` means the command was accepted onto the spine,
/// not that the transport has already changed (that lands next stage/frame).
fn accepted(action: &str) -> AppPortReply {
    (0, json!({ "accepted": true, "action": action }).to_string())
}

fn emit(actions: &mut MessageWriter<ActionRequest>, action: ActionId) {
    actions.write(ActionRequest {
        action,
        source: Source::Bus,
        args: Default::default(),
        invocation_focus: None,
    });
}

pub fn transport_start_verb(
    In(_request): In<AppPortRequest>,
    mut actions: MessageWriter<ActionRequest>,
) -> AppPortReply {
    emit(&mut actions, ids::TRANSPORT_START);
    accepted("start")
}

pub fn transport_stop_verb(
    In(_request): In<AppPortRequest>,
    mut actions: MessageWriter<ActionRequest>,
) -> AppPortReply {
    emit(&mut actions, ids::TRANSPORT_STOP);
    accepted("stop")
}

pub fn transport_pause_verb(
    In(_request): In<AppPortRequest>,
    mut actions: MessageWriter<ActionRequest>,
) -> AppPortReply {
    emit(&mut actions, ids::TRANSPORT_PAUSE);
    accepted("pause")
}

/// Read the effective transport state: `playing` is CTK's desired-state
/// projection (`null` when the mixer is not connected/ready), and
/// `position_seconds` is the live extrapolated playhead.
///
/// `playing` is the provisional-aware desired state (so a just-issued start
/// reads back `true` immediately), while `position_seconds` extrapolates from
/// the ACKNOWLEDGED clock — so in the brief pre-ack window a start reports
/// `playing:true` with a not-yet-advancing position. A snapshot read, not a
/// transaction.
pub fn transport_state_verb(
    In(_request): In<AppPortRequest>,
    transport: TransportState,
    position: Res<TransportPosition>,
    state: Res<MusicdMixerState>,
) -> AppPortReply {
    let playing = transport.desired_playing();
    let length = transport_length_secs(&state) as f64;
    let position_seconds = position.live_seconds(transport_is_playing(&state), length);
    (
        0,
        json!({
            "playing": playing,
            "position_seconds": position_seconds,
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::prelude::InboundRequest;
    use std::collections::BTreeMap;

    fn request() -> AppPortRequest {
        AppPortRequest {
            request: InboundRequest {
                connection_generation: 1,
                from: "peer".into(),
                command: TRANSPORT_START_VERB.into(),
                body: String::new(),
                headers: BTreeMap::new(),
                reply_id: Some("1".into()),
            },
            app_name: "studio-bevy-1".into(),
        }
    }

    fn run<M>(
        system: impl IntoSystem<In<AppPortRequest>, AppPortReply, M> + 'static,
    ) -> (u8, Vec<ActionRequest>) {
        let mut app = App::new();
        app.add_message::<ActionRequest>();
        let id = app.world_mut().register_system(system);
        let (rc, _body) = app.world_mut().run_system_with(id, request()).unwrap();
        let emitted = app
            .world_mut()
            .resource_mut::<Messages<ActionRequest>>()
            .drain()
            .collect();
        (rc, emitted)
    }

    #[test]
    fn start_stop_pause_emit_the_right_bus_sourced_actions() {
        let (rc, out) = run(transport_start_verb);
        assert_eq!(rc, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].action, ids::TRANSPORT_START);
        assert_eq!(out[0].source, Source::Bus);

        let (_, out) = run(transport_pause_verb);
        assert_eq!(out[0].action, ids::TRANSPORT_PAUSE);

        let (_, out) = run(transport_stop_verb);
        assert_eq!(out[0].action, ids::TRANSPORT_STOP);
    }

    #[test]
    fn state_verb_reports_null_playing_and_zero_position_when_not_ready() {
        // A default MusicdMixerState is Connecting/not-ready → the projection
        // yields None (json null); position has no base yet → 0. (The playing
        // path is validated live end-to-end.)
        let mut app = App::new();
        app.init_resource::<MusicdMixerState>();
        app.init_resource::<TransportPosition>();
        let id = app.world_mut().register_system(transport_state_verb);
        let (rc, body) = app.world_mut().run_system_with(id, request()).unwrap();
        assert_eq!(rc, 0);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(value["playing"].is_null(), "not-ready → playing null");
        assert_eq!(value["position_seconds"], 0.0);
    }
}
