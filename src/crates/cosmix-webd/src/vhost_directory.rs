//! Hot-swappable host-routing directory for webd.
//!
//! The directory is a single immutable snapshot containing every
//! host-derived view the daemon needs against the SPEC-12
//! `webd.vhosts` namespace:
//!
//! * `by_host` — lowercase-Host → `Arc<VhostState>`. Aliases share
//!   the same Arc as their primary, mirroring `[[webd.vhost]]`'s
//!   resolve-time alias-fan-out.
//! * `admit_plain_http` — the set of names the :80 redirect branch
//!   admits.
//! * `primaries` — one [`VhostDirectoryPrimary`] per `Arc<VhostState>`,
//!   sorted by primary FQDN, each carrying the row's alias list (also
//!   sorted, primary excluded). `webd.routes.list` and `webd.stats`
//!   iterate this list directly so Bus read paths pay no per-call
//!   dedup or alias-grouping cost.
//!
//! ## Why an immutable snapshot
//!
//! `NodeState::vhosts` is `Arc<ArcSwap<VhostDirectory>>` so the
//! provisioner (C4) and the C5 ergonomic verbs can publish a new
//! directory atomically — every reader (HTTPS dispatch, plain-HTTP
//! admit, Bus read verbs) sees a consistent view. Pre-computing the
//! three views inside the snapshot avoids "directory swapped between
//! read of `by_host` and read of `keys()`" race windows.
//!
//! ## Source — the substrate, not the config block (C3e)
//!
//! [`from_namespace_rows`] is the single source-of-truth constructor:
//! it consumes the post-bootstrap `webd.vhosts` snapshot (see
//! [`crate::vhosts_namespace::snapshot_rows`]) plus the
//! `resolve_node_state` runtime map, and produces a `VhostDirectory`.
//! Config-matched rows reuse the resolved `Arc<VhostState>` (so CMS /
//! JMAP / docs surfaces remain configured); orphan `bus_runtime` rows
//! (operator-added via `vhost.add`, no matching `[[webd.vhost]]` after
//! restart) get a minimal `VhostState` constructed from namespace
//! fields only (`fqdn`, `www_dir`) with CMS / JMAP / noded / docs all
//! `None` — the substrate-honest contract: the surface that lives in
//! the namespace is what the namespace materialises. See
//! `_doc/planned/webd-vhosts-phase3.md` § *Commit 3* for the full
//! ordering.
//!
//! ## Why `build` stays source-agnostic
//!
//! `VhostDirectory::build` takes `Vec<VhostDirectoryEntry>` rather
//! than reading the namespace directly. Two callers need it:
//!   * Startup: [`from_namespace_rows`] materialises entries from the
//!     post-bootstrap namespace snapshot.
//!   * Runtime republish (planned C4b): the provisioner rebuilds a
//!     fresh directory on each `VhostSet` / `VhostRemoved` event using
//!     the same entry shape.
//!
//! Keeping `build` source-agnostic lets the two paths share invariants
//! (duplicate-host rejection, sort order) without the directory
//! crate-depending on SPEC-12 internals.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::VhostState;
use crate::stats;
use crate::vhosts_namespace::VhostRow;

/// One vhost-row's worth of input to [`VhostDirectory::build`]. The
/// primary FQDN lives on `state.fqdn` (the canonical home — set once
/// at `VhostState` construction in `resolve_node_state`), so the entry
/// only carries the alias list separately.
///
/// `aliases` MUST be pre-normalised (lowercased, port-stripped, LDH-
/// validated) by the caller. `build` does not re-normalise; the
/// duplicate-host scan compares raw strings and a non-normalised input
/// is a caller bug, not a directory bug. `resolve_node_state` already
/// runs every name through `cosmix_daemon::http_host::parse_request_host`
/// and the C3c bootstrap path will do the same before materialising
/// entries from the namespace.
#[derive(Debug, Clone)]
pub struct VhostDirectoryEntry {
    pub state: Arc<VhostState>,
    pub aliases: Vec<String>,
}

/// One per-primary entry in [`VhostDirectory::primaries`]. Carries
/// both the per-vhost runtime state and the alias list that the Bus
/// read verbs surface. Aliases are sorted ascending with the primary
/// excluded — matching today's `webd.routes.list` output shape
/// (`bus::routes::list_body`).
#[derive(Debug, Clone)]
pub struct VhostDirectoryPrimary {
    pub state: Arc<VhostState>,
    pub aliases: Vec<String>,
}

/// Immutable host-routing snapshot. Three pre-computed views over the
/// same `Vec<VhostDirectoryEntry>` input. See module docs.
#[derive(Debug)]
pub struct VhostDirectory {
    /// Lowercase-Host → `Arc<VhostState>`. Primary and aliases both
    /// resolve to the same Arc.
    pub by_host: HashMap<String, Arc<VhostState>>,
    /// Names the plain-HTTP (:80) redirect branch admits. Equals
    /// `by_host.keys().cloned().collect()` at construction time;
    /// kept as a pre-built set so the admit middleware does not pay
    /// a clone per request.
    pub admit_plain_http: HashSet<String>,
    /// One entry per `Arc<VhostState>`, sorted by primary FQDN.
    /// Each entry carries the row's alias list so `webd.routes.list`
    /// can emit `{host, aliases}` without rescanning `by_host`.
    /// Aliases share the Arc with their primary and do not appear
    /// here themselves.
    pub primaries: Vec<VhostDirectoryPrimary>,
}

impl VhostDirectory {
    /// Empty directory — no vhosts registered. Lookup returns `None`
    /// for every host. Used as the initial `ArcSwap` value before the
    /// first `publish` lands, and by tests that need a no-op state.
    pub fn empty() -> Self {
        Self {
            by_host: HashMap::new(),
            admit_plain_http: HashSet::new(),
            primaries: Vec::new(),
        }
    }

    /// Build a directory from a list of entries.
    ///
    /// Rejects:
    ///   * Two entries sharing the same primary FQDN
    ///     (`state.fqdn` collision).
    ///   * Any name (primary OR alias) registered twice across the
    ///     input — including a row's primary colliding with another
    ///     row's alias, or its own alias.
    ///
    /// Mirrors the startup-time guard in `resolve_node_state` so the
    /// directory's invariants are independent of whichever path
    /// produced the entries.
    pub fn build(entries: Vec<VhostDirectoryEntry>) -> Result<Self> {
        let mut by_host: HashMap<String, Arc<VhostState>> = HashMap::new();
        let mut seen_primaries: HashSet<String> = HashSet::new();
        let mut primaries: Vec<VhostDirectoryPrimary> = Vec::with_capacity(entries.len());

        for entry in entries {
            let primary = entry.state.fqdn.clone();
            if !seen_primaries.insert(primary.clone()) {
                return Err(anyhow!(
                    "duplicate vhost primary {:?} — two entries claim the same primary FQDN",
                    primary
                ));
            }

            // Primary first, then aliases. `insert` returning `Some`
            // means this name was already registered — either by an
            // earlier entry (its primary or one of its aliases) or by
            // this same entry (primary matching one of its own
            // aliases). All three cases are startup errors.
            if by_host
                .insert(primary.clone(), entry.state.clone())
                .is_some()
            {
                return Err(anyhow!(
                    "duplicate host {:?} — already registered by an earlier vhost entry or as \
                     an alias",
                    primary
                ));
            }
            for alias in &entry.aliases {
                if by_host.insert(alias.clone(), entry.state.clone()).is_some() {
                    return Err(anyhow!(
                        "duplicate host {:?} — already registered (alias of {:?} collides with \
                         an earlier entry or this row's primary)",
                        alias,
                        primary
                    ));
                }
            }

            // Sorted, primary excluded — matches bus::routes::list_body
            // exactly so C3b's migration is a pure swap.
            let mut aliases = entry.aliases;
            aliases.sort();
            primaries.push(VhostDirectoryPrimary {
                state: entry.state,
                aliases,
            });
        }

        primaries.sort_by(|a, b| a.state.fqdn.cmp(&b.state.fqdn));
        let admit_plain_http: HashSet<String> = by_host.keys().cloned().collect();

        Ok(Self {
            by_host,
            admit_plain_http,
            primaries,
        })
    }

    /// Lookup a Host (caller pre-normalised, lowercase, port stripped)
    /// against the routing map. Returns `None` if no vhost claims this
    /// host. Used by the HTTPS dispatch middleware and (via the admit
    /// set) by the plain-HTTP redirect branch.
    pub fn vhost_for_host(&self, host: &str) -> Option<Arc<VhostState>> {
        self.by_host.get(host).cloned()
    }
}

/// Build the host-routing directory from a post-bootstrap `webd.vhosts`
/// namespace snapshot.
///
/// **Source of truth.** This is C3e's substrate-honest directory
/// constructor: the namespace IS the canonical vhost set, and the
/// directory mirrors exactly what it carries — config-block presence
/// is no longer load-bearing for routing. Operators who add a vhost
/// via `webd.vhost.add` (the `bus_runtime` source) survive a daemon
/// restart even with no matching `[[webd.vhost]]` block in node.conf.mix.
///
/// **Two row classes, one directory — classified by `row.source`.**
///
/// The SoT for routing classification is the daemon-stamped `source`
/// field on the namespace row, NOT FQDN-in-map presence. The C3c
/// bootstrap path treats `bus_runtime` rows as winning over a
/// matching `[[webd.vhost]]` block (the runtime row preserves
/// operator intent and the bootstrap logs a "conflict skip" WARN
/// without overwriting). C3e must mirror that precedence on the
/// routing side, otherwise the directory would happily route the
/// config-derived `Arc<VhostState>` while the namespace row still
/// carries divergent `www_dir`/intent.
///
/// * **`source = "config_bootstrap"`** — row materialised from
///   `[[webd.vhost]]` at bootstrap. The matching `Arc<VhostState>`
///   MUST be in `resolved_runtime_map` (C3d's orphan pass deletes
///   any `config_bootstrap` row whose `fqdn` is no longer in
///   `node.conf.mix`). We reuse the resolved Arc verbatim so CMS / JMAP
///   / noded WS / docs surfaces remain configured. Aliases are
///   recovered from the runtime map by walking other keys that
///   point to the same Arc — this handles the
///   `[[webd.vhost]] aliases = [...]` case that the namespace's
///   `aliases` field currently rejects (Phase 4 multi-SAN). A
///   missing-from-map `config_bootstrap` row is a substrate
///   invariant violation → `Err`, not silent-synthesize.
///
/// * **`source = "bus_runtime"`** — operator-added via `vhost.add`
///   (or `props.set namespace=vhosts`). We synthesize a minimal
///   `VhostState` from namespace fields only:
///   `fqdn = row.fqdn`, `www_dir = PathBuf::from(row.www_dir)`,
///   everything else `None` — **regardless** of whether a matching
///   `[[webd.vhost]]` block also exists. Static-file serving works;
///   `/api/posts*`, `/jmap`, `/ws`, `/docs` return 404 on this
///   vhost — the namespace simply doesn't carry those paths.
///   Operators who need those surfaces add them via the config
///   block then drop the `bus_runtime` row.
///
/// * **`source` absent** — pre-Phase-3 row, or a row not yet
///   stamped by a backend-origin write. Treat as
///   `config_bootstrap` for routing purposes: the resolved runtime
///   map is the only authoritative source for runtime wiring, so
///   FQDN-present-in-map reuses the Arc; FQDN-absent is the same
///   missing-map invariant violation. C3d's bootstrap will stamp
///   `source` on next run.
///
/// **Defensive `www_dir` validation on `bus_runtime` rows.**
/// Config-matched rows already had `www_dir` directory-existence
/// verified by `resolve_node_state` at startup. `bus_runtime` rows
/// haven't — they were written through the namespace's `before_set`
/// hook, which doesn't hit the filesystem. If the path is missing or
/// not a directory, we `tracing::error!` and **skip the row from the
/// directory** rather than register a vhost whose every request
/// would 5xx. The namespace row is preserved (the operator's stored
/// intent stays), so the remedy is operator-side `mkdir` or
/// `webd.vhost.update`.
///
/// **`enabled = false` rows are skipped from routing.** The schema
/// help text says "Disabled vhosts skip dispatch and skip ACME ticks";
/// C3e is where the directory enforces that. The namespace row stays;
/// only the routing surface drops out.
///
/// All remaining duplicate-host invariants (primary collision,
/// alias collision) flow through [`VhostDirectory::build`] for
/// consistency with the C4b runtime republish path.
/// `disabled_hosts` (B1 fail-soft) is every name — primary and alias —
/// that `resolve_node_state` dropped because its per-vhost validation
/// failed (bad cert, missing www_dir, unopenable CMS DB). Such a host
/// has a `config_bootstrap` namespace row (bootstrap writes the full
/// configured set, deliberately un-filtered, so the row survives to
/// resurrect on repair) but is **absent** from `resolved_runtime_map`.
/// Without this set that absence trips the invariant bail below; with
/// it, a known-disabled config row is skipped from the directory (its
/// routing surface drops out, its namespace row stays). An empty set
/// restores the strict pre-B1 behaviour.
pub fn from_namespace_rows(
    rows: &[VhostRow],
    resolved_runtime_map: &HashMap<String, Arc<VhostState>>,
    disabled_hosts: &HashSet<String>,
) -> Result<VhostDirectory> {
    // Pre-index the runtime map: primary FQDN → alias list. Walking
    // once is O(n); the per-row lookup that follows is O(1). Aliases
    // are recovered by name-set subtraction against the registered
    // keys, same trick `resolve_node_state`'s fanout uses.
    let mut aliases_by_primary: HashMap<String, Vec<String>> = HashMap::new();
    for (name, state) in resolved_runtime_map {
        if name != &state.fqdn {
            aliases_by_primary
                .entry(state.fqdn.clone())
                .or_default()
                .push(name.clone());
        }
    }

    let mut entries: Vec<VhostDirectoryEntry> = Vec::with_capacity(rows.len());
    for row in rows {
        // Disabled rows: namespace state preserved, routing surface
        // dropped. See module docs.
        if !row.enabled {
            continue;
        }

        let source = row.source.as_deref();
        let synthesize_from_namespace = matches!(source, Some("bus_runtime"));

        let (state, aliases) = if synthesize_from_namespace {
            // `bus_runtime` row wins, even if a matching
            // `[[webd.vhost]]` block also produced a runtime-map
            // entry. The namespace row carries the operator's
            // intent (`www_dir`, future fields) and C3c's bootstrap
            // path already deferred to it; the routing layer must
            // do the same. Validate `www_dir` exists; skip
            // (with ERROR log) on miss so the namespace row stays
            // operator-repairable but the directory doesn't
            // register a vhost whose every request would 5xx.
            let www_dir = PathBuf::from(&row.www_dir);
            if !www_dir.is_dir() {
                tracing::error!(
                    target: "webd::vhost_directory",
                    fqdn = %row.fqdn,
                    source = ?row.source,
                    www_dir = %row.www_dir,
                    "bus_runtime row's www_dir does not exist on disk; \
                     skipping from VhostDirectory. The namespace row is \
                     preserved; repair via webd.vhost.update or operator-side \
                     mkdir then restart webd.",
                );
                continue;
            }
            let state = Arc::new(VhostState {
                fqdn: row.fqdn.clone(),
                db: None,
                www_dir,
                jmap_upstream: None,
                noded_ws: None,
                docs_dir: None,
                dev_session_email: None,
                dev_session_password: None,
                public_read_email: None,
                public_read_password: None,
                system_sender_email: None,
                system_sender_password: None,
                mfa_break_glass: false,
                stats: Arc::new(stats::WebdStats::new()),
                session_epoch_cache: crate::SessionEpochCache::default(),
                public_response_cache: crate::public_response_cache::Cache::default(),
            });
            // Namespace-carried aliases — today always empty per
            // `before_set` rule 3 (multi-SAN reserved for Phase 4),
            // but threaded through unchanged so the future schema
            // bump is a one-line lift.
            (state, row.aliases.clone())
        } else {
            // `config_bootstrap` (or pre-Phase-3 unstamped row). The
            // matching `Arc<VhostState>` must be in the resolved
            // runtime map — C3d's orphan pass deletes stale config
            // rows whose `fqdn` is no longer in `node.conf.mix`, and a
            // current config row is always materialised into the
            // runtime map by `resolve_node_state`. Missing-from-map
            // is a substrate-invariant violation, not a silent
            // synthesize.
            let Some(existing) = resolved_runtime_map.get(&row.fqdn) else {
                // B1 fail-soft: a config row whose fqdn was dropped by
                // `resolve_node_state` (bad cert / missing www_dir /
                // unopenable CMS DB) is intentionally absent from the
                // runtime map but still carries a `config_bootstrap`
                // namespace row. Skip it from the directory (routing
                // surface drops out; the row stays so it resurrects on
                // repair) rather than treating the absence as an
                // invariant violation.
                if disabled_hosts.contains(&row.fqdn) {
                    tracing::info!(
                        target: "webd::vhost_directory",
                        fqdn = %row.fqdn,
                        "config_bootstrap row is fail-soft-disabled \
                         (per-vhost validation failed at resolve); skipping \
                         from VhostDirectory — namespace row preserved",
                    );
                    continue;
                }
                return Err(anyhow!(
                    "webd.vhosts row fqdn={:?} carries source={:?} but is \
                     absent from the resolve_node_state runtime map. This \
                     is a substrate-invariant violation: C3d's orphan \
                     pass should have deleted stale config_bootstrap rows \
                     whose fqdn is no longer in node.conf.mix, and a current \
                     config row is always materialised into the runtime \
                     map. Refusing to build the directory from an \
                     inconsistent snapshot.",
                    row.fqdn,
                    row.source,
                ));
            };
            let aliases = aliases_by_primary
                .get(&row.fqdn)
                .cloned()
                .unwrap_or_default();
            (existing.clone(), aliases)
        };

        entries.push(VhostDirectoryEntry { state, aliases });
    }

    VhostDirectory::build(entries)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::VhostState;
    use crate::stats;

    use super::*;

    fn make_state(fqdn: &str) -> Arc<VhostState> {
        Arc::new(VhostState {
            fqdn: fqdn.to_string(),
            db: None,
            www_dir: PathBuf::from("/srv/www"),
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: crate::SessionEpochCache::default(),
            public_response_cache: crate::public_response_cache::Cache::default(),
        })
    }

    #[test]
    fn empty_directory_is_no_op() {
        let d = VhostDirectory::empty();
        assert!(d.by_host.is_empty());
        assert!(d.admit_plain_http.is_empty());
        assert!(d.primaries.is_empty());
        assert!(d.vhost_for_host("any.example").is_none());
    }

    #[test]
    fn build_coherence_one_row_no_aliases() {
        // Single row, no aliases — by_host has 1 entry, admit set has
        // 1 entry, primaries has 1 entry, and the three views agree.
        let s = make_state("site-a.example");
        let d = VhostDirectory::build(vec![VhostDirectoryEntry {
            state: s.clone(),
            aliases: vec![],
        }])
        .unwrap();

        assert_eq!(d.by_host.len(), 1);
        assert!(Arc::ptr_eq(
            &d.vhost_for_host("site-a.example").unwrap(),
            &s
        ));
        assert_eq!(d.admit_plain_http.len(), 1);
        assert!(d.admit_plain_http.contains("site-a.example"));
        assert_eq!(d.primaries.len(), 1);
        assert!(Arc::ptr_eq(&d.primaries[0].state, &s));
        assert!(d.primaries[0].aliases.is_empty());
    }

    #[test]
    fn build_coherence_aliases_share_arc() {
        // Aliases must resolve to the same Arc as the primary —
        // mirrors the startup-time alias-fanout in `resolve_node_state`
        // and is the contract `webd.routes.list` relies on for the
        // dedup-by-Arc-identity grouping it inherited from the
        // HashMap-shaped predecessor.
        let s = make_state("site-a.example");
        let d = VhostDirectory::build(vec![VhostDirectoryEntry {
            state: s.clone(),
            aliases: vec!["www.site-a.example".into(), "alt.site-a.example".into()],
        }])
        .unwrap();

        assert_eq!(d.by_host.len(), 3);
        for name in ["site-a.example", "www.site-a.example", "alt.site-a.example"] {
            let got = d.vhost_for_host(name).unwrap_or_else(|| {
                panic!("expected {name} to resolve to the primary's VhostState")
            });
            assert!(
                Arc::ptr_eq(&got, &s),
                "alias {name} should share primary Arc"
            );
        }
        // The dedup is reflected in `primaries`: one entry even though
        // three names are registered.
        assert_eq!(d.primaries.len(), 1);
        assert_eq!(d.admit_plain_http.len(), 3);
    }

    #[test]
    fn build_primaries_sorted_by_fqdn() {
        // The :80 admit set is order-insensitive (HashSet) but
        // `primaries` feeds deterministic Bus outputs
        // (`webd.routes.list`, `webd.stats`). Sort by primary FQDN so
        // cross-node comparison doesn't fight HashMap iteration.
        let a = make_state("a.example");
        let b = make_state("b.example");
        let c = make_state("c.example");

        // Feed them out of order — the directory must sort.
        let d = VhostDirectory::build(vec![
            VhostDirectoryEntry {
                state: c.clone(),
                aliases: vec![],
            },
            VhostDirectoryEntry {
                state: a.clone(),
                aliases: vec![],
            },
            VhostDirectoryEntry {
                state: b.clone(),
                aliases: vec![],
            },
        ])
        .unwrap();

        let fqdns: Vec<&str> = d.primaries.iter().map(|p| p.state.fqdn.as_str()).collect();
        assert_eq!(fqdns, vec!["a.example", "b.example", "c.example"]);
    }

    #[test]
    fn build_primaries_carry_sorted_aliases() {
        // The contract C3b will rely on for `webd.routes.list`: each
        // primary entry carries its aliases (primary excluded, sorted
        // ascending), matching today's bus::routes::list_body shape
        // exactly. Codex C3a-MAJOR catch: without this, C3b has to
        // rescan by_host or re-group by Arc-identity, both of which
        // defeat the "precomputed view" promise.
        let a = make_state("a.example");
        let b = make_state("b.example");
        let d = VhostDirectory::build(vec![
            VhostDirectoryEntry {
                state: b.clone(),
                // Out-of-order aliases — must sort.
                aliases: vec!["www.b.example".into(), "alt.b.example".into()],
            },
            VhostDirectoryEntry {
                state: a.clone(),
                aliases: vec!["www.a.example".into()],
            },
        ])
        .unwrap();

        assert_eq!(d.primaries.len(), 2);
        // Primaries sorted by FQDN: a before b.
        assert_eq!(d.primaries[0].state.fqdn, "a.example");
        assert_eq!(d.primaries[0].aliases, vec!["www.a.example".to_string()]);
        assert_eq!(d.primaries[1].state.fqdn, "b.example");
        assert_eq!(
            d.primaries[1].aliases,
            vec!["alt.b.example".to_string(), "www.b.example".to_string()]
        );
        // Primary's own FQDN is not in the alias list.
        for p in &d.primaries {
            assert!(
                !p.aliases.contains(&p.state.fqdn),
                "primary FQDN must not appear in its own alias list"
            );
        }
    }

    #[test]
    fn build_rejects_duplicate_primary() {
        // Two entries with the same primary FQDN — distinct Arcs but
        // identical fqdn. Even though the by_host clash would also
        // catch this, the primary-set check fires first with a more
        // specific error so callers diagnose the *intent* not the
        // symptom.
        let a = make_state("site.example");
        let b = make_state("site.example");
        let err = VhostDirectory::build(vec![
            VhostDirectoryEntry {
                state: a,
                aliases: vec![],
            },
            VhostDirectoryEntry {
                state: b,
                aliases: vec![],
            },
        ])
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate vhost primary"),
            "expected primary-collision diagnostic, got: {msg}"
        );
        assert!(msg.contains("site.example"));
    }

    #[test]
    fn build_rejects_primary_colliding_with_earlier_alias() {
        // Row 1 registers `www.site.example` as an alias of
        // `site-a.example`; row 2 then tries to use
        // `www.site.example` as its primary. Must fail with the
        // by_host duplicate-host diagnostic.
        let row1 = make_state("site-a.example");
        let row2 = make_state("www.site.example");
        let err = VhostDirectory::build(vec![
            VhostDirectoryEntry {
                state: row1,
                aliases: vec!["www.site.example".into()],
            },
            VhostDirectoryEntry {
                state: row2,
                aliases: vec![],
            },
        ])
        .unwrap_err();
        assert!(err.to_string().contains("duplicate host"));
        assert!(err.to_string().contains("www.site.example"));
    }

    #[test]
    fn build_rejects_alias_colliding_with_earlier_primary() {
        // Reverse of the above: row 2 lists row 1's primary as one of
        // its aliases. The by_host insert for the alias collides.
        let row1 = make_state("site-a.example");
        let row2 = make_state("site-b.example");
        let err = VhostDirectory::build(vec![
            VhostDirectoryEntry {
                state: row1,
                aliases: vec![],
            },
            VhostDirectoryEntry {
                state: row2,
                aliases: vec!["site-a.example".into()],
            },
        ])
        .unwrap_err();
        assert!(err.to_string().contains("duplicate host"));
        assert!(err.to_string().contains("site-a.example"));
    }

    #[test]
    fn build_rejects_row_primary_matching_own_alias() {
        // A row that lists its own primary as an alias is a config
        // bug; resolve_node_state's startup guard rejects it and the
        // directory must too.
        let s = make_state("site.example");
        let err = VhostDirectory::build(vec![VhostDirectoryEntry {
            state: s,
            aliases: vec!["site.example".into()],
        }])
        .unwrap_err();
        assert!(err.to_string().contains("duplicate host"));
        assert!(err.to_string().contains("site.example"));
    }

    /// Helper: minimal `VhostRow` with the row's primary identifier
    /// (`fqdn`), `www_dir`, and `source` set, everything else at
    /// namespace defaults. `source` is the load-bearing
    /// classification field — `config_bootstrap` rows reuse the
    /// resolved `Arc<VhostState>` from the runtime map,
    /// `bus_runtime` rows synthesise their own (regardless of map
    /// presence).
    fn make_row(fqdn: &str, www_dir: &str, source: &str) -> VhostRow {
        VhostRow {
            fqdn: fqdn.into(),
            enabled: true,
            www_dir: www_dir.into(),
            aliases: Vec::new(),
            source: Some(source.into()),
            acme_provider: None,
            acme_challenge: None,
            acme_contact_email: None,
            tls_cert_path: None,
            tls_key_path: None,
            cert_blob_id: None,
            key_blob_id: None,
            not_after: None,
            last_attempt: None,
            last_error_count: None,
            last_error: None,
        }
    }

    #[test]
    fn from_namespace_rows_empty_input_empty_directory() {
        // No rows in the namespace + an empty runtime map → empty
        // directory. The startup ordering allows this: a fresh node
        // with no `[[webd.vhost]]` config block and no operator
        // `vhost.add` calls yet.
        let d = from_namespace_rows(&[], &HashMap::new(), &HashSet::new()).unwrap();
        assert!(d.by_host.is_empty());
        assert!(d.admit_plain_http.is_empty());
        assert!(d.primaries.is_empty());
    }

    #[test]
    fn from_namespace_rows_config_bootstrap_reuses_resolved_arc() {
        // `source = "config_bootstrap"`: the directory must reuse
        // the `Arc<VhostState>` from the runtime map so CMS / JMAP
        // / noded / docs surfaces configured via `[[webd.vhost]]`
        // survive into the directory. The `www_dir` field on the
        // namespace row is *not* used for path resolution — the
        // resolved state already carries the canonical PathBuf.
        let s = make_state("site-a.example");
        let mut map: HashMap<String, Arc<VhostState>> = HashMap::new();
        map.insert("site-a.example".into(), s.clone());

        // The row's www_dir points somewhere different (nonexistent).
        // For config_bootstrap classification we should NOT touch
        // the filesystem and should NOT use the row's www_dir.
        let rows = vec![make_row(
            "site-a.example",
            "/nonexistent/path",
            "config_bootstrap",
        )];

        let d = from_namespace_rows(&rows, &map, &HashSet::new()).unwrap();
        assert_eq!(d.primaries.len(), 1);
        assert!(Arc::ptr_eq(&d.primaries[0].state, &s));
        // The resolved Arc's www_dir (set by make_state) wins, not
        // the row's nonexistent path.
        assert_eq!(d.primaries[0].state.www_dir, PathBuf::from("/srv/www"));
    }

    #[test]
    fn from_namespace_rows_config_bootstrap_missing_from_runtime_map_errors() {
        // Substrate invariant: a `source = "config_bootstrap"` row
        // MUST have a matching entry in the resolved runtime map.
        // C3d's orphan pass deletes stale config_bootstrap rows
        // whose `fqdn` is no longer in `node.conf.mix`, and a current
        // config row is always materialised into the map. A
        // mismatch is a substrate bug, not a silent synthesize.
        let td = tempfile::TempDir::new().unwrap();
        let rows = vec![make_row(
            "stale-config.example",
            td.path().to_str().unwrap(),
            "config_bootstrap",
        )];

        let err = from_namespace_rows(&rows, &HashMap::new(), &HashSet::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("substrate-invariant violation"),
            "expected invariant-violation diagnostic, got: {msg}"
        );
        assert!(msg.contains("stale-config.example"));
    }

    #[test]
    fn from_namespace_rows_failsoft_disabled_host_skips_not_errors() {
        // B1 fail-soft: a `config_bootstrap` row whose fqdn is absent
        // from the runtime map (resolve_node_state dropped it on a bad
        // cert) but PRESENT in `disabled_hosts` must be SKIPPED, not
        // treated as the invariant violation above. The healthy sibling
        // still builds; the disabled fqdn leaves no routing surface.
        let healthy = make_state("good.example");
        let mut map: HashMap<String, Arc<VhostState>> = HashMap::new();
        map.insert("good.example".into(), healthy.clone());

        let rows = vec![
            make_row("good.example", "/srv/www", "config_bootstrap"),
            // Disabled sibling — absent from the runtime map by design.
            make_row("bad.example", "/nonexistent/path", "config_bootstrap"),
        ];
        let mut disabled = HashSet::new();
        disabled.insert("bad.example".to_string());

        let d = from_namespace_rows(&rows, &map, &disabled).unwrap();
        assert_eq!(d.primaries.len(), 1, "only the healthy vhost is routed");
        assert!(d.by_host.contains_key("good.example"));
        assert!(
            !d.by_host.contains_key("bad.example"),
            "disabled fqdn must not be routable"
        );
    }

    #[test]
    fn from_namespace_rows_bus_runtime_synthesises_minimal_state() {
        // `source = "bus_runtime"` row, no matching `[[webd.vhost]]`
        // block — the operator added it via `vhost.add` after the
        // last restart. The directory synthesises a minimal
        // `VhostState` from namespace fields only: www_dir =
        // row.www_dir, CMS / JMAP / noded / docs = None.
        let td = tempfile::TempDir::new().unwrap();
        let www_dir = td.path().to_path_buf();
        let rows = vec![make_row(
            "orphan.example",
            www_dir.to_str().unwrap(),
            "bus_runtime",
        )];

        let d = from_namespace_rows(&rows, &HashMap::new(), &HashSet::new()).unwrap();
        assert_eq!(d.primaries.len(), 1);
        let p = &d.primaries[0];
        assert_eq!(p.state.fqdn, "orphan.example");
        assert_eq!(p.state.www_dir, www_dir);
        assert!(p.state.db.is_none());
        assert!(p.state.jmap_upstream.is_none());
        assert!(p.state.noded_ws.is_none());
        assert!(p.state.docs_dir.is_none());
        assert_eq!(p.aliases, Vec::<String>::new());
        assert!(d.by_host.contains_key("orphan.example"));
        assert!(d.admit_plain_http.contains("orphan.example"));
    }

    #[test]
    fn from_namespace_rows_bus_runtime_wins_over_matching_config() {
        // **Regression for the C3c invariant.** When BOTH an
        // `bus_runtime` namespace row AND a matching `[[webd.vhost]]`
        // config block exist (and have therefore both populated the
        // runtime map), the namespace row wins — C3d's bootstrap
        // logs a "conflict skip" WARN and preserves the operator's
        // runtime row, so the routing layer must do the same. The
        // directory synthesises a minimal state from the namespace
        // row's www_dir; the config-derived Arc is ignored.
        let td = tempfile::TempDir::new().unwrap();
        let runtime_www_dir = td.path().to_path_buf();

        // Resolved runtime map carries a config-derived Arc with a
        // DIFFERENT www_dir than the namespace row.
        let config_state = make_state("contested.example");
        let mut map: HashMap<String, Arc<VhostState>> = HashMap::new();
        map.insert("contested.example".into(), config_state.clone());

        let rows = vec![make_row(
            "contested.example",
            runtime_www_dir.to_str().unwrap(),
            "bus_runtime",
        )];

        let d = from_namespace_rows(&rows, &map, &HashSet::new()).unwrap();
        assert_eq!(d.primaries.len(), 1);
        let p = &d.primaries[0];
        // The Arc is NOT the config-derived one.
        assert!(
            !Arc::ptr_eq(&p.state, &config_state),
            "bus_runtime row must synthesise its own VhostState, not reuse the config-derived Arc"
        );
        // The namespace row's www_dir wins, not the config's.
        assert_eq!(p.state.www_dir, runtime_www_dir);
        // CMS / JMAP / etc are None on the synthesised state —
        // operator-added vhosts get static-only by design.
        assert!(p.state.db.is_none());
        assert!(p.state.jmap_upstream.is_none());
    }

    #[test]
    fn from_namespace_rows_bus_runtime_with_missing_www_dir_is_skipped() {
        // `bus_runtime` row whose `www_dir` doesn't exist on disk:
        // the row is skipped from the directory (with ERROR log)
        // but the namespace row stays — repair is operator-side
        // `mkdir` then restart, or `webd.vhost.update` to point at
        // an existing path.
        let rows = vec![make_row(
            "broken.example",
            "/this/path/does/not/exist/at/all",
            "bus_runtime",
        )];

        let d = from_namespace_rows(&rows, &HashMap::new(), &HashSet::new()).unwrap();
        assert!(d.by_host.is_empty());
        assert!(d.admit_plain_http.is_empty());
        assert!(d.primaries.is_empty());
    }

    #[test]
    fn from_namespace_rows_disabled_row_dropped_from_routing() {
        // `enabled = false` rows are dropped from the directory
        // entirely — namespace state preserved, routing surface
        // skipped. Schema help text contract: "Disabled vhosts skip
        // dispatch and skip ACME ticks." The skip happens before
        // source classification, so both `config_bootstrap` and
        // `bus_runtime` disabled rows drop.
        let td = tempfile::TempDir::new().unwrap();
        let www_dir = td.path().to_str().unwrap();
        let mut row = make_row("disabled.example", www_dir, "bus_runtime");
        row.enabled = false;

        let d = from_namespace_rows(&[row], &HashMap::new(), &HashSet::new()).unwrap();
        assert!(d.by_host.is_empty());
        assert!(d.primaries.is_empty());
    }

    #[test]
    fn from_namespace_rows_aliases_recovered_from_runtime_map() {
        // `[[webd.vhost]] aliases = [...]` is rejected by the
        // namespace `before_set` rule 3 (Phase 4 multi-SAN), so the
        // namespace row's `aliases` field is always empty today.
        // But the runtime map registers each alias under a separate
        // key pointing at the same `Arc<VhostState>` — the
        // directory must recover those aliases by Arc-identity
        // name-set subtraction. **Config-bootstrap classification
        // only**: `bus_runtime` rows synthesise from namespace
        // state alone and use `row.aliases` directly.
        let s = make_state("site-a.example");
        let mut map: HashMap<String, Arc<VhostState>> = HashMap::new();
        map.insert("site-a.example".into(), s.clone());
        map.insert("www.site-a.example".into(), s.clone());
        map.insert("alt.site-a.example".into(), s.clone());

        let rows = vec![make_row("site-a.example", "/srv/www", "config_bootstrap")];
        let d = from_namespace_rows(&rows, &map, &HashSet::new()).unwrap();

        assert_eq!(d.primaries.len(), 1);
        let mut aliases = d.primaries[0].aliases.clone();
        aliases.sort();
        assert_eq!(
            aliases,
            vec![
                "alt.site-a.example".to_string(),
                "www.site-a.example".to_string()
            ]
        );
        // All three names resolve to the primary's Arc.
        for name in ["site-a.example", "www.site-a.example", "alt.site-a.example"] {
            assert!(Arc::ptr_eq(&d.vhost_for_host(name).unwrap(), &s));
        }
    }

    #[test]
    fn from_namespace_rows_mixed_config_and_orphan() {
        // Realistic mixed case: a `config_bootstrap` row (resolved
        // Arc reused) + an `bus_runtime` row whose fqdn is absent
        // from the runtime map (minimal state synthesised). The
        // directory carries both, sorted by primary FQDN.
        let td = tempfile::TempDir::new().unwrap();
        let orphan_www = td.path().to_path_buf();

        let s = make_state("config.example");
        let mut map: HashMap<String, Arc<VhostState>> = HashMap::new();
        map.insert("config.example".into(), s.clone());

        let rows = vec![
            make_row("config.example", "/srv/www", "config_bootstrap"),
            make_row(
                "orphan.example",
                orphan_www.to_str().unwrap(),
                "bus_runtime",
            ),
        ];

        let d = from_namespace_rows(&rows, &map, &HashSet::new()).unwrap();
        assert_eq!(d.primaries.len(), 2);
        // Sorted by primary FQDN.
        assert_eq!(d.primaries[0].state.fqdn, "config.example");
        assert!(Arc::ptr_eq(&d.primaries[0].state, &s));
        assert_eq!(d.primaries[1].state.fqdn, "orphan.example");
        // Orphan got the row's www_dir; resolved Arc kept its own.
        assert_eq!(d.primaries[1].state.www_dir, orphan_www);
    }

    #[test]
    fn build_rejects_alias_colliding_with_another_alias() {
        // Two distinct rows each claiming `shared.example` as an
        // alias. The second insert collides.
        let row1 = make_state("site-a.example");
        let row2 = make_state("site-b.example");
        let err = VhostDirectory::build(vec![
            VhostDirectoryEntry {
                state: row1,
                aliases: vec!["shared.example".into()],
            },
            VhostDirectoryEntry {
                state: row2,
                aliases: vec!["shared.example".into()],
            },
        ])
        .unwrap_err();
        assert!(err.to_string().contains("duplicate host"));
        assert!(err.to_string().contains("shared.example"));
    }
}
