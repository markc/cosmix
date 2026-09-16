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
    assert_eq!(actual, expected, "live adapter tree differs from P1");
    println!("SCENE_CONFORMANCE PASS ids families resolved ports defaults templates metadata");
}
