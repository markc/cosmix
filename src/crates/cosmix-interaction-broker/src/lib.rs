//! Headless decision core for the `interact.*` broker.
//!
//! This crate owns the transport- and render-independent policy of the
//! notification broker: **origin labelling** (§4), **per-origin rate-limiting
//! plus an aggregate flood backstop** and **dedupe/coalesce** (§6), **remote
//! urgency-clamp** (§10.3), and **dispatch-registration enforcement** (§2). It
//! turns a validated [`NotifyRequest`] plus a [`Caller`] into a
//! [`NotifyDecision`] the bin then executes — mint nothing, draw nothing, touch
//! no bus.
//!
//! What lives **outside** here, in the eventual `ctkd` bin:
//!
//! - Bus service registration + the `interact.notify`/`update`/`dismiss` verbs;
//! - `notify-rust` delivery to `org.freedesktop.Notifications`;
//! - the `ctk.notify` mesh-trust capability gate (remote peers);
//! - the SPEC-12 `interactions` props collection;
//! - minting the opaque [`NotifyHandle`] and reading the wall clock.
//!
//! Both of the latter are **injected** into [`NotifyBroker::accept`] (`now_ms`,
//! `fresh_handle`), so this core is deterministic and fully unit-testable, and it
//! carries no Bevy/Bus/mesh/RNG/clock dependency. Where the bin that wraps it
//! ultimately lives is deliberately left open for the in-context review.

mod dedupe;
mod dialog;
mod origin;
mod ratelimit;

pub use dedupe::DedupeTable;
pub use dialog::*;
pub use origin::{ANONYMOUS, OriginPolicy, RESERVED_LABELS, is_reserved};
pub use ratelimit::{DEFAULT_GLOBAL_RATE, RateConfig, RateLimiter};

use cosmix_interaction_schema::{
    NotifyHandle, NotifyRecord, NotifyRequest, NotifyState, Urgency, ValidationError,
};

/// Whether a service name is a currently-registered Bus service. The bin backs
/// this with the live `noded` registry; tests back it with a fixed set. A
/// notification whose action dispatches to an unregistered service resolves to
/// `failed` before desktop delivery (notify.v1 §2), so no action is shown for a
/// target absent from the worker's registry snapshot.
pub trait ServiceRegistry {
    fn is_registered(&self, service: &str) -> bool;
}

/// Any `Fn(&str) -> bool` is a registry — convenient for the bin and for tests.
impl<F: Fn(&str) -> bool> ServiceRegistry for F {
    fn is_registered(&self, service: &str) -> bool {
        self(service)
    }
}

/// Who is asking. `from` is the caller's **registered** Bus identity (the Bus
/// `from`); `None` means an anonymous local caller with no durable principal.
/// `is_remote` marks a mesh peer — remote callers are urgency-clamped (§10.3),
/// and the bin has already checked their `ctk.notify` capability before calling.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    pub from: Option<&'a str>,
    pub is_remote: bool,
}

impl<'a> Caller<'a> {
    /// A local caller with a registered identity.
    pub fn local(from: &'a str) -> Self {
        Caller {
            from: Some(from),
            is_remote: false,
        }
    }

    /// A local caller with no durable principal (stamps `origin = "anonymous"`).
    pub fn anonymous() -> Self {
        Caller {
            from: None,
            is_remote: false,
        }
    }

    /// A remote mesh peer (urgency-clamped to `<= Normal`).
    pub fn remote(from: &'a str) -> Self {
        Caller {
            from: Some(from),
            is_remote: true,
        }
    }
}

/// Why the broker refused a notification before delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Structurally invalid per [`NotifyRequest::validate`].
    Invalid(ValidationError),
    /// An action dispatches to a service that is not currently registered.
    UnregisteredDispatch { index: usize, service: String },
}

/// The broker's verdict on one `interact.notify`. The bin executes it: deliver
/// (optionally replacing a prior toast), drop a throttled one with a WARN, or
/// return the rejection to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyDecision {
    /// Deliver `record` via the notification daemon and store it in the
    /// `interactions` collection. When `replaces` is `Some`, it maps to the
    /// freedesktop `replaces_id` of the prior delivery for the same
    /// `(origin, dedupe_key)`, and `record.handle` reuses that prior handle so
    /// the caller's handle stays stable across coalesced updates.
    Deliver {
        record: NotifyRecord,
        replaces: Option<NotifyHandle>,
    },
    /// Over budget for this origin — do not deliver; log a WARN. Carries the
    /// stamped origin for the log line.
    Throttled { origin: String },
    /// Refused before any delivery attempt.
    Rejected(RejectReason),
}

/// Validate action targets against a service-registry snapshot. Kept separate
/// from [`NotifyBroker::accept`] so interactd can queue immediately and perform
/// the potentially slow `noded.list` lookup on its delivery worker.
pub fn validate_dispatch_targets<R: ServiceRegistry>(
    req: &NotifyRequest,
    registry: &R,
) -> Result<(), RejectReason> {
    for (index, action) in req.actions.iter().enumerate() {
        if let Some(d) = &action.on_invoke
            && !registry.is_registered(&d.service)
        {
            return Err(RejectReason::UnregisteredDispatch {
                index,
                service: d.service.clone(),
            });
        }
    }
    Ok(())
}

/// The notification broker's decision engine. Holds the origin policy, the
/// per-origin rate limiter plus aggregate backstop, and the dedupe table;
/// `accept` mutates the latter two. Delivery, storage, handle minting, and the
/// clock are the bin's.
#[derive(Debug, Clone, Default)]
pub struct NotifyBroker {
    origin: OriginPolicy,
    rate: RateLimiter,
    dedupe: DedupeTable,
}

impl NotifyBroker {
    /// The v1 broker: empty reserved-origin allowlist (§10.2), default
    /// per-origin fairness budgets (burst 5, ~1/s) and aggregate flood backstop
    /// (burst 30, ~5/s).
    pub fn v1() -> Self {
        NotifyBroker {
            origin: OriginPolicy::v1(),
            rate: RateLimiter::new(RateConfig::default()),
            dedupe: DedupeTable::new(),
        }
    }

    /// Construct with explicit policy pieces (e.g. a post-v1 allowlist or a
    /// tuned rate budget).
    pub fn new(origin: OriginPolicy, rate: RateConfig) -> Self {
        NotifyBroker {
            origin,
            rate: RateLimiter::new(rate),
            dedupe: DedupeTable::new(),
        }
    }

    /// Read-only view of the origin policy (for the bin's own reserved-label
    /// checks / diagnostics).
    pub fn origin_policy(&self) -> &OriginPolicy {
        &self.origin
    }

    /// Decide the fate of one `interact.notify` with an already-available
    /// registry view. This compatibility entry point performs dispatch-target
    /// validation synchronously, then delegates to [`Self::accept_queued`].
    pub fn accept<R: ServiceRegistry>(
        &mut self,
        req: &NotifyRequest,
        caller: Caller<'_>,
        now_ms: u64,
        fresh_handle: NotifyHandle,
        registry: &R,
    ) -> NotifyDecision {
        if let Err(e) = req.validate() {
            return NotifyDecision::Rejected(RejectReason::Invalid(e));
        }
        if let Err(reason) = validate_dispatch_targets(req, registry) {
            return NotifyDecision::Rejected(reason);
        }
        self.accept_queued(req, caller, now_ms, fresh_handle)
    }

    /// Decide the queue-time fate of one `interact.notify` without consulting
    /// the service registry. interactd uses this entry point so `noded.list`
    /// runs later on its delivery worker.
    ///
    /// - `req` — the caller's request (validated here).
    /// - `caller` — stamped identity + remote flag.
    /// - `now_ms` — wall-clock ms since the epoch, supplied by the bin.
    /// - `fresh_handle` — a freshly minted opaque handle, used only if this is a
    ///   *new* (non-coalesced) notification.
    ///
    /// Order: structural validation → origin stamp → dedupe lookup → unified
    /// rate-limit (fresh and replacement deliveries) → urgency clamp → record +
    /// verdict. Dispatch registration is checked asynchronously with
    /// [`validate_dispatch_targets`] before delivery.
    pub fn accept_queued(
        &mut self,
        req: &NotifyRequest,
        caller: Caller<'_>,
        now_ms: u64,
        fresh_handle: NotifyHandle,
    ) -> NotifyDecision {
        // 1. Structural validation (schema-owned).
        if let Err(e) = req.validate() {
            return NotifyDecision::Rejected(RejectReason::Invalid(e));
        }

        // 2. Broker-stamped provenance (caller cannot influence it).
        let origin = self.origin.resolve(caller.from);

        // 3. Dedupe: does a same-(origin,key) notification already occupy a slot?
        let existing = req
            .dedupe_key
            .as_deref()
            .and_then(|k| self.dedupe.lookup(&origin, k).cloned());

        // 4. Every delivery attempt consumes from one origin bucket, including
        //    coalescing replacements. Otherwise a hot dedupe key bypasses the
        //    sustained-rate policy indefinitely.
        if !self.rate.try_consume(&origin, now_ms) {
            return NotifyDecision::Throttled { origin };
        }

        // 5. Remote urgency clamp to <= Normal (§10.3). Recorded as an override
        //    only when it actually changed the requested level.
        let urgency_override = if caller.is_remote && req.urgency == Urgency::Critical {
            Some(Urgency::Normal)
        } else {
            None
        };

        // 6. New handle for a fresh notify; reuse the prior one on coalesce so the
        //    caller's handle stays stable across updates.
        let handle = existing.clone().unwrap_or(fresh_handle);
        if let Some(key) = req.dedupe_key.as_deref() {
            self.dedupe.record(&origin, key, handle.clone());
        }

        let record = NotifyRecord {
            handle,
            origin,
            state: NotifyState::Queued,
            created_at_ms: now_ms,
            summary: req.summary.clone(),
            effective_urgency: urgency_override.unwrap_or(req.urgency),
            urgency_override,
            dedupe_key: req.dedupe_key.clone(),
        };

        NotifyDecision::Deliver {
            record,
            replaces: existing,
        }
    }

    /// Spend one token for a mutation of an existing notification. Updates and
    /// notify/dedupe delivery therefore share the exact same origin bucket.
    pub fn try_consume(&mut self, origin: &str, now_ms: u64) -> bool {
        self.rate.try_consume(origin, now_ms)
    }

    /// Release the dedupe slot for a notification that has reached a terminal
    /// state. Rate buckets deliberately survive retirement so rapid
    /// create/fail/recreate cycles cannot reset their budget.
    pub fn retire(&mut self, handle: &NotifyHandle) {
        self.dedupe.forget_handle(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_interaction_schema::{Dispatch, IconRef, NotifyAction};

    /// Registry accepting a fixed set of service names.
    fn registry(names: &'static [&'static str]) -> impl ServiceRegistry {
        move |s: &str| names.contains(&s)
    }

    fn none_registered() -> impl ServiceRegistry {
        |_: &str| false
    }

    fn handle(s: &str) -> NotifyHandle {
        NotifyHandle(s.to_string())
    }

    #[test]
    fn happy_path_delivers_with_stamped_origin() {
        let mut b = NotifyBroker::v1();
        let dec = b.accept(
            &NotifyRequest::new("Render finished"),
            Caller::local("musicd"),
            1_000,
            handle("h1"),
            &none_registered(),
        );
        match dec {
            NotifyDecision::Deliver { record, replaces } => {
                assert_eq!(record.origin, "musicd");
                assert_eq!(record.handle, handle("h1"));
                assert_eq!(record.state, NotifyState::Queued);
                assert_eq!(record.created_at_ms, 1_000);
                assert_eq!(record.urgency_override, None);
                assert_eq!(replaces, None);
            }
            other => panic!("expected Deliver, got {other:?}"),
        }
    }

    #[test]
    fn caller_cannot_forge_origin() {
        let mut b = NotifyBroker::v1();
        // even claiming identity "system", a caller is stamped anonymous in v1
        let dec = b.accept(
            &NotifyRequest::new("System update ready"),
            Caller::local("system"),
            0,
            handle("h1"),
            &none_registered(),
        );
        let NotifyDecision::Deliver { record, .. } = dec else {
            panic!("expected Deliver");
        };
        assert_eq!(record.origin, ANONYMOUS);
    }

    #[test]
    fn anonymous_caller_labelled_anonymous() {
        let mut b = NotifyBroker::v1();
        let NotifyDecision::Deliver { record, .. } = b.accept(
            &NotifyRequest::new("hi"),
            Caller::anonymous(),
            0,
            handle("h1"),
            &none_registered(),
        ) else {
            panic!("expected Deliver");
        };
        assert_eq!(record.origin, ANONYMOUS);
    }

    #[test]
    fn structural_invalid_is_rejected() {
        let mut b = NotifyBroker::v1();
        let dec = b.accept(
            &NotifyRequest::new("   "),
            Caller::local("musicd"),
            0,
            handle("h1"),
            &none_registered(),
        );
        assert_eq!(
            dec,
            NotifyDecision::Rejected(RejectReason::Invalid(ValidationError::EmptySummary))
        );
    }

    #[test]
    fn action_to_unregistered_service_is_rejected() {
        let mut b = NotifyBroker::v1();
        let mut req = NotifyRequest::new("Build done");
        req.actions = vec![NotifyAction {
            key: "reveal".into(),
            label: "Reveal".into(),
            on_invoke: Some(Dispatch {
                service: "filemgr".into(),
                verb: "app.reveal".into(),
            }),
        }];
        let dec = b.accept(
            &req,
            Caller::local("musicd"),
            0,
            handle("h1"),
            &registry(&["musicd"]), // filemgr NOT registered
        );
        assert_eq!(
            dec,
            NotifyDecision::Rejected(RejectReason::UnregisteredDispatch {
                index: 0,
                service: "filemgr".into()
            })
        );
    }

    #[test]
    fn action_to_registered_service_is_accepted() {
        let mut b = NotifyBroker::v1();
        let mut req = NotifyRequest::new("Build done");
        req.icon = Some(IconRef::Lucide("check".into()));
        req.actions = vec![NotifyAction {
            key: "reveal".into(),
            label: "Reveal".into(),
            on_invoke: Some(Dispatch {
                service: "filemgr".into(),
                verb: "app.reveal".into(),
            }),
        }];
        let dec = b.accept(
            &req,
            Caller::local("musicd"),
            0,
            handle("h1"),
            &registry(&["musicd", "filemgr"]),
        );
        assert!(matches!(dec, NotifyDecision::Deliver { .. }));
    }

    #[test]
    fn action_without_dispatch_needs_no_registration() {
        let mut b = NotifyBroker::v1();
        let mut req = NotifyRequest::new("Note");
        req.actions = vec![NotifyAction {
            key: "ok".into(),
            label: "OK".into(),
            on_invoke: None, // dismiss + record only
        }];
        let dec = b.accept(
            &req,
            Caller::local("musicd"),
            0,
            handle("h1"),
            &none_registered(),
        );
        assert!(matches!(dec, NotifyDecision::Deliver { .. }));
    }

    #[test]
    fn dedupe_key_replaces_and_keeps_handle_stable() {
        let mut b = NotifyBroker::v1();
        let mut req = NotifyRequest::new("Rendering… 10%");
        req.dedupe_key = Some("render".into());

        let NotifyDecision::Deliver {
            record: r1,
            replaces,
        } = b.accept(
            &req,
            Caller::local("musicd"),
            0,
            handle("h1"),
            &none_registered(),
        )
        else {
            panic!("expected Deliver");
        };
        assert_eq!(replaces, None);
        assert_eq!(r1.handle, handle("h1"));

        // same key again — a *fresh* handle is offered but must be ignored
        let mut req2 = NotifyRequest::new("Rendering… 60%");
        req2.dedupe_key = Some("render".into());
        let NotifyDecision::Deliver {
            record: r2,
            replaces,
        } = b.accept(
            &req2,
            Caller::local("musicd"),
            10,
            handle("h2-fresh"),
            &none_registered(),
        )
        else {
            panic!("expected Deliver");
        };
        assert_eq!(
            r2.handle,
            handle("h1"),
            "handle must stay stable on coalesce"
        );
        assert_eq!(replaces, Some(handle("h1")));
    }

    #[test]
    fn dedupe_is_scoped_per_origin() {
        let mut b = NotifyBroker::v1();
        let mut req = NotifyRequest::new("x");
        req.dedupe_key = Some("k".into());
        let NotifyDecision::Deliver { replaces, .. } = b.accept(
            &req,
            Caller::local("a"),
            0,
            handle("ha"),
            &none_registered(),
        ) else {
            panic!("expected Deliver");
        };
        assert_eq!(replaces, None);
        // different origin, same key -> not a replace
        let NotifyDecision::Deliver { replaces, .. } = b.accept(
            &req,
            Caller::local("b"),
            0,
            handle("hb"),
            &none_registered(),
        ) else {
            panic!("expected Deliver");
        };
        assert_eq!(replaces, None);
    }

    #[test]
    fn over_budget_origin_is_throttled() {
        // default burst is 5; the 6th same-instant notify from one origin throttles
        let mut b = NotifyBroker::v1();
        for i in 0..5 {
            let dec = b.accept(
                &NotifyRequest::new("spam"),
                Caller::local("noisy"),
                0,
                handle(&format!("h{i}")),
                &none_registered(),
            );
            assert!(matches!(dec, NotifyDecision::Deliver { .. }), "burst {i}");
        }
        let dec = b.accept(
            &NotifyRequest::new("spam"),
            Caller::local("noisy"),
            0,
            handle("h6"),
            &none_registered(),
        );
        assert_eq!(
            dec,
            NotifyDecision::Throttled {
                origin: "noisy".into()
            }
        );
    }

    #[test]
    fn coalescing_replace_uses_the_same_rate_budget() {
        // Exhaust the budget with distinct notifications; a same-key replace is
        // also throttled so dedupe cannot bypass the sustained-rate policy.
        let mut b = NotifyBroker::v1();
        let mut keyed = NotifyRequest::new("progress");
        keyed.dedupe_key = Some("p".into());
        // first keyed notify consumes one token, records the slot
        assert!(matches!(
            b.accept(
                &keyed,
                Caller::local("x"),
                0,
                handle("h1"),
                &none_registered()
            ),
            NotifyDecision::Deliver { .. }
        ));
        // burn the remaining 4 tokens
        for i in 0..4 {
            assert!(matches!(
                b.accept(
                    &NotifyRequest::new("other"),
                    Caller::local("x"),
                    0,
                    handle(&format!("o{i}")),
                    &none_registered()
                ),
                NotifyDecision::Deliver { .. }
            ));
        }
        // budget now empty: a fresh notify throttles...
        assert!(matches!(
            b.accept(
                &NotifyRequest::new("more"),
                Caller::local("x"),
                0,
                handle("z"),
                &none_registered()
            ),
            NotifyDecision::Throttled { .. }
        ));
        // ...and the coalescing replacement is governed by that same bucket.
        assert_eq!(
            b.accept(
                &keyed,
                Caller::local("x"),
                0,
                handle("h1b"),
                &none_registered()
            ),
            NotifyDecision::Throttled { origin: "x".into() }
        );
    }

    #[test]
    fn remote_critical_is_clamped_local_is_not() {
        let mut b = NotifyBroker::v1();
        let mut req = NotifyRequest::new("Alert");
        req.urgency = Urgency::Critical;

        // local Critical passes through untouched
        let NotifyDecision::Deliver { record, .. } = b.accept(
            &req,
            Caller::local("maild"),
            0,
            handle("h1"),
            &none_registered(),
        ) else {
            panic!("expected Deliver");
        };
        assert_eq!(record.urgency_override, None);
        assert_eq!(record.effective_urgency, Urgency::Critical);

        // remote Critical is clamped to Normal
        let NotifyDecision::Deliver { record, .. } = b.accept(
            &req,
            Caller::remote("peer-maild"),
            0,
            handle("h2"),
            &none_registered(),
        ) else {
            panic!("expected Deliver");
        };
        assert_eq!(record.urgency_override, Some(Urgency::Normal));
        assert_eq!(record.effective_urgency, Urgency::Normal);
    }

    #[test]
    fn retire_frees_dedupe_slot() {
        let mut b = NotifyBroker::v1();
        let mut req = NotifyRequest::new("x");
        req.dedupe_key = Some("k".into());
        assert!(matches!(
            b.accept(
                &req,
                Caller::local("a"),
                0,
                handle("h1"),
                &none_registered()
            ),
            NotifyDecision::Deliver { replaces: None, .. }
        ));
        b.retire(&handle("h1"));
        // after retire, the same key is a fresh delivery, not a replace
        let NotifyDecision::Deliver { record, replaces } = b.accept(
            &req,
            Caller::local("a"),
            100,
            handle("h2"),
            &none_registered(),
        ) else {
            panic!("expected Deliver");
        };
        assert_eq!(replaces, None);
        assert_eq!(record.handle, handle("h2"));
    }
}
