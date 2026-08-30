//! Strict-data parse mode for `.*.mix` substrate-internal data files.
//!
//! These tests pin the contract from
//! `_doc/2026-05-04-substrate-mix-data-formats.md` § 4: the literal-
//! data subset is accepted and produces a tree-shaped `Value` that
//! round-trips through `Value::write_mix_data`; every executable Mix
//! construct is rejected as `MixError::StrictDataViolation`.

use cosmix_mix::{error::MixError, parse_data, value::Value};

fn p(src: &str) -> Value {
    parse_data(src).expect("strict-data parse should accept this fixture")
}

fn rejects(src: &str) -> MixError {
    match parse_data(src) {
        Ok(v) => panic!("expected strict-data violation, got Value: {v:?}"),
        Err(e) => e,
    }
}

fn assert_violation(err: &MixError, construct_substring: &str) {
    match err {
        MixError::StrictDataViolation { construct, .. } => {
            assert!(
                construct.contains(construct_substring),
                "expected violation construct to contain {construct_substring:?}, got {construct:?}"
            );
        }
        other => panic!("expected StrictDataViolation, got {other:?}"),
    }
}

// --- Accept: per Value variant -------------------------------------------------

#[test]
fn accepts_top_level_map_body_without_braces() {
    let v = p("name: \"alpha\"\nnumber: 42\n");
    let Value::Map(m) = &v else {
        panic!("expected map")
    };
    assert_eq!(m.get("name"), Some(&Value::String("alpha".into())));
    assert_eq!(m.get("number"), Some(&Value::Number(42.0)));
}

#[test]
fn accepts_explicit_brace_map_at_top_level() {
    let v = p("{ name: \"alpha\", n: 1 }");
    let Value::Map(m) = &v else {
        panic!("expected map")
    };
    assert_eq!(m.len(), 2);
}

#[test]
fn accepts_top_level_list() {
    let v = p("[1, 2, 3]");
    assert_value_eq(
        &v,
        &Value::list(vec![
            Value::Number(1.0),
            Value::Number(2.0),
            Value::Number(3.0),
        ]),
    );
}

#[test]
fn accepts_empty_input_as_empty_map() {
    let v = p("");
    let Value::Map(m) = &v else {
        panic!("expected empty map")
    };
    assert!(m.is_empty());
}

#[test]
fn accepts_empty_input_with_only_newlines() {
    let v = p("\n\n  \n");
    let Value::Map(m) = &v else {
        panic!("expected empty map")
    };
    assert!(m.is_empty());
}

#[test]
fn accepts_all_scalar_types() {
    let v = p("s: \"text\"\n\
         n: 1.25\n\
         neg: -42\n\
         t: true\n\
         f: false\n\
         z: nil\n");
    let Value::Map(m) = &v else { panic!() };
    assert_eq!(m.get("s"), Some(&Value::String("text".into())));
    assert_eq!(m.get("n"), Some(&Value::Number(1.25)));
    assert_eq!(m.get("neg"), Some(&Value::Number(-42.0)));
    assert_eq!(m.get("t"), Some(&Value::Bool(true)));
    assert_eq!(m.get("f"), Some(&Value::Bool(false)));
    assert_eq!(m.get("z"), Some(&Value::Nil));
}

#[test]
fn accepts_literal_heredocs_with_legacy_newline_semantics() {
    let v = p("plain: <<E\nhello\nE\ntrailing: <<E\nhello\n\nE\n");
    let Value::Map(m) = &v else { panic!() };
    assert_eq!(m.get("plain"), Some(&Value::String("hello".into())));
    assert_eq!(m.get("trailing"), Some(&Value::String("hello\n".into())));
}

#[test]
fn accepts_nested_lists_and_maps() {
    let v = p("outer: { inner: [1, [2, 3], { deep: \"value\" }] }\n\
         flat: [{ a: 1 }, { b: 2 }]\n");
    let Value::Map(m) = &v else { panic!() };
    assert!(matches!(m.get("outer"), Some(Value::Map(_))));
    let Some(Value::List(items)) = m.get("flat") else {
        panic!()
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn accepts_quoted_string_keys() {
    let v = p("{ \"with-dash\": 1, plain: 2 }");
    let Value::Map(m) = &v else { panic!() };
    assert!(m.contains_key("with-dash"));
    assert!(m.contains_key("plain"));
}

#[test]
fn accepts_trailing_comma_in_list() {
    let v = p("[1, 2, 3,]");
    assert_value_eq(
        &v,
        &Value::list(vec![
            Value::Number(1.0),
            Value::Number(2.0),
            Value::Number(3.0),
        ]),
    );
}

#[test]
fn rejects_braced_map_entries_without_comma() {
    // Lexer suppresses newlines inside `{ }`, so `{ a: 1\n b: 2 }`
    // reaches the parser as adjacent key-value pairs. Strict-data
    // mode requires explicit commas to avoid silently accepting
    // malformed input.
    let e = rejects("{\n  a: 1\n  b: 2\n  c: 3\n}");
    match e {
        MixError::ParseError { .. } | MixError::StrictDataViolation { .. } => {}
        other => panic!("expected parse/violation error, got {other:?}"),
    }
}

#[test]
fn accepts_comma_separated_multi_line_braced_map() {
    let v = p("{\n  a: 1,\n  b: 2,\n  c: 3,\n}");
    let Value::Map(m) = &v else { panic!() };
    assert_eq!(m.len(), 3);
}

#[test]
fn accepts_full_spec_mix_fixture() {
    let src = include_str!("fixtures/example.spec.mix");
    let v = p(src);
    let Value::Map(m) = &v else {
        panic!("fixture must be a top-level map")
    };
    assert_eq!(m.get("name"), Some(&Value::String("phase-8d.4-fix".into())));
    assert_eq!(m.get("priority"), Some(&Value::Number(2.0)));
    assert_eq!(m.get("draft"), Some(&Value::Bool(false)));
    assert_eq!(m.get("deadline"), Some(&Value::Nil));
    let Some(Value::List(allow)) = m.get("allow_list") else {
        panic!()
    };
    assert_eq!(allow.len(), 1);
    let Some(Value::Map(review)) = m.get("review") else {
        panic!()
    };
    assert_eq!(review.get("reviewer"), Some(&Value::String("codex".into())));
}

// --- Reject: per executable construct ----------------------------------------

#[test]
fn rejects_string_interpolation() {
    // Mix interpolation requires `${name}` braces; a bare `$name`
    // inside double quotes is just a literal `$`.
    let e = rejects("greeting: \"hello ${name}\"\n");
    assert_violation(&e, "interpolation");
}

#[test]
fn rejects_heredoc_interpolation() {
    let e = rejects("note: <<E\n${name}\nE\n");
    assert_violation(&e, "interpolation");
}

#[test]
fn rejects_variable_reference() {
    let e = rejects("x: $foo\n");
    assert_violation(&e, "variable reference");
}

#[test]
fn rejects_command_substitution() {
    let e = rejects("dir: $(pwd)\n");
    assert_violation(&e, "command substitution");
}

#[test]
fn rejects_function_call() {
    let e = rejects("now: time()\n");
    assert_violation(&e, "function call");
}

#[test]
fn rejects_send_expression() {
    let e = rejects("result: send \"target.local\" ping\n");
    assert_violation(&e, "send");
}

#[test]
fn rejects_sh_expression() {
    let e = rejects("out: sh \"ls\"\n");
    assert_violation(&e, "sh");
}

#[test]
fn rejects_function_literal() {
    let e = rejects("f: function($x) = $x + 1\n");
    assert_violation(&e, "function literal");
}

#[test]
fn rejects_parenthesised_expression() {
    let e = rejects("n: (1 + 2)\n");
    assert_violation(&e, "parenthesised");
}

#[test]
fn rejects_arithmetic_via_trailing_token() {
    // `1 + 2` parses `1` as the value, then sees `+` as trailing.
    let e = rejects("n: 1 + 2\n");
    match e {
        MixError::StrictDataViolation { construct, .. } => {
            assert!(
                construct.contains("trailing") || construct.contains("missing separator"),
                "got construct: {construct:?}"
            );
        }
        other => panic!("expected StrictDataViolation, got {other:?}"),
    }
}

#[test]
fn rejects_negation_of_non_number() {
    // `-true` is not a valid data literal.
    let e = rejects("flag: -true\n");
    assert_violation(&e, "unary minus");
}

#[test]
fn rejects_duplicate_key_at_top_level() {
    // `role: "user"\nrole: "admin"` is last-wins under permissive
    // map semantics; strict-data treats it as a copy-paste bug and
    // refuses to silently pick a winner.
    let e = rejects("role: \"user\"\nrole: \"admin\"\n");
    assert_violation(&e, "duplicate map key `role`");
}

#[test]
fn rejects_duplicate_key_inside_braces() {
    let e = rejects("{ a: 1, a: 2 }");
    assert_violation(&e, "duplicate map key `a`");
}

#[test]
fn rejects_top_level_scalar() {
    // Per the design decision, top-level scalars are not supported —
    // the file shape is map body, `[ list ]`, or `{ map }`.
    let e = rejects("42");
    assert_violation(&e, "non-identifier map key");
}

// --- Round-trip property -----------------------------------------------------

#[test]
fn round_trips_through_write_mix_data() {
    // Build a representative Value tree, format it via the strict-data
    // serializer, re-parse it, and check structural equality. This is
    // the load-bearing property: data values survive
    // `write_mix_data → parse_data`.
    //
    // Includes the cases that broke the prior `write_mix`-based
    // claim: empty string, string-that-looks-like-keyword, string
    // with spaces, key with dash, embedded quote/backslash/newline.
    use indexmap::IndexMap;
    let mut inner = IndexMap::new();
    inner.insert("port".to_string(), Value::Number(7777.0));
    inner.insert("tls".to_string(), Value::Bool(true));
    inner.insert("note".to_string(), Value::String("hello world".to_string()));
    inner.insert("empty".to_string(), Value::String(String::new()));
    inner.insert(
        "keyword_lookalike".to_string(),
        Value::String("true".to_string()),
    );
    inner.insert(
        "with_specials".to_string(),
        Value::String("quote\"backslash\\newline\nand-$dollar".to_string()),
    );

    let mut outer = IndexMap::new();
    outer.insert("name".to_string(), Value::String("alpha".to_string()));
    outer.insert(
        "peers".to_string(),
        Value::list(vec![Value::String("a".into()), Value::String("b".into())]),
    );
    outer.insert("config".to_string(), Value::map(inner));
    outer.insert("retired".to_string(), Value::Nil);
    outer.insert("ratio".to_string(), Value::Number(0.75));
    // Key with dash — would lex as `with` `-` `dash` if emitted
    // as a bareword. The serializer must quote it.
    outer.insert("with-dash".to_string(), Value::Number(1.0));

    let original = Value::map(outer);
    let formatted = original.to_mix_data_string().expect("serialize");
    let reparsed = parse_data(&formatted)
        .unwrap_or_else(|e| panic!("re-parse failed: {e}\nformatted: {formatted}"));

    assert_value_eq(&original, &reparsed);

    // Idempotence: a second round-trip reaches the same source text.
    let second = reparsed.to_mix_data_string().expect("serialize");
    assert_eq!(formatted, second, "to_mix_data_string is not idempotent");
}

#[test]
fn write_mix_data_quotes_string_that_looks_like_keyword() {
    // Without quoting, Value::String("true") would emit `true` and
    // re-parse as Value::Bool(true) — silent type corruption.
    let v = Value::String("true".to_string());
    let s = v.to_mix_data_string().expect("serialize");
    assert_eq!(s, "\"true\"", "got {s:?}");
    // Reparsed as a top-level list to dodge the no-top-level-scalar
    // rule, then unwrap.
    let wrapped = format!("[{}]", s);
    let parsed = parse_data(&wrapped).unwrap();
    let Value::List(items) = &parsed else {
        panic!()
    };
    assert_value_eq(&items[0], &v);
}

#[test]
fn write_mix_data_escapes_leading_tilde_for_round_trip() {
    // The lexer expands a leading `~` followed by `/` or end-of-string
    // to $HOME at runtime. Without escaping, Value::String("~") and
    // Value::String("~/foo") would serialize as "~"/"~/foo" and
    // re-parse as $HOME / $HOME/foo — silently losing the value.
    // Strings where the leading `~` is followed by anything else (a
    // letter, a dot, end of the string-payload chars before the closing
    // quote of something more complex) are NOT lexer-expanded and need
    // no escape.
    for s in &["~", "~/", "~/foo", "~/.config/cosmix"] {
        let v = Value::String((*s).to_string());
        let emitted = v.to_mix_data_string().expect("serialize");
        let wrapped = format!("[{}]", emitted);
        let parsed = parse_data(&wrapped).unwrap_or_else(|e| {
            panic!("tilde round-trip failed for {s:?}: {e}\nemitted: {emitted}")
        });
        let Value::List(items) = &parsed else {
            panic!()
        };
        assert_value_eq(&items[0], &v);
    }

    // Mid-string `~` and `~letter` at position 0 are NOT lexer-expanded
    // and must serialize WITHOUT a leading-tilde escape (so existing
    // DNS-zone-token style strings stay byte-identical on round-trip).
    for s in &["~example.net", "~bus", "~.", "a~b", "a~/b"] {
        let v = Value::String((*s).to_string());
        let emitted = v.to_mix_data_string().expect("serialize");
        assert!(
            !emitted.starts_with("\"\\~"),
            "non-expanding tilde at {s:?} got escaped unnecessarily: {emitted}"
        );
        let wrapped = format!("[{}]", emitted);
        let parsed = parse_data(&wrapped)
            .unwrap_or_else(|e| panic!("round-trip failed for {s:?}: {e}\nemitted: {emitted}"));
        let Value::List(items) = &parsed else {
            panic!()
        };
        assert_value_eq(&items[0], &v);
    }
}

#[test]
fn write_mix_data_escapes_special_characters() {
    let v = Value::String("a\"b\\c\nd\te\rf$g\x1bh".to_string());
    let s = v.to_mix_data_string().expect("serialize");
    let wrapped = format!("[{}]", s);
    let parsed = parse_data(&wrapped)
        .unwrap_or_else(|e| panic!("escape round-trip failed: {e}\nemitted: {s}"));
    let Value::List(items) = &parsed else {
        panic!()
    };
    assert_value_eq(&items[0], &v);
}

#[test]
fn round_trips_large_integral_floats_outside_i64_range() {
    // Large integral floats (1e20, -1e20, 1e30) overflow i64 — the
    // serializer must NOT cast-truncate them. Cases inside i64 range
    // (42, -2^53) still take the integer branch.
    let cases: &[f64] = &[
        0.0,
        42.0,
        -42.0,
        1e15,                    // within i64 range
        9_007_199_254_740_992.0, // 2^53 — last exactly-representable integer
        1e20,                    // outside i64 range, integral
        -1e20,
        1e30,
        1.25, // non-integral control
        -0.5,
    ];
    for &n in cases {
        let v = Value::Number(n);
        let s = v
            .to_mix_data_string()
            .unwrap_or_else(|e| panic!("serialize failed for {n}: {e}"));
        let wrapped = format!("[{}]", s);
        let parsed =
            parse_data(&wrapped).unwrap_or_else(|e| panic!("re-parse failed for {n} ({s:?}): {e}"));
        let Value::List(items) = &parsed else {
            panic!()
        };
        let Value::Number(reparsed) = items[0] else {
            panic!("re-parsed as non-Number: {:?}", items[0])
        };
        // Bit-equal (handles 0.0 vs -0.0 distinction too, though
        // none of the cases hit that).
        assert_eq!(
            n.to_bits(),
            reparsed.to_bits(),
            "round-trip altered value: {n} → {s:?} → {reparsed}"
        );
    }
}

#[test]
fn write_mix_data_rejects_non_finite_numbers() {
    // Non-finite f64 values have no strict-data round-trip:
    // `inf` / `NaN` re-parse as `Value::String`, `-inf` rejects.
    // The serializer must refuse rather than emit non-round-tripping
    // output.
    for n in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let v = Value::Number(n);
        match v.to_mix_data_string() {
            Ok(s) => panic!("expected error for {n}, got Ok({s:?})"),
            Err(MixError::DataSerializeError { msg }) => {
                assert!(
                    msg.contains("non-finite"),
                    "expected non-finite mention, got {msg:?}"
                );
            }
            Err(other) => panic!("expected DataSerializeError, got {other:?}"),
        }
    }
}

#[test]
fn write_mix_data_rejects_non_finite_inside_nested_tree() {
    // Recursion into List/Map must propagate the error.
    use indexmap::IndexMap;
    let mut m = IndexMap::new();
    m.insert("ok".to_string(), Value::Number(1.0));
    m.insert("bad".to_string(), Value::Number(f64::NAN));
    let nested = Value::list(vec![Value::map(m)]);
    let result = nested.to_mix_data_string();
    assert!(matches!(result, Err(MixError::DataSerializeError { .. })));
}

/// Structural equality that matches across all `Value` variants —
/// the crate's `PartialEq` is deliberately partial (cross-type
/// number-string coercion, no list/map equality), so tests need
/// their own walker.
fn assert_value_eq(a: &Value, b: &Value) {
    match (a, b) {
        (Value::String(x), Value::String(y)) => assert_eq!(x, y),
        (Value::Number(x), Value::Number(y)) => assert_eq!(x, y),
        (Value::Bool(x), Value::Bool(y)) => assert_eq!(x, y),
        (Value::Nil, Value::Nil) => {}
        (Value::List(xs), Value::List(ys)) => {
            assert_eq!(xs.len(), ys.len());
            for (x, y) in xs.iter().zip(ys.iter()) {
                assert_value_eq(x, y);
            }
        }
        (Value::Map(xs), Value::Map(ys)) => {
            assert_eq!(xs.len(), ys.len());
            for (k, vx) in xs.iter() {
                let vy = ys.get(k).unwrap_or_else(|| panic!("missing key {k}"));
                assert_value_eq(vx, vy);
            }
        }
        (x, y) => panic!("type mismatch: {x:?} vs {y:?}"),
    }
}
