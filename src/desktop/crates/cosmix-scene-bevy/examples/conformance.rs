//! Compare a live shell.scene.get reply with P1's public resolver.
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        3,
        "usage: conformance SCENE_DOCUMENT GET_REPLY_JSON"
    );
    let document = cosmix_scene::parse(&std::fs::read_to_string(&args[1]).unwrap()).unwrap();
    let expected = serde_json::to_value(cosmix_scene::resolve(&document).unwrap()).unwrap();
    let actual: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&args[2]).unwrap()).unwrap();
    assert!(
        equal(&actual, &expected),
        "live adapter tree differs from P1"
    );
    println!("SCENE_CONFORMANCE PASS ids families resolved ports defaults templates metadata");
}

// Mix's JSON round-trip writes integral f64 values without a decimal suffix.
// Compare JSON numbers by value while keeping every key and array position exact.
fn equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| equal(a, b)))
        }
        _ => a == b,
    }
}
