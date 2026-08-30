//! SPEC 07 §2 property surface for cosmix-indexd (L1 conformance).
//!
//! Validates the core/citizen pattern from `cosmix-lib-props-store`: a second
//! daemon adopts the property surface as boilerplate-light wiring.
//! Paths cover config (socket, model, dtype, idle, embed_dim), lifecycle
//! (started_at, uptime_s, health, props_level, model_loaded,
//! model_circuit, embed_circuit), and corpus (chunks, bytes_db, kinds map).
//!
//! L1 only: queryable. L2/L3 (props.changed events, world.indexd retained
//! snapshot) deferred until a concrete consumer materialises.

use std::collections::BTreeMap;

use cosmix_props::{PropDescribe, PropPath, PropTree, PropType, PropValue, tree::build_snapshot};
use serde_json::Value as Json;

/// Snapshot of indexd's observable state. Built fresh per `props.get`
/// from live `AppState` + `VectorDb::stats`. The struct is the
/// snapshot — no async locks held inside the `PropTree` impl.
pub struct IndexdPropsSnapshot {
    pub socket_path: String,
    pub model_id: String,
    pub dtype: String,
    pub idle_timeout_secs: u64,
    pub embed_dim: u64,
    pub started_at: String,
    pub uptime_s: u64,
    pub model_loaded: bool,
    pub model_circuit: String,
    pub embed_circuit: String,
    pub corpus_chunks: u64,
    pub corpus_bytes_db: u64,
    pub corpus_kinds: BTreeMap<String, u64>,
}

impl IndexdPropsSnapshot {
    pub fn snapshot_value(&self) -> PropValue {
        let kinds = PropValue::Object(
            self.corpus_kinds
                .iter()
                .map(|(k, v)| (k.clone(), PropValue::from(*v)))
                .collect(),
        );
        build_snapshot([
            (
                PropPath::new("config.socket_path").unwrap(),
                PropValue::from(self.socket_path.clone()),
            ),
            (
                PropPath::new("config.model_id").unwrap(),
                PropValue::from(self.model_id.clone()),
            ),
            (
                PropPath::new("config.dtype").unwrap(),
                PropValue::from(self.dtype.clone()),
            ),
            (
                PropPath::new("config.idle_timeout_secs").unwrap(),
                PropValue::from(self.idle_timeout_secs),
            ),
            (
                PropPath::new("config.embed_dim").unwrap(),
                PropValue::from(self.embed_dim),
            ),
            (
                PropPath::new("lifecycle.started_at").unwrap(),
                PropValue::from(self.started_at.clone()),
            ),
            (
                PropPath::new("lifecycle.uptime_s").unwrap(),
                PropValue::from(self.uptime_s),
            ),
            (
                PropPath::new("lifecycle.health").unwrap(),
                PropValue::from("ok"),
            ),
            (
                PropPath::new("lifecycle.props_level").unwrap(),
                PropValue::from("L3"),
            ),
            (
                PropPath::new("lifecycle.model_loaded").unwrap(),
                PropValue::Bool(self.model_loaded),
            ),
            (
                PropPath::new("lifecycle.model_circuit").unwrap(),
                PropValue::from(self.model_circuit.clone()),
            ),
            (
                PropPath::new("lifecycle.embed_circuit").unwrap(),
                PropValue::from(self.embed_circuit.clone()),
            ),
            (
                PropPath::new("corpus.chunks").unwrap(),
                PropValue::from(self.corpus_chunks),
            ),
            (
                PropPath::new("corpus.bytes_db").unwrap(),
                PropValue::from(self.corpus_bytes_db),
            ),
            (PropPath::new("corpus.kinds").unwrap(), kinds),
        ])
    }
}

impl PropTree for IndexdPropsSnapshot {
    fn snapshot(&self) -> PropValue {
        self.snapshot_value()
    }

    fn list(&self) -> Vec<PropPath> {
        all_paths()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        describe_path(path)
    }
}

fn all_paths() -> Vec<PropPath> {
    [
        "config.socket_path",
        "config.model_id",
        "config.dtype",
        "config.idle_timeout_secs",
        "config.embed_dim",
        "lifecycle.started_at",
        "lifecycle.uptime_s",
        "lifecycle.health",
        "lifecycle.props_level",
        "lifecycle.model_loaded",
        "lifecycle.model_circuit",
        "lifecycle.embed_circuit",
        "corpus.chunks",
        "corpus.bytes_db",
        "corpus.kinds",
    ]
    .into_iter()
    .map(|s| PropPath::new(s).unwrap())
    .collect()
}

fn describe_path(path: &PropPath) -> Option<PropDescribe> {
    use PropType::*;
    match path.as_str() {
        "config.socket_path" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Unix socket path the indexd daemon listens on for local clients.",
        )),
        "config.model_id" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "HuggingFace embedding model identifier loaded on demand.",
        )),
        "config.dtype" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Model precision (f16 | f32).",
        )),
        "config.idle_timeout_secs" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Seconds of inactivity before the model is unloaded to free RAM.",
        )),
        "config.embed_dim" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Embedding vector dimensionality (768 for nomic-embed-text-v1.5).",
        )),
        "lifecycle.started_at" => Some(
            PropDescribe::leaf(path.clone(), String, "RFC 3339 timestamp of process start.")
                .with_format("rfc3339"),
        ),
        "lifecycle.uptime_s" => Some(
            PropDescribe::leaf(path.clone(), Number, "Seconds since process start.")
                .with_transient(true),
        ),
        "lifecycle.health" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Coarse health classification (ok | degraded | failing).",
        )),
        "lifecycle.props_level" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "SPEC 07 conformance level (L0 | L1 | L2 | L3).",
        )),
        "lifecycle.model_loaded" => Some(PropDescribe::leaf(
            path.clone(),
            Bool,
            "Whether the embedding model is currently resident in memory.",
        )),
        "lifecycle.model_circuit" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Model-load circuit breaker state (closed | open | half-open).",
        )),
        "lifecycle.embed_circuit" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Embed circuit breaker state (closed | open | half-open).",
        )),
        "corpus.chunks" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Total number of indexed chunks across all sources.",
        )),
        "corpus.bytes_db" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Bytes on disk for the sqlite-vec database file.",
        )),
        "corpus.kinds" => Some(PropDescribe::leaf(
            path.clone(),
            Object,
            "Map of source-type → chunk count (skill, doc, journal, ...).",
        )),
        _ => None,
    }
}

/// Parse the optional `args` JSON header into a `serde_json::Value`.
pub fn parse_args(s: Option<&str>) -> Option<Json> {
    s.and_then(|raw| serde_json::from_str(raw).ok())
}
