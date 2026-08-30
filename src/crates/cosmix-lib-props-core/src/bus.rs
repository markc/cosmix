//! Bus wiring for the SPEC 07 §2 read surface.
//!
//! `<svc>.props.{get,list,describe}` against a [`PropTree`]. The
//! sibling SPEC 12 §5–§8 mutation router (`mutation::PropsRouter`)
//! lives in `cosmix-lib-props-store` (cos repo); that crate's
//! `bus.rs` is a thin shim re-exporting `dispatch_props` /
//! `build_response` / `PropsResponse` from here and declaring its
//! own `pub mod mutation;` so `cosmix_props::bus::mutation::PropsRouter`
//! resolves end-to-end for cos-side consumers.

use crate::path::PropPath;
use crate::tree::PropTree;
use cosmix_bus::bus::BusMessage;
use serde_json::{Value as Json, json};

/// Outcome of dispatching one props.* command.
#[derive(Debug, Clone)]
pub struct PropsResponse {
    pub rc: i32,
    pub body: String,
    pub error: Option<String>,
}

impl PropsResponse {
    fn ok(body: Json) -> Self {
        Self {
            rc: 0,
            body: body.to_string(),
            error: None,
        }
    }
    fn err(rc: i32, msg: impl Into<String>) -> Self {
        let m = msg.into();
        Self {
            rc,
            body: json!({ "error": m.clone() }).to_string(),
            error: Some(m),
        }
    }
}

/// Dispatch a `<svc>.props.{get,list,describe}` request against a tree.
///
/// `command_suffix` is the part after `<svc>.props.` — i.e. `"get"`,
/// `"list"`, or `"describe"`.
///
/// Caller is responsible for routing (matching `<svc>.props.*` and
/// stripping the prefix), and for emitting the response message itself
/// (only the body + rc are produced here).
pub fn dispatch_props(
    tree: &dyn PropTree,
    command_suffix: &str,
    args: Option<&Json>,
    redact_sensitive: bool,
) -> PropsResponse {
    match command_suffix {
        "get" => handle_get(tree, args, redact_sensitive),
        "list" => handle_list(tree),
        "describe" => handle_describe(tree, args),
        other => PropsResponse::err(10, format!("unknown props subcommand: {other}")),
    }
}

fn handle_get(tree: &dyn PropTree, args: Option<&Json>, redact: bool) -> PropsResponse {
    let path_arg = args.and_then(|v| v.get("path")).and_then(|v| v.as_str());
    let snapshot = if redact {
        tree.redacted_snapshot()
    } else {
        tree.snapshot()
    };
    match path_arg {
        None => PropsResponse::ok((&snapshot).into()),
        Some(s) => match PropPath::new(s) {
            Err(e) => PropsResponse::err(10, format!("invalid path: {e}")),
            Ok(p) => match tree.get(&p) {
                Some(v) => {
                    // re-redact the slice if needed
                    let v = if redact {
                        let mut full = if redact {
                            tree.redacted_snapshot()
                        } else {
                            tree.snapshot()
                        };
                        // walk into full via the path again so we get the redacted slice
                        for seg in p.segments() {
                            full = match full {
                                crate::value::PropValue::Object(mut m) => match m.remove(seg) {
                                    Some(x) => x,
                                    None => {
                                        return PropsResponse::err(
                                            10,
                                            format!("path not found: {p}"),
                                        );
                                    }
                                },
                                _ => return PropsResponse::err(10, format!("path not found: {p}")),
                            };
                        }
                        full
                    } else {
                        v
                    };
                    PropsResponse::ok((&v).into())
                }
                None => PropsResponse::err(10, format!("path not found: {p}")),
            },
        },
    }
}

fn handle_list(tree: &dyn PropTree) -> PropsResponse {
    let paths: Vec<String> = tree.list().into_iter().map(|p| p.to_string()).collect();
    PropsResponse::ok(json!(paths))
}

fn handle_describe(tree: &dyn PropTree, args: Option<&Json>) -> PropsResponse {
    let path_arg = match args.and_then(|v| v.get("path")).and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return PropsResponse::err(10, "describe requires args.path"),
    };
    let path = match PropPath::new(path_arg) {
        Ok(p) => p,
        Err(e) => return PropsResponse::err(10, format!("invalid path: {e}")),
    };
    match tree.describe(&path) {
        Some(d) => PropsResponse::ok(serde_json::to_value(&d).unwrap()),
        None => PropsResponse::err(10, format!("unknown path: {path}")),
    }
}

/// Build a response Bus message from a `PropsResponse` and the originating
/// request. Sets `type=response`, mirrors `from`/`to`, and copies the
/// command name. Caller can override headers if needed.
pub fn build_response(
    req: &BusMessage,
    svc: &str,
    command: &str,
    resp: PropsResponse,
) -> BusMessage {
    let mut m = BusMessage::new();
    m.set("bus", "1");
    m.set("type", "response");
    m.set("from", svc);
    if let Some(req_from) = req.from_addr() {
        m.set("to", req_from);
    }
    m.set("command", command);
    m.set("rc", &resp.rc.to_string());
    m.body = resp.body;
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::{PropDescribe, PropType};
    use crate::path::PropPath;
    use crate::tree::build_snapshot;
    use crate::value::PropValue;

    struct T;

    impl PropTree for T {
        fn snapshot(&self) -> PropValue {
            build_snapshot([
                (
                    PropPath::new("config.bind").unwrap(),
                    PropValue::from("a:1"),
                ),
                (
                    PropPath::new("lifecycle.uptime_s").unwrap(),
                    PropValue::from(7_u64),
                ),
            ])
        }
        fn list(&self) -> Vec<PropPath> {
            vec![
                PropPath::new("config.bind").unwrap(),
                PropPath::new("lifecycle.uptime_s").unwrap(),
            ]
        }
        fn describe(&self, p: &PropPath) -> Option<PropDescribe> {
            match p.as_str() {
                "config.bind" => Some(PropDescribe::leaf(p.clone(), PropType::String, "addr")),
                "lifecycle.uptime_s" => {
                    Some(PropDescribe::leaf(p.clone(), PropType::Number, "uptime"))
                }
                _ => None,
            }
        }
    }

    #[test]
    fn dispatch_get_root() {
        let r = dispatch_props(&T, "get", None, false);
        assert_eq!(r.rc, 0);
        let v: Json = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["config"]["bind"], "a:1");
        assert_eq!(v["lifecycle"]["uptime_s"], 7.0);
    }

    #[test]
    fn dispatch_get_path() {
        let args = json!({"path": "config.bind"});
        let r = dispatch_props(&T, "get", Some(&args), false);
        assert_eq!(r.rc, 0);
        assert_eq!(r.body, "\"a:1\"");
    }

    #[test]
    fn dispatch_list() {
        let r = dispatch_props(&T, "list", None, false);
        assert_eq!(r.rc, 0);
        let v: Json = serde_json::from_str(&r.body).unwrap();
        let paths: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(paths.contains(&"config.bind"));
        assert!(paths.contains(&"lifecycle.uptime_s"));
    }

    #[test]
    fn dispatch_describe() {
        let args = json!({"path": "config.bind"});
        let r = dispatch_props(&T, "describe", Some(&args), false);
        assert_eq!(r.rc, 0);
        let v: Json = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["path"], "config.bind");
        assert_eq!(v["type"], "string");
    }

    #[test]
    fn dispatch_unknown_path_errors() {
        let args = json!({"path": "nope"});
        let r = dispatch_props(&T, "get", Some(&args), false);
        assert_eq!(r.rc, 10);
        let r = dispatch_props(&T, "describe", Some(&args), false);
        assert_eq!(r.rc, 10);
    }

    #[test]
    fn dispatch_unknown_subcommand_errors() {
        let r = dispatch_props(&T, "frobnicate", None, false);
        assert_eq!(r.rc, 10);
    }
}
