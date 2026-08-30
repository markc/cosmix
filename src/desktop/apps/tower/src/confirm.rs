//! Correlated CTK confirmation dialogs for Tower mutations.

use std::collections::HashMap;

use bevy::app::{App, Plugin, Update};
use bevy::ecs::message::{Message, MessageReader, MessageWriter};
use bevy::prelude::{IntoScheduleConfigs, ResMut, Resource};
use ctk::prelude::{
    ActionRole, InteractionAction, InteractionId, InteractionOutcome, InteractionRequest,
    InteractionResult, InteractionSeverity,
};

use crate::inspector::{InspectorMutation, InspectorMutationRequest};
use crate::lifecycle::{LifecycleCommand, LifecycleVerb};
use crate::panes::ascii_ui_text;

#[derive(Message, Clone, Debug)]
pub(crate) enum ConfirmIntent {
    Inspector(InspectorMutation),
    Lifecycle(LifecycleCommand),
}

#[derive(Resource, Default)]
struct PendingConfirmations(HashMap<InteractionId, ConfirmIntent>);

pub(crate) struct ConfirmationPlugin;

impl Plugin for ConfirmationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingConfirmations>()
            .add_message::<ConfirmIntent>()
            .add_message::<InspectorMutationRequest>()
            .add_systems(Update, (raise_confirmations, resolve_confirmations).chain());
    }
}

fn raise_confirmations(
    mut intents: MessageReader<ConfirmIntent>,
    mut pending: ResMut<PendingConfirmations>,
    mut interactions: MessageWriter<InteractionRequest>,
) {
    for intent in intents.read() {
        let (title, message, severity) = match intent {
            ConfirmIntent::Inspector(mutation) => {
                let (title, message) = mutation.confirmation();
                (title, message, InteractionSeverity::Warning)
            }
            ConfirmIntent::Lifecycle(command) => (
                format!("{} {}?", command.verb.as_str(), command.unit),
                format!(
                    "Run systemctl {} on node {} via SSH.",
                    command.verb.as_str(),
                    command.node
                ),
                InteractionSeverity::Danger,
            ),
        };
        let request = InteractionRequest::confirm(ascii_ui_text(&title), ascii_ui_text(&message))
            .severity(severity)
            .action(InteractionAction::new("cancel", "Cancel", ActionRole::Cancel).default())
            .action(InteractionAction::new(
                "confirm",
                "Confirm",
                ActionRole::Destructive,
            ));
        pending.0.insert(request.id(), intent.clone());
        interactions.write(request);
    }
}

fn resolve_confirmations(
    mut results: MessageReader<InteractionResult>,
    mut pending: ResMut<PendingConfirmations>,
    mut inspector: MessageWriter<InspectorMutationRequest>,
    mut lifecycle: MessageWriter<LifecycleCommand>,
) {
    for result in results.read() {
        let Some(intent) = pending.0.remove(&result.id) else {
            continue;
        };
        let confirmed = matches!(
            &result.outcome,
            InteractionOutcome::Action(action) if action == "confirm"
        );
        if !confirmed {
            continue;
        }
        match intent {
            ConfirmIntent::Inspector(mutation) => {
                inspector.write(InspectorMutationRequest(mutation));
            }
            ConfirmIntent::Lifecycle(command) => {
                // Start is intentionally direct in the node pane; only
                // stop/restart arrive through this destructive dialog path.
                debug_assert!(command.verb != LifecycleVerb::Start);
                lifecycle.write(command);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::ecs::message::Messages;

    use super::*;

    fn mutation() -> InspectorMutation {
        InspectorMutation::InvokeAction {
            service: "studio-bevy-4242".into(),
            action: "transport.toggle".into(),
            identity: crate::inspector::ProcessIdentity {
                service: "studio-bevy-4242".into(),
                pid: Some(4242),
                started_at: Some("2026-07-24T00:00:00Z".into()),
            },
        }
    }

    #[test]
    fn inspector_mutation_only_dispatches_after_explicit_confirmation() {
        let mut app = App::new();
        app.add_message::<InteractionRequest>()
            .add_message::<InteractionResult>()
            .add_message::<LifecycleCommand>()
            .add_plugins(ConfirmationPlugin);

        app.world_mut()
            .write_message(ConfirmIntent::Inspector(mutation()));
        app.update();
        let request = app
            .world_mut()
            .resource_mut::<Messages<InteractionRequest>>()
            .drain()
            .next()
            .unwrap();
        assert!(app
            .world_mut()
            .resource_mut::<Messages<InspectorMutationRequest>>()
            .drain()
            .next()
            .is_none());

        app.world_mut().write_message(InteractionResult {
            id: request.id(),
            outcome: InteractionOutcome::Action("cancel".into()),
        });
        app.update();
        assert!(app
            .world_mut()
            .resource_mut::<Messages<InspectorMutationRequest>>()
            .drain()
            .next()
            .is_none());

        app.world_mut()
            .write_message(ConfirmIntent::Inspector(mutation()));
        app.update();
        let request = app
            .world_mut()
            .resource_mut::<Messages<InteractionRequest>>()
            .drain()
            .next()
            .unwrap();
        app.world_mut().write_message(InteractionResult {
            id: request.id(),
            outcome: InteractionOutcome::Action("confirm".into()),
        });
        app.update();
        let dispatched = app
            .world_mut()
            .resource_mut::<Messages<InspectorMutationRequest>>()
            .drain()
            .next()
            .unwrap();
        assert_eq!(dispatched.0, mutation());
    }
}
