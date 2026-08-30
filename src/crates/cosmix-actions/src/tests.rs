use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::*;

fn id(value: &'static str) -> ActionId {
    ActionId::from_static(value)
}

fn stroke(value: &str) -> KeyStroke {
    value.parse().expect("valid test stroke")
}

fn chord(value: &str) -> Chord {
    value.parse().expect("valid test chord")
}

fn binding(action: &'static str, keys: &str) -> Binding {
    Binding {
        action: id(action),
        chord: chord(keys),
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    }
}

fn input(keys: &str) -> RawInput {
    let stroke = stroke(keys);
    RawInput::pressed(stroke.key, stroke.modifiers)
}

fn keymap(bindings: Vec<Binding>) -> Keymap {
    Keymap {
        defaults: bindings,
        ..Keymap::default()
    }
}

fn resolve_one(
    raw: RawInput,
    context: &FocusContext,
    keymap: &Keymap,
    state: &mut ResolveState,
    at: u64,
) -> Resolved {
    resolve(raw, context, keymap, state, Tick(at))
}

#[test]
fn action_id_is_static_shaped_and_dynamic_ids_are_interned() {
    assert_eq!(std::mem::size_of::<ActionId>(), std::mem::size_of::<&str>());
    let first = ActionId::intern("view-mixer").unwrap();
    let owned = "view-mixer".to_owned();
    let second = ActionId::intern(&owned).unwrap();
    assert_eq!(first, second);
    assert!(std::ptr::eq(first.as_str(), second.as_str()));
    assert_eq!(
        ActionId::intern("bad action"),
        Err(ActionIdError::InvalidCharacter)
    );
}

#[test]
fn custom_binding_replaces_same_action_default_and_beats_conflicting_default() {
    let mut map = keymap(vec![
        binding("save", "Ctrl+S"),
        binding("default-conflict", "Ctrl+K"),
    ]);
    map.custom = vec![
        BindingOverride {
            action: id("save"),
            chord: Some(chord("Ctrl+Shift+S")),
            scope: BindingScope::Global,
            repeat: RepeatPolicy::Ignore,
            allow_in_editable: false,
        },
        BindingOverride {
            action: id("custom-winner"),
            chord: Some(chord("Ctrl+K")),
            scope: BindingScope::Global,
            repeat: RepeatPolicy::Ignore,
            allow_in_editable: false,
        },
    ];
    let context = FocusContext::global();
    let mut state = ResolveState::default();

    assert_eq!(
        resolve_one(input("Ctrl+S"), &context, &map, &mut state, 0),
        Resolved {
            actions: vec![],
            outcome: ResolveOutcome::NoMatch,
            diagnostics: vec![],
        }
    );
    assert_eq!(
        resolve_one(input("Ctrl+Shift+S"), &context, &map, &mut state, 1).actions,
        [id("save")]
    );
    assert_eq!(
        resolve_one(input("Ctrl+K"), &context, &map, &mut state, 2).actions,
        [id("custom-winner")]
    );
}

#[test]
fn longer_chord_defers_shorter_then_wins() {
    let map = keymap(vec![
        binding("short", "Ctrl+K"),
        binding("long", "Ctrl+K, Ctrl+C"),
    ]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();

    assert_eq!(
        resolve_one(input("Ctrl+K"), &context, &map, &mut state, 100),
        Resolved {
            actions: vec![],
            outcome: ResolveOutcome::Pending {
                deadline: Tick(1_100),
            },
            diagnostics: vec![],
        }
    );
    assert_eq!(
        resolve_one(input("Ctrl+C"), &context, &map, &mut state, 500),
        Resolved {
            actions: vec![id("long")],
            outcome: ResolveOutcome::Complete,
            diagnostics: vec![],
        }
    );
}

#[test]
fn cross_layer_prefixes_preserve_lower_layer_fallbacks_both_directions() {
    let context = FocusContext::global();

    let mut default_short = keymap(vec![binding("default-short", "Ctrl+K")]);
    default_short.custom.push(BindingOverride {
        action: id("custom-long"),
        chord: Some(chord("Ctrl+K, Ctrl+C")),
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    });
    let mut state = ResolveState::default();
    assert!(matches!(
        resolve_one(input("Ctrl+K"), &context, &default_short, &mut state, 0).outcome,
        ResolveOutcome::Pending { .. }
    ));
    assert_eq!(
        resolve_timeout(&context, &default_short, &mut state, Tick(1_000)).actions,
        [id("default-short")]
    );

    let mut default_long = keymap(vec![binding("default-long", "Ctrl+K, Ctrl+C")]);
    default_long.custom.push(BindingOverride {
        action: id("custom-short"),
        chord: Some(chord("Ctrl+K")),
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    });
    resolve_one(input("Ctrl+K"), &context, &default_long, &mut state, 2_000);
    assert_eq!(
        resolve_timeout(&context, &default_long, &mut state, Tick(3_000)).actions,
        [id("custom-short")]
    );
    resolve_one(input("Ctrl+K"), &context, &default_long, &mut state, 4_000);
    assert_eq!(
        resolve_one(input("Ctrl+C"), &context, &default_long, &mut state, 4_100).actions,
        [id("default-long")]
    );
    assert!(default_short.diagnostics().is_empty());
    assert!(default_long.diagnostics().is_empty());
}

#[test]
fn chord_timeout_commits_shorter_or_cancels_incomplete_long_only() {
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    let map = keymap(vec![
        binding("short", "Ctrl+K"),
        binding("long", "Ctrl+K, Ctrl+C"),
    ]);
    resolve_one(input("Ctrl+K"), &context, &map, &mut state, 10);
    assert!(matches!(
        resolve_timeout(&context, &map, &mut state, Tick(1_009)).outcome,
        ResolveOutcome::Pending { .. }
    ));
    assert_eq!(
        resolve_timeout(&context, &map, &mut state, Tick(1_010)).actions,
        [id("short")]
    );

    let long_only = keymap(vec![binding("long", "Ctrl+K, Ctrl+C")]);
    resolve_one(input("Ctrl+K"), &context, &long_only, &mut state, 2_000);
    assert_eq!(
        resolve_timeout(&context, &long_only, &mut state, Tick(3_000)),
        Resolved {
            actions: vec![],
            outcome: ResolveOutcome::NoMatch,
            diagnostics: vec![],
        }
    );
}

#[test]
fn timeout_revalidates_modal_editable_and_hot_reloaded_keymap() {
    let map = keymap(vec![
        binding("short", "Ctrl+K"),
        binding("long", "Ctrl+K, Ctrl+C"),
    ]);
    let global = FocusContext::global();
    let mut state = ResolveState::default();

    resolve_one(input("Ctrl+K"), &global, &map, &mut state, 0);
    let modal = FocusContext::modal("file").unwrap();
    let modal_result = resolve_timeout(&modal, &map, &mut state, Tick(1_000));
    assert!(modal_result.actions.is_empty());
    assert_eq!(modal_result.outcome, ResolveOutcome::NoMatch);
    assert_eq!(
        modal_result.diagnostics,
        [ResolveDiagnostic::PendingInvalidated {
            reason: PendingInvalidation::FocusContextChanged,
        }]
    );

    resolve_one(input("Ctrl+K"), &global, &map, &mut state, 2_000);
    let editable = FocusContext::global().with_editable(true);
    let editable_result = resolve_timeout(&editable, &map, &mut state, Tick(3_000));
    assert!(editable_result.actions.is_empty());
    assert_eq!(editable_result.outcome, ResolveOutcome::NoMatch);
    assert_eq!(
        editable_result.diagnostics,
        [ResolveDiagnostic::PendingInvalidated {
            reason: PendingInvalidation::FocusContextChanged,
        }]
    );

    resolve_one(input("Ctrl+K"), &global, &map, &mut state, 4_000);
    let reloaded = keymap(vec![binding("long", "Ctrl+K, Ctrl+C")]);
    let reload_result = resolve_timeout(&global, &reloaded, &mut state, Tick(5_000));
    assert!(reload_result.actions.is_empty());
    assert_eq!(reload_result.outcome, ResolveOutcome::NoMatch);
    assert_eq!(
        reload_result.diagnostics,
        [ResolveDiagnostic::PendingInvalidated {
            reason: PendingInvalidation::KeymapChanged,
        }]
    );
}

#[test]
fn mismatching_second_stroke_commits_shorter_and_reprocesses_new_stroke() {
    let map = keymap(vec![
        binding("short", "Ctrl+K"),
        binding("long", "Ctrl+K, Ctrl+C"),
        binding("other", "Ctrl+X"),
    ]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    resolve_one(input("Ctrl+K"), &context, &map, &mut state, 0);
    let got = resolve_one(input("Ctrl+X"), &context, &map, &mut state, 100);
    assert_eq!(got.actions, [id("short"), id("other")]);
    assert_eq!(got.outcome, ResolveOutcome::Complete);
}

#[test]
fn editable_focus_suppresses_space_but_allows_opted_in_command() {
    let mut save = binding("save", "Ctrl+S");
    save.allow_in_editable = true;
    let map = keymap(vec![binding("toggle", "Space"), save]);
    let context = FocusContext {
        focused_editable: true,
        ..FocusContext::default()
    };
    let mut state = ResolveState::default();

    assert_eq!(
        resolve_one(input("Space"), &context, &map, &mut state, 0).outcome,
        ResolveOutcome::Suppressed(SuppressionReason::EditableFocus)
    );
    assert_eq!(
        resolve_one(input("Ctrl+S"), &context, &map, &mut state, 1).actions,
        [id("save")]
    );
}

#[test]
fn modal_capture_is_exclusive_and_modal_binding_beats_global() {
    let mut modal = binding("accept-modal", "Enter");
    modal.scope = BindingScope::modal("file").unwrap();
    let map = keymap(vec![binding("global-enter", "Enter"), modal]);
    let context = FocusContext::modal("file").unwrap();
    let mut state = ResolveState::default();
    assert_eq!(
        resolve_one(input("Enter"), &context, &map, &mut state, 0).actions,
        [id("accept-modal")]
    );
    assert_eq!(
        resolve_one(input("Space"), &context, &map, &mut state, 1).outcome,
        ResolveOutcome::Suppressed(SuppressionReason::ModalCapture("file".into()))
    );
}

#[test]
fn focus_tag_binding_beats_global_in_its_context() {
    let mut local = binding("canvas-delete", "Delete");
    local.scope = BindingScope::focus_tag("canvas").unwrap();
    let map = keymap(vec![binding("global-delete", "Delete"), local]);
    let context = FocusContext::global().with_focus_tag("canvas").unwrap();
    let mut state = ResolveState::default();
    assert_eq!(
        resolve_one(input("Delete"), &context, &map, &mut state, 0).actions,
        [id("canvas-delete")]
    );
}

#[test]
fn repeat_policy_is_per_binding_and_does_not_disturb_pending_chord() {
    let ignored = binding("toggle", "Space");
    let mut allowed = binding("zoom", "Ctrl+Equal");
    allowed.repeat = RepeatPolicy::Allow;
    let map = keymap(vec![ignored, allowed, binding("long", "Ctrl+K, Ctrl+C")]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    let mut repeated_space = input("Space");
    repeated_space.repeat = true;
    assert_eq!(
        resolve_one(repeated_space, &context, &map, &mut state, 0).outcome,
        ResolveOutcome::IgnoredRepeat
    );
    let mut repeated_zoom = input("Ctrl+Equal");
    repeated_zoom.repeat = true;
    assert_eq!(
        resolve_one(repeated_zoom, &context, &map, &mut state, 1).actions,
        [id("zoom")]
    );

    resolve_one(input("Ctrl+K"), &context, &map, &mut state, 2);
    let mut repeated_prefix = input("Ctrl+K");
    repeated_prefix.repeat = true;
    assert_eq!(
        resolve_one(repeated_prefix, &context, &map, &mut state, 3).outcome,
        ResolveOutcome::IgnoredRepeat
    );
    assert_eq!(state.sequence(), [stroke("Ctrl+K")]);
}

#[test]
fn repeated_prefix_keeps_its_admitting_policy_through_timeout() {
    let mut short = binding("repeat-short", "Ctrl+K");
    short.repeat = RepeatPolicy::Allow;
    let mut long = binding("repeat-long", "Ctrl+K, Ctrl+C");
    long.repeat = RepeatPolicy::Allow;
    let mut repeat_ineligible_tag = binding("tag-ignore", "Ctrl+K, Ctrl+X");
    repeat_ineligible_tag.scope = BindingScope::focus_tag("editor").unwrap();
    let map = keymap(vec![short, long, repeat_ineligible_tag]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    let mut repeated_prefix = input("Ctrl+K");
    repeated_prefix.repeat = true;

    assert!(matches!(
        resolve_one(repeated_prefix, &context, &map, &mut state, 0).outcome,
        ResolveOutcome::Pending { .. }
    ));
    let tag_changed = context.with_focus_tag("editor").unwrap();
    let resolved = resolve_timeout(&tag_changed, &map, &mut state, Tick(1_000));
    assert_eq!(resolved.actions, [id("repeat-short")]);
    assert!(resolved.diagnostics.is_empty());
}

#[test]
fn admitting_repeat_state_persists_across_later_chord_strokes() {
    let mut short = binding("repeat-short", "Ctrl+K, Ctrl+C");
    short.repeat = RepeatPolicy::Allow;
    let mut long = binding("repeat-long", "Ctrl+K, Ctrl+C, Ctrl+D");
    long.repeat = RepeatPolicy::Allow;
    let ignored = binding("ignore-short", "Ctrl+K, Ctrl+C");
    let map = keymap(vec![short, long, ignored]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    let mut repeated_prefix = input("Ctrl+K");
    repeated_prefix.repeat = true;

    resolve_one(repeated_prefix, &context, &map, &mut state, 0);
    assert!(matches!(
        resolve_one(input("Ctrl+C"), &context, &map, &mut state, 100).outcome,
        ResolveOutcome::Pending { .. }
    ));
    assert_eq!(
        resolve_timeout(&context, &map, &mut state, Tick(1_100)).actions,
        [id("repeat-short")]
    );
}

#[test]
fn pending_revalidation_ignores_order_and_irrelevant_focus_changes() {
    let mut short = binding("short", "Ctrl+K");
    short.allow_in_editable = true;
    let mut long = binding("long", "Ctrl+K, Ctrl+C");
    long.allow_in_editable = true;
    let map = keymap(vec![short.clone(), long.clone()]);
    let reordered = keymap(vec![long, short]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    resolve_one(input("Ctrl+K"), &context, &map, &mut state, 0);

    let changed = FocusContext::global()
        .with_editable(true)
        .with_focus_tag("unrelated")
        .unwrap();
    let result = resolve_timeout(&changed, &reordered, &mut state, Tick(500));
    assert_eq!(
        result.outcome,
        ResolveOutcome::Pending {
            deadline: Tick(1_000)
        }
    );
    assert!(result.diagnostics.is_empty());
}

#[test]
fn joint_editable_and_tag_change_preserves_same_eligible_candidates() {
    let mut short = binding("global-short", "Ctrl+K");
    short.allow_in_editable = true;
    let mut long = binding("global-long", "Ctrl+K, Ctrl+C");
    long.allow_in_editable = true;
    let mut tag_only = binding("editor-long", "Ctrl+K, Ctrl+X");
    tag_only.scope = BindingScope::focus_tag("editor").unwrap();
    tag_only.allow_in_editable = false;
    let map = keymap(vec![short, long, tag_only]);
    let mut state = ResolveState::default();
    resolve_one(
        input("Ctrl+K"),
        &FocusContext::global(),
        &map,
        &mut state,
        0,
    );

    let changed = FocusContext::global()
        .with_editable(true)
        .with_focus_tag("editor")
        .unwrap();
    let pending = resolve_timeout(&changed, &map, &mut state, Tick(500));
    assert_eq!(
        pending.outcome,
        ResolveOutcome::Pending {
            deadline: Tick(1_000)
        }
    );
    assert!(pending.diagnostics.is_empty());
    assert_eq!(
        resolve_timeout(&changed, &map, &mut state, Tick(1_000)).actions,
        [id("global-short")]
    );
}

#[test]
fn equal_priority_conflicts_are_reported_and_never_invoked() {
    let map = keymap(vec![binding("alpha", "Ctrl+K"), binding("beta", "Ctrl+K")]);
    let mut state = ResolveState::default();
    let got = resolve_one(
        input("Ctrl+K"),
        &FocusContext::global(),
        &map,
        &mut state,
        0,
    );
    assert!(got.actions.is_empty());
    assert_eq!(got.outcome, ResolveOutcome::NoMatch);
    assert_eq!(
        got.diagnostics,
        [ResolveDiagnostic::BindingConflict {
            chord: chord("Ctrl+K"),
            actions: vec![id("alpha"), id("beta")],
        }]
    );
}

#[test]
fn shorter_conflict_is_reported_at_timeout_if_longer_is_not_completed() {
    let map = keymap(vec![
        binding("alpha", "Ctrl+K"),
        binding("beta", "Ctrl+K"),
        binding("long", "Ctrl+K, Ctrl+C"),
    ]);
    let mut state = ResolveState::default();
    resolve_one(
        input("Ctrl+K"),
        &FocusContext::global(),
        &map,
        &mut state,
        0,
    );
    assert_eq!(
        resolve_timeout(&FocusContext::global(), &map, &mut state, Tick(1_000)).diagnostics,
        [ResolveDiagnostic::BindingConflict {
            chord: chord("Ctrl+K"),
            actions: vec![id("alpha"), id("beta")],
        }]
    );
}

#[test]
fn late_input_preserves_expired_conflict_and_still_resolves_new_stroke() {
    let map = keymap(vec![
        binding("alpha", "Ctrl+K"),
        binding("beta", "Ctrl+K"),
        binding("long", "Ctrl+K, Ctrl+C"),
        binding("other", "Ctrl+X, Ctrl+Y"),
    ]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    resolve_one(input("Ctrl+K"), &context, &map, &mut state, 0);
    let got = resolve_one(input("Ctrl+X"), &context, &map, &mut state, 1_000);
    assert!(got.actions.is_empty());
    assert_eq!(
        got.outcome,
        ResolveOutcome::Pending {
            deadline: Tick(2_000),
        }
    );
    assert_eq!(
        got.diagnostics,
        [ResolveDiagnostic::BindingConflict {
            chord: chord("Ctrl+K"),
            actions: vec![id("alpha"), id("beta")],
        }]
    );
}

#[test]
fn release_is_ignored_without_changing_chord_state() {
    let map = keymap(vec![binding("long", "Ctrl+K, Ctrl+C")]);
    let context = FocusContext::global();
    let mut state = ResolveState::default();
    resolve_one(input("Ctrl+K"), &context, &map, &mut state, 0);
    let mut released = input("Ctrl+K");
    released.state = RawInputState::Released;
    assert_eq!(
        resolve_one(released, &context, &map, &mut state, 1).outcome,
        ResolveOutcome::IgnoredRelease
    );
    assert!(state.is_pending());
}

#[test]
fn mix_round_trip_preserves_layers_chords_scope_and_policies() {
    let mut map = keymap(vec![binding("comment", "Ctrl+K, Ctrl+C")]);
    map.chord_timeout_ms = 750;
    map.defaults[0].scope = BindingScope::focus_tag("editor").unwrap();
    map.defaults[0].repeat = RepeatPolicy::Allow;
    map.defaults[0].allow_in_editable = true;
    map.custom.push(BindingOverride {
        action: id("disabled-default"),
        chord: None,
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    });
    let source = to_keymap_mix(&map).unwrap();
    assert!(!source.contains("toml"));
    assert_eq!(parse_keymap(&source).unwrap(), map);
}

#[test]
fn invalid_programmatic_keys_chords_and_scopes_are_rejected() {
    assert_eq!(
        Key::Character('a').validate(),
        Err(KeyParseError::InvalidCharacter('a'))
    );
    assert_eq!(
        Key::Function(0).validate(),
        Err(KeyParseError::InvalidFunction(0))
    );
    assert_eq!(
        BindingScope::modal(""),
        Err(KeyParseError::InvalidScopeName("".into()))
    );
    assert_eq!(Chord::new(Vec::new()), Err(KeyParseError::EmptyChord));
    assert!(matches!(
        Chord::new(vec![stroke("A"); MAX_CHORD_STROKES + 1]),
        Err(KeyParseError::TooManyChordStrokes { .. })
    ));
    assert_eq!(
        Chord::new(vec![KeyStroke {
            key: Key::Character('é'),
            modifiers: Modifiers::NONE,
        }]),
        Err(KeyParseError::InvalidCharacter('é'))
    );

    let mut invalid_scope = binding("bad-scope", "Ctrl+K");
    invalid_scope.scope = BindingScope::Modal(String::new());
    assert!(matches!(
        keymap(vec![invalid_scope]).validate(),
        Err(KeymapError::InvalidBinding { .. })
    ));
}

#[test]
fn keymap_bounds_are_checked_before_batch_interning() {
    let long = "x".repeat(MAX_ACTION_ID_LEN + 1);
    let long_source = format!(
        "{{ version: 1, chord_timeout_ms: 1000, defaults: [{{ action: \"{long}\", chord: [\"A\"] }}], custom: [] }}"
    );
    assert!(
        parse_keymap(&long_source)
            .unwrap_err()
            .to_string()
            .contains("maximum")
    );

    let mut entries = String::new();
    for index in 0..=MAX_KEYMAP_ACTION_IDS {
        if index != 0 {
            entries.push(',');
        }
        entries.push_str(&format!("{{ action: \"cap{index}\", chord: [\"A\"] }}"));
    }
    let source =
        format!("{{ version: 1, chord_timeout_ms: 1000, defaults: [{entries}], custom: [] }}");
    assert!(
        parse_keymap(&source)
            .unwrap_err()
            .to_string()
            .contains("distinct action ids")
    );

    let repeated = binding("one-action", "A");
    let oversized = Keymap {
        defaults: vec![repeated; MAX_KEYMAP_BINDINGS + 1],
        ..Keymap::default()
    };
    assert!(matches!(
        oversized.validate(),
        Err(KeymapError::TooManyBindings { .. })
    ));

    let mut repeated_entries = String::new();
    for index in 0..MAX_KEYMAP_BINDINGS {
        if index != 0 {
            repeated_entries.push(',');
        }
        repeated_entries.push_str("{ action: \"one-action\", chord: [\"A\"] }");
    }
    let repeated_source = format!(
        "{{ version: 1, chord_timeout_ms: 1000, defaults: [{repeated_entries}], custom: [{{ action: \"one-action\", chord: [\"B\"] }}] }}"
    );
    let error = parse_keymap(&repeated_source).unwrap_err();
    assert!(matches!(error, KeymapError::Decode(_)));
    assert!(error.to_string().contains("combined limit"));

    let strokes = std::iter::repeat_n("\"A\"", MAX_CHORD_STROKES + 1)
        .collect::<Vec<_>>()
        .join(",");
    let chord_source = format!(
        "{{ version: 1, chord_timeout_ms: 1000, defaults: [{{ action: \"one-action\", chord: [{strokes}] }}], custom: [] }}"
    );
    assert!(matches!(
        parse_keymap(&chord_source),
        Err(KeymapError::Decode(_))
    ));

    let oversized_source = " ".repeat(MAX_KEYMAP_FILE_BYTES + 1);
    assert!(matches!(
        parse_keymap(&oversized_source),
        Err(KeymapError::FileTooLarge { .. })
    ));
}

#[test]
fn load_keymap_distinguishes_io_from_present_file_parse_failure() {
    let directory =
        std::env::temp_dir().join(format!("cosmix-actions-load-errors-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let missing = directory.join("missing.mix");
    assert!(matches!(
        load_keymap(&missing),
        Err(KeymapError::Read { .. })
    ));

    let malformed = directory.join("malformed.mix");
    std::fs::write(&malformed, "this is not strict data").unwrap();
    assert!(matches!(
        load_keymap(&malformed),
        Err(KeymapError::Parse { .. })
    ));
    std::fs::remove_file(malformed).unwrap();

    let unsupported = directory.join("unsupported.mix");
    std::fs::write(
        &unsupported,
        "{ version: 99, chord_timeout_ms: 1000, defaults: [], custom: [] }",
    )
    .unwrap();
    assert_eq!(
        load_keymap(&unsupported),
        Err(KeymapError::UnsupportedVersion(99))
    );
    std::fs::remove_file(unsupported).unwrap();

    let oversized = directory.join("oversized.mix");
    std::fs::write(&oversized, vec![b' '; MAX_KEYMAP_FILE_BYTES + 1]).unwrap();
    assert!(matches!(
        load_keymap(&oversized),
        Err(KeymapError::FileTooLarge { .. })
    ));
    std::fs::remove_file(oversized).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn capped_reader_consumes_at_most_limit_plus_one() {
    let mut reader = std::io::Cursor::new(vec![b'x'; MAX_KEYMAP_FILE_BYTES + 128]);
    assert!(matches!(
        crate::keymap::read_capped_source(&mut reader),
        Err(crate::keymap::CappedReadError::TooLarge)
    ));
    assert_eq!(reader.position(), (MAX_KEYMAP_FILE_BYTES + 1) as u64);
}

#[test]
fn unsupported_keymap_version_remains_typed() {
    let source = "{ version: 2, v2_only_field: { nested: true } }";
    assert_eq!(
        parse_keymap(source),
        Err(KeymapError::UnsupportedVersion(2))
    );
}

#[test]
fn effective_binding_iterator_and_static_diagnostics_are_public_policy() {
    let mut map = keymap(vec![
        binding("shadowed", "Ctrl+K"),
        binding("conflict-a", "Ctrl+X"),
        binding("conflict-b", "Ctrl+X"),
        binding("rebound", "Ctrl+R"),
    ]);
    map.custom.extend([
        BindingOverride {
            action: id("winner"),
            chord: Some(chord("Ctrl+K")),
            scope: BindingScope::Global,
            repeat: RepeatPolicy::Ignore,
            allow_in_editable: false,
        },
        BindingOverride {
            action: id("rebound"),
            chord: Some(chord("Ctrl+Shift+R")),
            scope: BindingScope::Global,
            repeat: RepeatPolicy::Ignore,
            allow_in_editable: false,
        },
    ]);

    assert_eq!(map.effective_bindings().count(), 5);
    let diagnostics = map.diagnostics();
    assert!(diagnostics.contains(&KeymapDiagnostic::DefaultReplaced {
        action: id("rebound"),
        scope: BindingScope::Global,
        unbound: false,
    }));
    assert!(diagnostics.contains(&KeymapDiagnostic::Shadowed {
        chord: chord("Ctrl+K"),
        scope: BindingScope::Global,
        shadowed: id("shadowed"),
        by: vec![id("winner")],
    }));
    assert!(diagnostics.contains(&KeymapDiagnostic::Conflict {
        chord: chord("Ctrl+X"),
        scope: BindingScope::Global,
        layer: BindingLayer::Default,
        actions: vec![id("conflict-a"), id("conflict-b")],
    }));
}

#[test]
fn shadow_diagnostic_requires_full_repeat_and_editable_coverage() {
    let mut lower = binding("lower", "Ctrl+K");
    lower.repeat = RepeatPolicy::Allow;
    lower.allow_in_editable = true;
    let mut map = keymap(vec![lower]);
    map.custom.push(BindingOverride {
        action: id("ordinary-winner"),
        chord: Some(chord("Ctrl+K")),
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    });

    assert!(!map.diagnostics().iter().any(|diagnostic| matches!(
        diagnostic,
        KeymapDiagnostic::Shadowed { shadowed, .. } if *shadowed == id("lower")
    )));
}

#[test]
fn reverse_lookup_uses_custom_then_default_and_nil_unbinds() {
    let mut map = keymap(vec![binding("save", "Ctrl+S")]);
    assert_eq!(map.binding_for(id("save")).as_deref(), Some("Ctrl+S"));
    map.custom.push(BindingOverride {
        action: id("save"),
        chord: Some(chord("Ctrl+Shift+S")),
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    });
    assert_eq!(map.binding_for(id("save")).as_deref(), Some("Ctrl+Shift+S"));
    map.custom[0].chord = None;
    assert_eq!(map.binding_for(id("save")), None);
    map.custom.push(BindingOverride {
        action: id("save"),
        chord: Some(chord("Ctrl+Alt+S")),
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    });
    assert_eq!(map.binding_for(id("save")).as_deref(), Some("Ctrl+Alt+S"));
}

#[test]
fn reverse_lookup_omits_shadowed_and_conflicted_chords() {
    let mut shadowed = keymap(vec![binding("save", "Ctrl+S")]);
    shadowed.custom.push(BindingOverride {
        action: id("other"),
        chord: Some(chord("Ctrl+S")),
        scope: BindingScope::Global,
        repeat: RepeatPolicy::Ignore,
        allow_in_editable: false,
    });
    assert_eq!(shadowed.binding_for(id("save")), None);
    assert_eq!(shadowed.binding_for(id("other")).as_deref(), Some("Ctrl+S"));

    let conflicted = keymap(vec![binding("save", "Ctrl+S"), binding("other", "Ctrl+S")]);
    assert_eq!(conflicted.binding_for(id("save")), None);
    assert_eq!(conflicted.binding_for(id("other")), None);
}

#[test]
fn studio_asset_has_transport_menu_and_modal_actions() {
    let map = parse_keymap(STUDIO_DEFAULT_KEYMAP_MIX).unwrap();
    assert_eq!(map.defaults.len(), studio::DEFAULT_KEYMAP_ACTION_IDS.len());
    for action in studio::DEFAULT_KEYMAP_ACTION_IDS {
        assert!(
            map.effective_bindings()
                .any(|binding| binding.action == action),
            "missing binding for {action}"
        );
    }
    assert_eq!(
        map.binding_for(studio::TRANSPORT_TOGGLE).as_deref(),
        Some("Space")
    );
}

#[test]
fn filemgr_asset_covers_every_static_action_and_menu_item() {
    let map = parse_keymap(FILEMGR_DEFAULT_KEYMAP_MIX).unwrap();
    assert_eq!(map.defaults.len(), filemgr::DEFAULT_KEYMAP_ACTION_IDS.len());
    for action in filemgr::DEFAULT_KEYMAP_ACTION_IDS {
        assert!(
            map.effective_bindings()
                .any(|binding| binding.action == action),
            "missing binding for {action}"
        );
    }
    for action in filemgr::MENU_ACTION_IDS {
        assert!(
            map.binding_for(action).is_some(),
            "menu action {action} has no accelerator"
        );
    }
    assert_eq!(
        map.binding_for(filemgr::VIEW_TOGGLE_HIDDEN).as_deref(),
        Some("Ctrl+H")
    );
}

fn sample_meta() -> ActionMeta {
    ActionMeta {
        id: id("theme.set"),
        label: "Set theme".into(),
        args_schema: ArgsSchema {
            fields: vec![ActionArg {
                name: "scheme".into(),
                kind: ActionArgKind::String,
                required: true,
                description: Some("Palette scheme".into()),
            }],
            allow_extra: false,
        },
        category: Some("theme".into()),
        icon_name: Some("palette".into()),
        description: Some("Apply a palette".into()),
        interactive: None,
        allowed_sources: ActionSources::default(),
    }
}

#[test]
fn registry_keeps_runtime_behaviour_out_of_serialisable_metadata() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let mut registry = ActionRegistry::new();
    registry
        .register(
            sample_meta(),
            Arc::new(move |_| {
                handler_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            Arc::new(|| true),
        )
        .unwrap();
    let metadata = registry.metadata_collection();
    let source = to_action_metadata_mix(&metadata).unwrap();
    let decoded = parse_action_metadata(&source).unwrap();
    assert_eq!(decoded, metadata);

    let args = BTreeMap::from([("scheme".into(), ActionValue::String("ocean".into()))]);
    registry.invoke(id("theme.set"), &args).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn metadata_decode_is_atomic_and_ignores_unknown_meta_fields() {
    assert!(!ActionId::is_interned("transactional-new-id"));
    let source = r#"[
        {
            id: "transactional-new-id",
            label: "First",
            future_query_field: "accepted"
        },
        {
            id: "later-invalid-schema",
            label: "Second",
            args_schema: {
                fields: [
                    { name: "same", kind: "string" },
                    { name: "same", kind: "boolean" }
                ]
            }
        }
    ]"#;
    let error = parse_action_metadata(source).unwrap_err();
    assert!(
        matches!(error, ActionMetadataError::InvalidSchema { .. }),
        "{error:?}"
    );
    assert!(!ActionId::is_interned("transactional-new-id"));

    let accepted = r#"[{
        id: "metadata-forward-compatible",
        label: "Forward compatible",
        future_query_field: "ignored"
    }]"#;
    assert_eq!(
        parse_action_metadata(accepted).unwrap().as_slice()[0].id,
        ActionId::intern("metadata-forward-compatible").unwrap()
    );
}

#[test]
fn interactive_metadata_round_trips_and_rejects_an_empty_direct_verb() {
    let mut meta = sample_meta();
    meta.interactive = Some(InteractiveAction {
        direct_verb: Some("app.theme.set".into()),
    });
    let metadata = ActionMetadata::new(vec![meta]).unwrap();
    let source = to_action_metadata_mix(&metadata).unwrap();
    assert_eq!(parse_action_metadata(&source).unwrap(), metadata);

    let invalid = r#"[{
        id: "interactive-invalid",
        label: "Invalid",
        interactive: { direct_verb: "" }
    }]"#;
    assert!(matches!(
        parse_action_metadata(invalid),
        Err(ActionMetadataError::InvalidInteractiveDirectVerb { .. })
    ));
    assert!(!ActionId::is_interned("interactive-invalid"));
}

#[test]
fn interactive_actions_cannot_allow_bus_in_metadata_or_live_registries() {
    let source = r#"[{
        id: "interactive-bus-forbidden",
        label: "Forbidden",
        interactive: {},
        allowed_sources: { bus: true }
    }]"#;
    assert!(matches!(
        parse_action_metadata(source),
        Err(ActionMetadataError::InteractiveBusAllowed { .. })
    ));
    assert!(!ActionId::is_interned("interactive-bus-forbidden"));

    let mut meta = sample_meta();
    meta.interactive = Some(InteractiveAction { direct_verb: None });
    meta.allowed_sources = ActionSources::BUS;
    let mut registry = ActionRegistry::new();
    assert_eq!(
        registry.register(meta, Arc::new(|_| Ok(())), Arc::new(|| true)),
        Err(RegistryError::InteractiveBusAllowed {
            id: id("theme.set"),
        })
    );
    assert!(registry.is_empty());
}

#[test]
fn source_policy_defaults_bus_closed_and_can_be_explicitly_opened() {
    let mut registry = ActionRegistry::new();
    registry
        .register(sample_meta(), Arc::new(|_| Ok(())), Arc::new(|| true))
        .unwrap();
    let args = BTreeMap::from([("scheme".into(), ActionValue::String("dark".into()))]);
    assert!(matches!(
        registry.validate_invocation_from(id("theme.set"), &args, ActionSource::Bus),
        Err(RegistryError::SourceNotAllowed { .. })
    ));

    let mut bus_meta = sample_meta();
    bus_meta.id = id("theme.bus-set");
    bus_meta.allowed_sources = ActionSources::BUS;
    registry
        .register(bus_meta, Arc::new(|_| Ok(())), Arc::new(|| true))
        .unwrap();
    assert!(
        registry
            .validate_invocation_from(id("theme.bus-set"), &args, ActionSource::Bus)
            .is_ok()
    );
}

#[test]
fn runtime_registry_bounds_aggregate_metadata() {
    let mut meta = sample_meta();
    meta.label = "x".repeat(MAX_ACTION_REGISTRY_BYTES);
    let mut registry = ActionRegistry::new();
    assert!(matches!(
        registry.register(meta, Arc::new(|_| Ok(())), Arc::new(|| true)),
        Err(RegistryError::RegistryMetadataLimit { .. })
    ));
    assert!(registry.is_empty());
    assert_eq!(registry.revision(), 0);
}

#[test]
fn runtime_registry_bounds_item_count() {
    let mut registry = ActionRegistry::new();
    for index in 0..MAX_ACTION_REGISTRY_ITEMS {
        let mut meta = sample_meta();
        let action = format!("bound.action{index}");
        meta.id = ActionId::intern(&action).unwrap();
        registry
            .register(meta, Arc::new(|_| Ok(())), Arc::new(|| true))
            .unwrap();
    }
    let mut extra = sample_meta();
    extra.id = ActionId::intern("bound.overflow").unwrap();
    assert_eq!(
        registry.register(extra, Arc::new(|_| Ok(())), Arc::new(|| true)),
        Err(RegistryError::RegistryItemLimit {
            maximum: MAX_ACTION_REGISTRY_ITEMS,
        })
    );
}

#[test]
fn registry_checks_enabled_state_and_argument_schema() {
    let mut registry = ActionRegistry::new();
    registry
        .register(sample_meta(), Arc::new(|_| Ok(())), Arc::new(|| false))
        .unwrap();
    assert_eq!(
        registry.invoke(id("theme.set"), &ActionArgs::new()),
        Err(RegistryError::Disabled(id("theme.set")))
    );

    let mut enabled = ActionRegistry::new();
    enabled
        .register(sample_meta(), Arc::new(|_| Ok(())), Arc::new(|| true))
        .unwrap();
    assert_eq!(
        enabled.invoke(id("theme.set"), &ActionArgs::new()),
        Err(RegistryError::MissingArgument {
            id: id("theme.set"),
            argument: "scheme".into(),
        })
    );
    let wrong = BTreeMap::from([("scheme".into(), ActionValue::Boolean(true))]);
    assert_eq!(
        enabled.invoke(id("theme.set"), &wrong),
        Err(RegistryError::WrongArgumentType {
            id: id("theme.set"),
            argument: "scheme".into(),
            expected: ActionArgKind::String,
        })
    );
}
