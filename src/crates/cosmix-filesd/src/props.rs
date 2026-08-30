//! The read-only SPEC-07 props surface for cosmix-filesd (`filesd.props.*`).
//!
//! An OWNED snapshot struct implements `PropTree` (the indexd pattern — no Store
//! handle / lock held inside the trait impl), built fresh per request from the
//! single index-writer's `Store`. Conformance is **L1**: `get`/`list`/`describe`
//! only — no `props.changed` events, no `props.watch`, no retained `world.*`.

use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};

use cosmix_files::store::CorpusStats;

/// An owned snapshot of the daemon's reportable state.
pub struct FilesdProps {
    pub corpus_id: String,
    pub root: String,
    pub started_at: String,
    pub uptime_s: u64,
    pub stats: CorpusStats,
}

impl FilesdProps {
    fn leaves(&self) -> Vec<(PropPath, PropValue)> {
        vec![
            (
                path("config.corpus_id"),
                PropValue::from(self.corpus_id.clone()),
            ),
            (path("config.root"), PropValue::from(self.root.clone())),
            (
                path("lifecycle.started_at"),
                PropValue::from(self.started_at.clone()),
            ),
            (path("lifecycle.uptime_s"), PropValue::from(self.uptime_s)),
            (
                path("lifecycle.props_level"),
                PropValue::from("L1".to_string()),
            ),
            (path("corpus.files"), PropValue::from(self.stats.files)),
            (
                path("corpus.conflicts"),
                PropValue::from(self.stats.conflicts),
            ),
            (
                path("corpus.unmanaged"),
                PropValue::from(self.stats.unmanaged),
            ),
            (path("corpus.bytes"), PropValue::from(self.stats.bytes)),
            // modseq as a STRING leaf: PropValue numeric variants serialize as JSON
            // numbers → f64 precision loss for a large tip on a Mix/JS consumer.
            (
                path("corpus.modseq"),
                PropValue::from(self.stats.modseq.to_string()),
            ),
        ]
    }
}

impl PropTree for FilesdProps {
    fn snapshot(&self) -> PropValue {
        build_snapshot(self.leaves())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves().into_iter().map(|(p, _)| p).collect()
    }

    fn describe(&self, p: &PropPath) -> Option<PropDescribe> {
        use PropType::{Number, String as Str};
        let d = match p.as_str() {
            "config.corpus_id" => PropDescribe::leaf(
                p.clone(),
                Str,
                "Corpus identifier (one index.db per corpus).",
            ),
            "config.root" => {
                PropDescribe::leaf(p.clone(), Str, "Corpus root directory watched on disk.")
            }
            "lifecycle.started_at" => {
                PropDescribe::leaf(p.clone(), Str, "RFC 3339 process start time.")
                    .with_format("rfc3339")
            }
            "lifecycle.uptime_s" => {
                PropDescribe::leaf(p.clone(), Number, "Seconds since process start.")
                    .with_transient(true)
            }
            "lifecycle.props_level" => {
                PropDescribe::leaf(p.clone(), Str, "SPEC 07 conformance level.")
            }
            "corpus.files" => {
                PropDescribe::leaf(p.clone(), Number, "Live (non-tombstoned) documents.")
            }
            "corpus.conflicts" => {
                PropDescribe::leaf(p.clone(), Number, "Syncthing whole-file conflicts seen.")
            }
            "corpus.unmanaged" => PropDescribe::leaf(
                p.clone(),
                Number,
                "Live id-less files a passive scan won't mint.",
            ),
            "corpus.bytes" => {
                PropDescribe::leaf(p.clone(), Number, "Sum of live document size_bytes.")
            }
            "corpus.modseq" => PropDescribe::leaf(
                p.clone(),
                Str,
                "Current per-corpus modseq tip (string — f64-precision-safe).",
            ),
            _ => return None,
        };
        Some(d)
    }
}

/// Build a `PropPath` from a static, known-valid literal.
fn path(s: &str) -> PropPath {
    PropPath::new(s).expect("static prop path literal is valid")
}
