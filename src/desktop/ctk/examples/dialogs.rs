//! Interactive gallery for every in-process CTK dialog kind.

use bevy::ecs::observer::On;
use bevy::feathers::{dark_theme::create_dark_theme, theme::UiTheme};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ActivateOnPress, Button};
use ctk::prelude::*;
use ctk::theme::tokens;

#[derive(Component, Clone, Copy)]
enum DialogDemo {
    Message,
    Confirm,
    Prompt,
    SecretPrompt,
    Choice,
    MultiChoice,
    Progress,
    Slider,
    TextView,
}

#[derive(Resource, Default)]
struct ProgressDemo {
    active: Option<ActiveProgressDemo>,
}

struct ActiveProgressDemo {
    id: InteractionId,
    current: u64,
    timer: Timer,
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CTK dialog gallery".into(),
                resolution: (760, 420).into(),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            ctk::interaction::InteractionPlugin,
        ))
        .init_resource::<ProgressDemo>()
        .add_observer(open_demo)
        .add_systems(Startup, setup)
        .add_systems(Update, (advance_progress, report_results))
        .run();
}

fn setup(mut commands: Commands, mut theme: ResMut<UiTheme>, mut theme_state: ResMut<ThemeState>) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let message = demo_button(&mut commands, "Message", DialogDemo::Message);
    let confirm = demo_button(&mut commands, "Confirm", DialogDemo::Confirm);
    let prompt = demo_button(&mut commands, "Prompt", DialogDemo::Prompt);
    let secret = demo_button(&mut commands, "Secret prompt", DialogDemo::SecretPrompt);
    let choice = demo_button(&mut commands, "Choice", DialogDemo::Choice);
    let multi = demo_button(&mut commands, "Multi-choice", DialogDemo::MultiChoice);
    let progress = demo_button(&mut commands, "Progress", DialogDemo::Progress);
    let slider = demo_button(&mut commands, "Slider", DialogDemo::Slider);
    let text_view = demo_button(&mut commands, "Text view", DialogDemo::TextView);
    let title = commands
        .spawn((
            Text::new("CTK dialog suite"),
            TextFont::from_font_size(24.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
        ))
        .id();
    let description = commands
        .spawn((
            Text::new("Open several dialogs to exercise the shared FIFO modal lane."),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id();
    let row = commands
        .spawn(Node {
            max_width: px(680),
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            justify_content: JustifyContent::Center,
            column_gap: px(9),
            row_gap: px(9),
            ..default()
        })
        .add_children(&[
            message, confirm, prompt, secret, choice, multi, progress, slider, text_view,
        ])
        .id();
    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: px(18),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::SURFACE),
        ))
        .add_children(&[title, description, row]);
}

fn demo_button(commands: &mut Commands, label: &str, demo: DialogDemo) -> Entity {
    let label = commands
        .spawn((
            Text::new(label),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
        ))
        .id();
    commands
        .spawn((
            Node {
                min_height: px(32),
                padding: UiRect::axes(px(12), px(6)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::CONTROL),
            BorderColor::all(Color::NONE),
            bevy::feathers::theme::ThemeBorderColor(tokens::BORDER),
            Button,
            ActivateOnPress,
            TabIndex(0),
            demo,
        ))
        .add_child(label)
        .id()
}

fn open_demo(
    activated: On<Activate>,
    demos: Query<&DialogDemo>,
    mut requests: MessageWriter<InteractionRequest>,
    mut progress_demo: ResMut<ProgressDemo>,
) {
    let Ok(demo) = demos.get(activated.entity) else {
        return;
    };
    let request = match demo {
        DialogDemo::Message => {
            InteractionRequest::message("Message", "The shared message presenter is live.")
        }
        DialogDemo::Confirm => InteractionRequest::confirm(
            "Continue?",
            "Confirm retains legacy action-key outcomes for existing consumers.",
        )
        .action(InteractionAction::new(
            "cancel",
            "Cancel",
            ActionRole::Cancel,
        ))
        .action(InteractionAction::new("continue", "Continue", ActionRole::Accept).default()),
        DialogDemo::Prompt => InteractionRequest::prompt(
            "Name this item",
            "Invalid file names remain open with an inline error.",
        )
        .initial_text("New Folder")
        .select_all()
        .validator(TextValidator::new(validate_filename)),
        DialogDemo::SecretPrompt => InteractionRequest::secret_prompt(
            "Enter a secret",
            "The value is masked, redacted from accessibility and zeroised on drop.",
        )
        .max_length(128),
        DialogDemo::Choice => InteractionRequest::choice(
            "Choose a channel",
            "Disabled choices remain visible but cannot be selected.",
            [
                ChoiceItem::new("stable", "Stable")
                    .description("Recommended for production workloads"),
                ChoiceItem::new("preview", "Preview")
                    .description("Newer features with a faster release cadence"),
                ChoiceItem::new("nightly", "Nightly")
                    .description("Unavailable in this gallery")
                    .disabled(),
            ],
        )
        .initial_choice("stable"),
        DialogDemo::MultiChoice => InteractionRequest::multi_choice(
            "Select reports",
            "Selections resolve in the order shown.",
            [
                ChoiceItem::new("summary", "Summary"),
                ChoiceItem::new("events", "Event log"),
                ChoiceItem::new("metrics", "Metrics"),
            ],
        )
        .initial_choices(["summary", "metrics"]),
        DialogDemo::Progress => {
            // The gallery driver tracks a single in-flight progress card. Refuse a second
            // launch rather than overwrite `active` and orphan the first card (which would
            // then advance no further). The widget layer itself supports concurrent cards;
            // this cap is the example harness's, not ctk's.
            if progress_demo.active.is_some() {
                InteractionRequest::message(
                    "Progress already running",
                    "This gallery drives one progress card at a time — wait for the current \
                     export to finish or cancel it before starting another.",
                )
            } else {
                let request = InteractionRequest::progress(
                    "Exporting archive",
                    "Starting export…",
                    ProgressValue::Determinate {
                        current: 0,
                        total: 20,
                    },
                )
                .cancellable();
                progress_demo.active = Some(ActiveProgressDemo {
                    id: request.id(),
                    current: 0,
                    timer: Timer::from_seconds(0.2, TimerMode::Repeating),
                });
                request
            }
        }
        DialogDemo::Slider => InteractionRequest::slider(
            "Retention period",
            "Choose the number of days to retain local history.",
            1,
            30,
            1,
            7,
        ),
        DialogDemo::TextView => InteractionRequest::text_view(
            "Release notes",
            "Scrollable, read-only content with an acknowledge action.",
            "Phase 2b\n\n\
             • Choice and multi-choice dialogs\n\
             • Non-modal owner-driven progress\n\
             • Integer slider backed by CTK numeric controls\n\
             • Scrollable text view\n\n\
             This deliberately includes enough text to exercise vertical scrolling.\n\
             The same shell, focus trap and action semantics are shared by every modal kind.\n\
             Progress stays outside that lane and can advance while a modal is open.",
        )
        .monospace(),
    };
    requests.write(request);
}

fn advance_progress(
    time: Res<Time>,
    mut demo: ResMut<ProgressDemo>,
    mut updates: MessageWriter<ProgressUpdate>,
    mut completions: MessageWriter<ProgressComplete>,
) {
    let Some(active) = demo.active.as_mut() else {
        return;
    };
    active.timer.tick(time.delta());
    if !active.timer.just_finished() {
        return;
    }
    active.current += 1;
    if active.current >= 20 {
        completions.write(ProgressComplete::new(
            active.id,
            ProgressCompletion::Succeeded,
        ));
        demo.active = None;
        return;
    }
    updates.write(
        ProgressUpdate::new(active.id)
            .label(format!("Exported {} of 20 files", active.current))
            .progress(ProgressValue::Determinate {
                current: active.current,
                total: 20,
            }),
    );
}

fn report_results(
    mut results: MessageReader<InteractionResult>,
    mut progress_demo: ResMut<ProgressDemo>,
) {
    for result in results.read() {
        if progress_demo
            .active
            .as_ref()
            .is_some_and(|active| active.id == result.id)
        {
            progress_demo.active = None;
        }
        match &result.outcome {
            InteractionOutcome::Resolved(InteractionValue::Secret(_)) => {
                info!(id = %result.id, "secret prompt resolved (value redacted)");
            }
            outcome => info!(id = %result.id, ?outcome, "dialog resolved"),
        }
    }
}
