//! Dialog and layout verbs (scene-editor plan §4.3 Q2): `shell.dialog.show`,
//! `shell.dialog.hide` and `shell.scene.layout`. They live here, not in
//! `bus_service.rs`, so Stage Q1 (which owns `bus_service.rs`) and Stage Q2
//! (which owns this file and `embedded.rs`) never edit the same file.
//!
//! Stage S registers the routing only: each verb answers rc 10
//! `{error_code:"UNIMPLEMENTED", message}` until Q2 implements it. The
//! request/reply/refusal shapes are frozen in
//! `src/desktop/scripts/tests/fixtures/scene-editor/shell-verbs.json`.
//! Like every Bus verb they are mesh-open with no owner gate; what stays
//! unconditional is correctness (the scene exists and is a dialog).

use serde_json::json;

/// Commands this module answers.
pub(crate) const VERBS: &[&str] = &["shell.dialog.show", "shell.dialog.hide", "shell.scene.layout"];

pub(crate) fn handles(command: &str) -> bool {
    VERBS.contains(&command)
}

/// Stage S stub: `(rc, body)` for a command [`handles`] accepted.
pub(crate) fn dispatch(command: &str) -> (u8, String) {
    (
        10,
        json!({
            "error_code": "UNIMPLEMENTED",
            "message": format!("{command} arrives in Stage Q2 of the Scene Editor plan"),
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_verbs_answer_unimplemented_until_q2() {
        for verb in VERBS {
            assert!(handles(verb));
            let (rc, body) = dispatch(verb);
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!((rc, body["error_code"].as_str()), (10, Some("UNIMPLEMENTED")));
        }
        assert!(!handles("shell.scene.load"));
        assert!(!handles("shell.panel.order"));
    }
}
