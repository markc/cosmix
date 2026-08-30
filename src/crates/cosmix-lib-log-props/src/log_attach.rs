//! Citizen-only watcher: read the `log` substrate row, swap the live
//! `EnvFilter`, then keep swapping on every accepted change.
//!
//! Plan §4.1 frozen semantics:
//!   - A malformed filter on the wire is rejected by `before_set` in
//!     `log_namespace.rs`; the live filter is never touched for invalid
//!     input.
//!   - The watcher reads the *committed* row (post-hook), so any
//!     directive that reaches the swap path already parses.
//!   - `level=none` is a valid runtime mutation — it installs the
//!     equivalent of `EnvFilter::new("off")` which drops every event
//!     but leaves the subscriber installed.
//!   - The watcher logs a single `info` event naming the moment of
//!     takeover so the bootstrap-to-props seam is visible in the
//!     timeline.

use std::sync::Arc;

use anyhow::Result;
use cosmix_log::LogReloadHandle;
use cosmix_props::record::RecordKey;
use cosmix_props::runtime::Runtime;
use cosmix_props::store::StoreError;
use cosmix_props::value::PropValue;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

use crate::log_namespace::{LogPropsRuntime, namespace_name};

/// Attach the live `<svc>.log` watcher to an initialised `cosmix_log`
/// subscriber.
///
/// Reads the current substrate row (if any), applies it once, then
/// spawns a watcher task that re-applies on every accepted change.
/// Returns once the initial swap is done.
///
/// If the supplied `LogHandle` has no reload handle (which never happens
/// on a successful `cosmix_log::init` — `init` always installs a
/// subscriber, even for `--log-level none`), this is a no-op and returns
/// `Ok(())`.
///
/// **Watcher lifecycle.** The spawned task takes a clone of the
/// `Arc<Runtime>`, so dropping the caller's handle does not stop the
/// task — it lives until the tokio runtime itself shuts down. This
/// matches the intended use: a citizen daemon holds its router and
/// runtime for process lifetime, and the watcher loop is meant to
/// run for that same lifetime. A `Weak<Runtime>` + `JoinHandle`
/// shape that lets callers explicitly retire the watcher is a P3+
/// extension once a use case for it appears (e.g. test harnesses
/// that spin runtimes up and down).
pub async fn attach_props(handle: &cosmix_log::LogHandle, runtime: LogPropsRuntime) -> Result<()> {
    let Some(reload) = handle.reload_handle() else {
        return Ok(());
    };
    attach(runtime, reload).await
}

/// Read the current row (if any), apply it once, then spawn a watcher
/// task that re-applies on every notify wake-up. Returns once the
/// initial swap is done.
async fn attach(runtime: LogPropsRuntime, handle: LogReloadHandle) -> Result<()> {
    // Destructure the bundle: `rt` for reads, `signal` is the
    // namespace-private wake pulsed by the `after_set` hook. We must use
    // THIS notify, never `rt.events_signal()` — that one is drained by
    // the props dispatcher and the watcher would race it.
    let LogPropsRuntime {
        runtime: rt,
        signal,
    } = runtime;
    let initial = read_filter(&rt).await?;
    if let Some(filter) = initial {
        apply_filter(&handle, filter);
        tracing::info!(
            target: "cosmix_log",
            "log namespace attached — Phase B (props-driven) filter installed"
        );
    } else {
        tracing::info!(
            target: "cosmix_log",
            "log namespace attached — no substrate row yet, Phase A bootstrap filter retained"
        );
    }
    tokio::spawn(watch_loop(rt, handle, signal));
    Ok(())
}

async fn watch_loop(runtime: Arc<Runtime>, handle: LogReloadHandle, signal: Arc<Notify>) {
    loop {
        signal.notified().await;
        match read_filter(&runtime).await {
            Ok(Some(filter)) => {
                apply_filter(&handle, filter);
            }
            Ok(None) => {
                // Row missing — keep the previous filter installed. A
                // singleton row cannot be deleted (log_namespace.rs
                // `before_delete` refuses), so reaching here means the
                // wakeup raced a Replace that committed-then-rolled-back
                // before the substrate window we read. The next event
                // will re-apply.
            }
            Err(e) => {
                // Don't kill the watcher on transient read failures —
                // the next commit's notify wakes us again. We can't
                // even log this through `tracing` reliably if the read
                // failure is itself caused by the same substrate hiccup
                // taking down logging, so emit at debug only.
                tracing::debug!(
                    target: "cosmix_log",
                    error = %e,
                    "log namespace watcher: substrate read failed; will retry on next event"
                );
            }
        }
    }
}

async fn read_filter(runtime: &Runtime) -> Result<Option<EnvFilter>> {
    let key = RecordKey::singleton(namespace_name());
    match runtime.store().get(&key).await {
        Ok(snap) => Ok(extract_filter(&snap.value.value)),
        Err(StoreError::NotFound) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("log namespace watcher read failed: {e}")),
    }
}

/// Build an `EnvFilter` from the substrate row's `level` + `filter`
/// fields. Returns `None` only if the value is not an object at all
/// (defensive against hand-edited rows). Missing or non-string
/// `level`/`filter` fields fall back to the schema defaults
/// (`level=info`, `filter=""`) so a PATCH-style first write
/// carrying only `{ "level": "debug" }` still produces a swap —
/// the substrate's merge_patch leaves omitted fields absent on the
/// stored row, it does not back-fill schema defaults at read time.
fn extract_filter(value: &PropValue) -> Option<EnvFilter> {
    let obj = value.as_object()?;
    let level = obj
        .get("level")
        .and_then(|v| match v {
            PropValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("info");
    let filter = obj
        .get("filter")
        .and_then(|v| match v {
            PropValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("");
    let directive = compose_directive(level, filter);
    EnvFilter::try_new(&directive).ok()
}

fn compose_directive(level: &str, filter: &str) -> String {
    let level_directive = match level {
        "none" => "off",
        other => other,
    };
    if filter.is_empty() {
        level_directive.to_string()
    } else {
        format!("{level_directive},{filter}")
    }
}

fn apply_filter(handle: &LogReloadHandle, new_filter: EnvFilter) {
    // `reload_filter` fails only if the inner subscriber has been
    // dropped; since the registry is global and lives forever, this is
    // a can't-happen on the happy path. We log-and-swallow the error to
    // keep the watcher resilient — a failed swap leaves the prior
    // filter in place, which is the right failure mode.
    if let Err(e) = handle.reload_filter(new_filter) {
        tracing::debug!(
            target: "cosmix_log",
            error = %e,
            "log namespace watcher: live filter swap failed; prior filter retained"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn compose_directive_level_only() {
        assert_eq!(compose_directive("info", ""), "info");
        assert_eq!(compose_directive("none", ""), "off");
        assert_eq!(compose_directive("debug", ""), "debug");
    }

    #[test]
    fn compose_directive_level_and_filter() {
        assert_eq!(
            compose_directive("info", "cosmix_maild=debug"),
            "info,cosmix_maild=debug"
        );
        assert_eq!(
            compose_directive("none", "cosmix_log=info"),
            "off,cosmix_log=info"
        );
    }

    #[test]
    fn extract_filter_handles_well_formed_object() {
        let mut obj = BTreeMap::new();
        obj.insert("level".into(), PropValue::String("info".into()));
        obj.insert(
            "filter".into(),
            PropValue::String("cosmix_log=debug".into()),
        );
        let v = PropValue::Object(obj);
        assert!(extract_filter(&v).is_some());
    }

    #[test]
    fn extract_filter_rejects_non_object() {
        let v = PropValue::String("not an object".into());
        assert!(extract_filter(&v).is_none());
    }

    #[test]
    fn extract_filter_defaults_missing_filter_to_empty() {
        // Codex F2 regression: PATCH-style first write of just
        // `{level:"debug"}` lands a row without `filter`. The watcher
        // must still swap, defaulting filter="" rather than returning
        // None and silently keeping the bootstrap filter.
        let mut obj = BTreeMap::new();
        obj.insert("level".into(), PropValue::String("debug".into()));
        let v = PropValue::Object(obj);
        assert!(extract_filter(&v).is_some());
    }

    #[test]
    fn extract_filter_defaults_missing_level_to_info() {
        // Symmetric to the prior case: a PATCH of just
        // `{filter:"cosmix_log=debug"}` falls back to level=info.
        let mut obj = BTreeMap::new();
        obj.insert(
            "filter".into(),
            PropValue::String("cosmix_log=debug".into()),
        );
        let v = PropValue::Object(obj);
        assert!(extract_filter(&v).is_some());
    }
}
