//! IMAP sequence-set / UID-set grammar (RFC 9051 §9).
//!
//! ```text
//! sequence-set    = (seq-number / seq-range) ["," sequence-set]
//! seq-number      = nz-number / "*"
//! seq-range       = seq-number ":" seq-number
//! nz-number       = digit-nz *DIGIT          ; 1..=4_294_967_295
//! ```
//!
//! The wire grammar is identical for message-sequence-number sets
//! (FETCH / STORE / COPY / MOVE / SEARCH) and UID sets (UID FETCH /
//! UID STORE / UID COPY / UID MOVE / UID SEARCH). The two diverge
//! only at *resolution* time:
//!
//! * msn `*` = the number of messages in the mailbox (EXISTS).
//! * UID `*` = the UID of the last message, or `UIDNEXT - 1` /
//!   `UIDNEXT` on an empty mailbox (per RFC 9051 §9 "if the mailbox
//!   is empty, the mailbox's current UIDNEXT value").
//! * msn ranges select 1-based positions within the visible sequence.
//! * UID ranges select absolute UIDs that may not all currently
//!   exist; RFC 9051 §6.4.8 says UID ranges that include `*` always
//!   include the largest UID in the mailbox, even if the explicit
//!   endpoint exceeds it (`3291:*` matches the last UID even if the
//!   last UID is < 3291).
//!
//! T8 lands the parser + a passive `SequenceMap` resolver. No verb
//! (FETCH / STORE / COPY / SEARCH) wiring yet — those come in T9+.
//!
//! ## Design choices
//!
//! * `Endpoint::Number(u32)` carries the nz-number invariant in the
//!   type itself; the parser refuses `0` at the byte level. RFC 9051
//!   defines nz-number as `1..=u32::MAX`, so any wider integer is a
//!   grammar violation and produces a `BAD` response upstream.
//!
//! * `Range { from, to }` stores endpoints in wire order; resolution
//!   normalises (`from > to` becomes `to..=from` per RFC 9051 §9
//!   "4:2 and 2:4 are equivalent"). Storing wire order rather than
//!   normalised order keeps error messages user-recognisable.
//!
//! * `SeqSet { elements }` is guaranteed non-empty by the parser —
//!   the grammar requires at least one element. The invariant lets
//!   resolvers skip the `if elements.is_empty()` branch.
//!
//! * Resolution returns a `Vec<Resolved>` *deduplicated* and *sorted
//!   by msn*. RFC 9051 §9 explicitly permits coalescing overlaps
//!   ("Servers MAY coalesce overlaps and/or execute the sequence in
//!   any order"), and FETCH responses on the wire are conventionally
//!   ascending-by-sequence. The deterministic ordering keeps test
//!   assertions tractable.

use std::collections::BTreeMap;

use cosmix_mds::ItemId;

/// Single endpoint of a sequence-set element. RFC 9051 §9
/// `seq-number = nz-number / "*"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// nz-number: 1..=u32::MAX. Zero is rejected at parse.
    Number(u32),
    /// The literal `*` token: "largest in use" — meaning depends on
    /// resolution context (msn vs uid).
    Star,
}

/// One element of the comma-separated `sequence-set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    /// A single seq-number — `1`, `42`, `*`.
    Single(Endpoint),
    /// `from:to`. Endpoints may be in either wire order; resolution
    /// normalises. `*:*` is permitted by the grammar.
    Range { from: Endpoint, to: Endpoint },
}

/// Parsed `sequence-set`. Always non-empty (the grammar requires at
/// least one element).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqSet {
    elements: Vec<Element>,
}

impl SeqSet {
    /// Borrow the parsed elements in wire order.
    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    /// True iff `msn` belongs to this set under MSN-context Star
    /// semantics. `exists` is the `* N EXISTS` value at the time of
    /// the test. Star resolves to `exists`. Matches the resolution
    /// rules in [`SequenceMap::resolve_msn`] but answers a single
    /// point — used by `SEARCH` (and any future per-row matcher that
    /// can't afford to materialise a full resolved vector).
    pub fn contains_msn(&self, msn: u32, exists: u32) -> bool {
        if msn == 0 || msn > exists {
            return false;
        }
        for el in &self.elements {
            if let Some((lo, hi)) = element_endpoints_msn(el, exists)
                && lo <= msn
                && msn <= hi
            {
                return true;
            }
        }
        false
    }

    /// True iff `uid` belongs to this set under UID-context Star
    /// semantics. `largest_uid` is `MAX(uid)` over the mailbox
    /// snapshot, or `UIDNEXT - 1` on an empty mailbox. Matches the
    /// resolution rules in [`SequenceMap::resolve_uid`] but answers a
    /// single point — used by `UID SEARCH`.
    pub fn contains_uid(&self, uid: u64, largest_uid: u64) -> bool {
        for el in &self.elements {
            let (lo, hi) = element_endpoints_uid(el, largest_uid);
            if hi >= lo && uid >= lo && uid <= hi {
                return true;
            }
        }
        false
    }

    /// Parse `1:5,7,9:*` per RFC 9051 §9. Returns a human-readable
    /// error string on grammar violation; callers wrap that into a
    /// tagged `BAD` response upstream.
    ///
    /// Strict rules (every rejected case has a regression test):
    ///
    /// * No whitespace anywhere. The grammar permits none.
    /// * No empty input. The grammar permits none.
    /// * No leading/trailing commas. No empty elements between commas.
    /// * `nz-number = 1..=u32::MAX`. `0` is rejected. Any value that
    ///   would overflow `u32` is rejected.
    /// * Each `from:to` range has exactly one `:`. `1:2:3` is BAD.
    /// * A bare `:` or `:5` or `5:` is BAD.
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.is_empty() {
            return Err("sequence-set is empty".to_string());
        }
        if input.bytes().any(|b| b == b' ' || b == b'\t') {
            return Err("sequence-set contains whitespace".to_string());
        }
        let mut elements = Vec::new();
        for raw in input.split(',') {
            if raw.is_empty() {
                return Err(
                    "sequence-set has empty element (consecutive or trailing comma)".to_string(),
                );
            }
            elements.push(parse_element(raw)?);
        }
        // Defensive: `input.split(',')` on a non-empty input always
        // yields at least one item, and we error on empty items above,
        // so `elements` is guaranteed non-empty here. The assert keeps
        // the SeqSet invariant honest for future refactors.
        debug_assert!(!elements.is_empty());
        Ok(Self { elements })
    }
}

fn parse_element(s: &str) -> Result<Element, String> {
    match s.split_once(':') {
        None => Ok(Element::Single(parse_endpoint(s)?)),
        Some((lhs, rhs)) => {
            if rhs.contains(':') {
                return Err(format!(
                    "sequence-set range {s:?} has more than one ':' separator"
                ));
            }
            if lhs.is_empty() || rhs.is_empty() {
                return Err(format!("sequence-set range {s:?} has empty endpoint"));
            }
            Ok(Element::Range {
                from: parse_endpoint(lhs)?,
                to: parse_endpoint(rhs)?,
            })
        }
    }
}

fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    if s == "*" {
        return Ok(Endpoint::Star);
    }
    // RFC 9051 nz-number: digits only, no leading zero, no sign.
    // We reject `0`, `01`, `+1`, `-1`, `1.0` and anything wider than u32.
    if s.is_empty() {
        return Err("sequence-set endpoint is empty".to_string());
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "sequence-set endpoint {s:?} is not a non-negative integer"
        ));
    }
    if s.len() > 1 && s.starts_with('0') {
        return Err(format!("sequence-set endpoint {s:?} has a leading zero"));
    }
    let n: u32 = s
        .parse()
        .map_err(|_| format!("sequence-set endpoint {s:?} does not fit in 32 bits"))?;
    if n == 0 {
        return Err(format!(
            "sequence-set endpoint {s:?} is zero (RFC 9051 nz-number forbids 0)"
        ));
    }
    Ok(Endpoint::Number(n))
}

/// Per-selected-mailbox snapshot of `msn ↔ (uid, item_id)`. Built at
/// SELECT/EXAMINE time from `list_emails_in_mailbox` and refreshed
/// on EXISTS/EXPUNGE notifier deltas (Phase 5 — IDLE).
///
/// Entries are sorted ascending by UID, which is the canonical IMAP
/// MSN ordering (RFC 9051 §2.3.1 "When a message is appended to a
/// mailbox its UID … MUST be greater than the UID of every other
/// message in the mailbox"). MSN is the 1-based index into this
/// vector.
#[derive(Debug, Clone, Default)]
pub struct SequenceMap {
    /// `(uid, item_id)` ordered by ascending uid. Index `i` ↔ msn `i+1`.
    entries: Vec<(u64, ItemId)>,
}

/// One row of a resolved sequence-set: the wire MSN, the UID, and
/// the storage-side item id. Returned ascending-by-MSN, deduplicated
/// across overlapping elements (RFC 9051 §9 permits coalescing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// 1-based message sequence number at resolution time.
    pub msn: u32,
    /// UID at resolution time (stable across the session if
    /// UIDVALIDITY hasn't rotated).
    pub uid: u64,
    /// Storage-side identifier for FETCH / STORE / COPY plumbing.
    pub item_id: ItemId,
}

impl SequenceMap {
    /// Build from `(uid, item_id)` pairs in any order — entries are
    /// sorted ascending-by-UID and deduplicated internally. Callers
    /// can pass `list_emails_in_mailbox` rows directly.
    ///
    /// **Invariants enforced (debug builds):**
    ///
    /// * Each UID is `<= u32::MAX`. RFC 9051 §9 defines wire UIDs as
    ///   `nz-number` (1..=u32::MAX). Cosmix's storage-side
    ///   `MembershipUid::uid` is `u64`, but it's widened from a `u32`
    ///   sequence (see `mailstore::next_seq`), so any larger value
    ///   here would indicate a violated invariant rather than legal
    ///   data.
    /// * UIDs are unique within a mailbox snapshot. The mailstore
    ///   membership table enforces this; a duplicate here would
    ///   produce multiple `Resolved` rows for a single UID lookup
    ///   and violate the IMAP UID uniqueness contract.
    ///
    /// In release builds, the constructor is permissive: duplicates
    /// are coalesced (first occurrence wins), wider UIDs are kept.
    /// The debug assertions catch contract violations during testing
    /// without panicking a live IMAP session over corrupt input.
    pub fn from_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (u64, ItemId)>,
    {
        let mut entries: Vec<(u64, ItemId)> = entries.into_iter().collect();
        entries.sort_by_key(|(uid, _)| *uid);
        debug_assert!(
            entries.iter().all(|(uid, _)| *uid <= u32::MAX as u64),
            "SequenceMap UID exceeds u32 (RFC 9051 nz-number ceiling)"
        );
        let dup_count_before = entries.len();
        entries.dedup_by_key(|(uid, _)| *uid);
        debug_assert_eq!(
            dup_count_before,
            entries.len(),
            "SequenceMap built with duplicate UIDs"
        );
        Self { entries }
    }

    /// Number of messages in the selected mailbox at snapshot time —
    /// the `* N EXISTS` value and the upper bound for msn `*`.
    pub fn exists(&self) -> u32 {
        // u32::MAX message MSNs is far beyond any realistic mailbox;
        // the cap is the RFC 9051 nz-number ceiling. A mailbox with
        // more than 4 billion messages would have already failed
        // earlier limits.
        u32::try_from(self.entries.len()).expect("EXISTS exceeds u32; mailbox too large")
    }

    /// Largest UID in the snapshot, or `None` on an empty mailbox.
    /// RFC 9051 §9 special-cases empty-mailbox UID resolution against
    /// the *caller-supplied* UIDNEXT — see [`resolve_uid`].
    pub fn max_uid(&self) -> Option<u64> {
        self.entries.last().map(|(uid, _)| *uid)
    }

    /// Resolve a sequence-set as **message sequence numbers**
    /// (FETCH / STORE / COPY / MOVE / SEARCH — no `UID` prefix).
    ///
    /// * `*` maps to the highest MSN (`EXISTS`).
    /// * Empty-mailbox MSN sets resolve to an empty `Vec`. RFC 9051
    ///   §6.4.5 (FETCH) and §7.5.3 (untagged EXPUNGE) are clear that
    ///   FETCH on an empty mailbox returns no untagged responses.
    /// * MSNs that exceed EXISTS are silently dropped — RFC 9051 §6.4.8
    ///   "A non-existing unique identifier is ignored without any
    ///   error message generated" applies to msn ranges too per the
    ///   FETCH section; only an explicit reference like `STORE 99`
    ///   on a 3-message mailbox is a BAD per §6.4.6, but range
    ///   walking past `*` is silently truncated, matching server-side
    ///   convention.
    ///
    /// Returns ascending-by-msn, deduplicated.
    pub fn resolve_msn(&self, set: &SeqSet) -> Vec<Resolved> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let exists = self.exists();
        let mut hits: BTreeMap<u32, Resolved> = BTreeMap::new();
        for el in &set.elements {
            let Some((a, b)) = element_endpoints_msn(el, exists) else {
                // Element lies wholly above EXISTS — no msns to resolve.
                // Per RFC 9051 §6.4.8 (extended to msn sets via §6.4.5
                // FETCH semantics), out-of-range msns are silently
                // dropped at the resolver layer. Stricter per-verb
                // checks (e.g. STORE rejecting a single OOR explicit
                // msn as BAD per §6.4.6) are the verb's job.
                continue;
            };
            for msn in a..=b {
                let idx = (msn - 1) as usize;
                let (uid, item_id) = self.entries[idx];
                hits.insert(msn, Resolved { msn, uid, item_id });
            }
        }
        hits.into_values().collect()
    }

    /// Resolve a sequence-set as **UIDs** (UID FETCH / UID STORE /
    /// UID COPY / UID MOVE / UID SEARCH).
    ///
    /// * `*` maps to `max_uid` if the mailbox is non-empty. On an
    ///   empty mailbox, RFC 9051 §9 says `*` is the mailbox's current
    ///   UIDNEXT — but UIDNEXT itself never refers to a real message,
    ///   and this resolver only intersects against present entries,
    ///   so the empty-mailbox case is observably empty regardless of
    ///   which sentinel we use. We pass `uid_next - 1` to the
    ///   element-endpoint helper purely to keep the `5:*` ↔ `*:5`
    ///   normalisation arithmetic well-formed.
    /// * Ranges that include `*` always include the largest UID,
    ///   regardless of the other endpoint, per RFC 9051 §6.4.8 (`3291:*`
    ///   matches the last UID even if the last UID is < 3291).
    /// * UIDs that don't correspond to a current message are dropped
    ///   (RFC 9051 §6.4.8: "A non-existing unique identifier is
    ///   ignored without any error message generated. This SHOULD NOT
    ///   be considered an error.")
    ///
    /// `uid_next` is the caller's current UIDNEXT — only consulted
    /// when the mailbox is empty *and* the wire references `*`.
    /// Returns ascending-by-msn, deduplicated.
    pub fn resolve_uid(&self, set: &SeqSet, uid_next: u64) -> Vec<Resolved> {
        let max_uid_or_uidnext_minus_one = self
            .max_uid()
            .or_else(|| uid_next.checked_sub(1))
            .unwrap_or(0);
        // For each element compute the inclusive UID range it covers.
        // We then intersect with the present `entries`.
        let mut hits: BTreeMap<u32, Resolved> = BTreeMap::new();
        for el in &set.elements {
            let (lo, hi) = element_endpoints_uid(el, max_uid_or_uidnext_minus_one);
            if hi < lo {
                continue;
            }
            // Walk entries and pick those in [lo, hi]. The vec is
            // sorted ascending by uid, so a binary-search slice would
            // be O(log n + k); for now an O(n) scan keeps the code
            // small and ships test-grade.
            for (idx, (uid, item_id)) in self.entries.iter().enumerate() {
                if *uid >= lo && *uid <= hi {
                    let msn = (idx + 1) as u32;
                    hits.insert(
                        msn,
                        Resolved {
                            msn,
                            uid: *uid,
                            item_id: *item_id,
                        },
                    );
                }
            }
        }
        hits.into_values().collect()
    }
}

/// Intersect a sequence-set element with `[1, exists]`, returning the
/// inclusive MSN range to walk or `None` if the element lies wholly
/// above EXISTS. `Star` resolves to `exists`. Caller guarantees
/// `exists >= 1` (we early-return on empty mailbox).
///
/// Returning `None` rather than clamping is load-bearing: a single
/// explicit msn `99` on a 3-message mailbox must resolve to *nothing*,
/// not to msn 3. Clamping would silently retarget every out-of-range
/// FETCH / STORE / COPY at the last message in the mailbox, which is
/// a correctness disaster.
fn element_endpoints_msn(el: &Element, exists: u32) -> Option<(u32, u32)> {
    let resolve = |e: Endpoint| -> u32 {
        match e {
            Endpoint::Number(n) => n,
            Endpoint::Star => exists,
        }
    };
    let (a, b) = match *el {
        Element::Single(e) => {
            let n = resolve(e);
            (n, n)
        }
        Element::Range { from, to } => (resolve(from), resolve(to)),
    };
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    if lo > exists {
        // Wholly above EXISTS — empty intersection. Includes the
        // single-msn `99` on a 3-msg mailbox and the range `5:99` on
        // a 3-msg mailbox.
        return None;
    }
    // lo is in [1, exists]; clamp hi down to exists. `*` resolves to
    // `exists` so the upper-bound clamp is the only side that needs
    // adjusting.
    Some((lo.max(1), hi.min(exists)))
}

/// Normalise a sequence-set element into a `[lo, hi]` UID range.
/// `Star` resolves to `largest_uid` (or `uid_next - 1` on empty
/// mailbox). Star-containing ranges always include `largest_uid`
/// per RFC 9051 §6.4.8 even if the numeric endpoint exceeds it.
fn element_endpoints_uid(el: &Element, largest_uid: u64) -> (u64, u64) {
    match *el {
        Element::Single(Endpoint::Number(n)) => (n as u64, n as u64),
        Element::Single(Endpoint::Star) => (largest_uid, largest_uid),
        Element::Range { from, to } => {
            let star_present = matches!(from, Endpoint::Star) || matches!(to, Endpoint::Star);
            let a = match from {
                Endpoint::Number(n) => n as u64,
                Endpoint::Star => largest_uid,
            };
            let b = match to {
                Endpoint::Number(n) => n as u64,
                Endpoint::Star => largest_uid,
            };
            let (mut lo, mut hi) = if a <= b { (a, b) } else { (b, a) };
            // RFC 9051 §6.4.8: a range containing `*` always
            // includes the largest UID. `3291:*` on a mailbox whose
            // largest UID is 5 covers UID 5, not the empty set.
            if star_present {
                lo = lo.min(largest_uid);
                hi = hi.max(largest_uid);
            }
            (lo, hi)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn iid(seed: u64) -> ItemId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&seed.to_be_bytes());
        ItemId(Uuid::from_bytes(bytes))
    }

    // ---- parser ----

    #[test]
    fn parse_single_atom() {
        let s = SeqSet::parse("42").unwrap();
        assert_eq!(s.elements(), &[Element::Single(Endpoint::Number(42))]);
    }

    #[test]
    fn parse_star() {
        let s = SeqSet::parse("*").unwrap();
        assert_eq!(s.elements(), &[Element::Single(Endpoint::Star)]);
    }

    #[test]
    fn parse_simple_range() {
        let s = SeqSet::parse("1:5").unwrap();
        assert_eq!(
            s.elements(),
            &[Element::Range {
                from: Endpoint::Number(1),
                to: Endpoint::Number(5),
            }]
        );
    }

    #[test]
    fn parse_range_to_star() {
        let s = SeqSet::parse("9:*").unwrap();
        assert_eq!(
            s.elements(),
            &[Element::Range {
                from: Endpoint::Number(9),
                to: Endpoint::Star,
            }]
        );
    }

    #[test]
    fn parse_star_to_atom() {
        let s = SeqSet::parse("*:5").unwrap();
        assert_eq!(
            s.elements(),
            &[Element::Range {
                from: Endpoint::Star,
                to: Endpoint::Number(5),
            }]
        );
    }

    #[test]
    fn parse_compound() {
        let s = SeqSet::parse("2,4:7,9,12:*").unwrap();
        assert_eq!(
            s.elements(),
            &[
                Element::Single(Endpoint::Number(2)),
                Element::Range {
                    from: Endpoint::Number(4),
                    to: Endpoint::Number(7),
                },
                Element::Single(Endpoint::Number(9)),
                Element::Range {
                    from: Endpoint::Number(12),
                    to: Endpoint::Star,
                },
            ]
        );
    }

    #[test]
    fn parse_max_u32() {
        // RFC 9051 nz-number ceiling. Round-trip the boundary.
        let s = SeqSet::parse("4294967295").unwrap();
        assert_eq!(s.elements(), &[Element::Single(Endpoint::Number(u32::MAX))]);
    }

    #[test]
    fn parse_rejects_empty() {
        let e = SeqSet::parse("").unwrap_err();
        assert!(e.contains("empty"), "{e}");
    }

    #[test]
    fn parse_rejects_whitespace() {
        for s in ["1 2", " 1", "1 ", "1, 2", "1:5 ", "\t1"] {
            let e = SeqSet::parse(s).unwrap_err();
            assert!(e.contains("whitespace"), "{s:?} → {e}");
        }
    }

    #[test]
    fn parse_rejects_zero() {
        for s in ["0", "0:5", "1:0", "1,0,3", "*:0"] {
            let e = SeqSet::parse(s).unwrap_err();
            assert!(e.contains("zero"), "{s:?} → {e}");
        }
    }

    #[test]
    fn parse_rejects_overflow() {
        // u32::MAX + 1
        let e = SeqSet::parse("4294967296").unwrap_err();
        assert!(e.contains("32 bits"), "{e}");
    }

    #[test]
    fn parse_rejects_leading_zero() {
        let e = SeqSet::parse("01").unwrap_err();
        assert!(e.contains("leading zero"), "{e}");
    }

    #[test]
    fn parse_rejects_non_digit() {
        for s in ["a", "1a", "1.5", "-1", "+1", "1e2", "0x5"] {
            let e = SeqSet::parse(s).unwrap_err();
            // "leading zero" applies to 0x5; "not a non-negative integer"
            // catches the rest. Either is acceptable.
            assert!(
                e.contains("non-negative") || e.contains("leading zero"),
                "{s:?} → {e}"
            );
        }
    }

    #[test]
    fn parse_rejects_empty_element() {
        for s in [",1", "1,", "1,,2", ","] {
            let e = SeqSet::parse(s).unwrap_err();
            assert!(e.contains("empty element"), "{s:?} → {e}");
        }
    }

    #[test]
    fn parse_rejects_malformed_range() {
        for s in [":5", "5:", ":", "1:2:3", "1:", ":*"] {
            let e = SeqSet::parse(s).unwrap_err();
            assert!(
                e.contains("empty endpoint") || e.contains("more than one"),
                "{s:?} → {e}"
            );
        }
    }

    // ---- msn resolution ----

    #[test]
    fn resolve_msn_empty_mailbox_is_empty() {
        let map = SequenceMap::from_entries::<[(u64, ItemId); 0]>([]);
        let s = SeqSet::parse("1:*").unwrap();
        assert!(map.resolve_msn(&s).is_empty());
    }

    #[test]
    fn resolve_msn_single() {
        let map = SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3))]);
        let s = SeqSet::parse("2").unwrap();
        let got = map.resolve_msn(&s);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].msn, 2);
        assert_eq!(got[0].uid, 20);
        assert_eq!(got[0].item_id, iid(2));
    }

    #[test]
    fn resolve_msn_star() {
        let map = SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3))]);
        let s = SeqSet::parse("*").unwrap();
        let got = map.resolve_msn(&s);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].msn, 3);
        assert_eq!(got[0].uid, 30);
    }

    #[test]
    fn resolve_msn_range() {
        // Mailbox has 5 messages (uids 10..50 step 10).
        let map = SequenceMap::from_entries((1..=5).map(|i| (i as u64 * 10, iid(i as u64))));
        let s = SeqSet::parse("2:4").unwrap();
        let got = map.resolve_msn(&s);
        assert_eq!(got.iter().map(|r| r.msn).collect::<Vec<_>>(), vec![2, 3, 4]);
        assert_eq!(
            got.iter().map(|r| r.uid).collect::<Vec<_>>(),
            vec![20, 30, 40]
        );
    }

    #[test]
    fn resolve_msn_reversed_range_is_equivalent() {
        // RFC 9051 §9 "2:4 and 4:2 are equivalent".
        let map = SequenceMap::from_entries((1..=5).map(|i| (i as u64 * 10, iid(i as u64))));
        let a = map.resolve_msn(&SeqSet::parse("2:4").unwrap());
        let b = map.resolve_msn(&SeqSet::parse("4:2").unwrap());
        assert_eq!(a, b);
    }

    #[test]
    fn resolve_msn_range_clamps_above_exists() {
        // 3-msg mailbox, request 2:99 → covers msns 2,3 only.
        let map = SequenceMap::from_entries((1..=3).map(|i| (i as u64 * 10, iid(i as u64))));
        let got = map.resolve_msn(&SeqSet::parse("2:99").unwrap());
        assert_eq!(got.iter().map(|r| r.msn).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn resolve_msn_above_exists_is_empty() {
        // 3-msg mailbox, request msn 99 → empty. Clamping a single
        // out-of-range msn to `exists` would silently retarget any
        // out-of-range FETCH / STORE / COPY at the last message,
        // which is a correctness disaster. Resolver intersects with
        // [1, exists] instead of clamping. Per-verb stricter handling
        // (e.g. STORE on OOR msn returning BAD per §6.4.6) is layered
        // on top by the verb handler; the resolver answers "no rows."
        let map = SequenceMap::from_entries((1..=3).map(|i| (i as u64 * 10, iid(i as u64))));
        let got = map.resolve_msn(&SeqSet::parse("99").unwrap());
        assert!(got.is_empty(), "got {got:?}");
    }

    #[test]
    fn resolve_msn_range_wholly_above_exists_is_empty() {
        // 3-msg mailbox, request 5:99 → empty (lo > exists). Distinct
        // from `2:99` which clips to 2..=3 — that case is covered by
        // `resolve_msn_range_clamps_above_exists`.
        let map = SequenceMap::from_entries((1..=3).map(|i| (i as u64 * 10, iid(i as u64))));
        let got = map.resolve_msn(&SeqSet::parse("5:99").unwrap());
        assert!(got.is_empty(), "got {got:?}");
        // Reversed wire order behaves identically (RFC 9051 §9
        // equivalence of `2:4` and `4:2`).
        let got = map.resolve_msn(&SeqSet::parse("99:5").unwrap());
        assert!(got.is_empty(), "got {got:?}");
    }

    #[test]
    fn resolve_msn_dedups_overlapping_elements() {
        let map = SequenceMap::from_entries((1..=5).map(|i| (i as u64 * 10, iid(i as u64))));
        // 2:4,3:5,4 → msns 2,3,4,5 (dedup'd, ascending).
        let got = map.resolve_msn(&SeqSet::parse("2:4,3:5,4").unwrap());
        assert_eq!(
            got.iter().map(|r| r.msn).collect::<Vec<_>>(),
            vec![2, 3, 4, 5]
        );
    }

    // ---- uid resolution ----

    #[test]
    fn resolve_uid_single_hit() {
        let map = SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3))]);
        let got = map.resolve_uid(&SeqSet::parse("20").unwrap(), 31);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, 20);
        assert_eq!(got[0].msn, 2);
    }

    #[test]
    fn resolve_uid_missing_uid_is_silently_dropped() {
        let map = SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3))]);
        let got = map.resolve_uid(&SeqSet::parse("25").unwrap(), 31);
        assert!(got.is_empty());
    }

    #[test]
    fn resolve_uid_range_picks_present_uids_only() {
        // UIDs 10,20,30. Request 15:25 → only UID 20.
        let map = SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3))]);
        let got = map.resolve_uid(&SeqSet::parse("15:25").unwrap(), 31);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, 20);
    }

    #[test]
    fn resolve_uid_star_is_largest_uid() {
        let map = SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3))]);
        let got = map.resolve_uid(&SeqSet::parse("*").unwrap(), 31);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, 30);
    }

    #[test]
    fn resolve_uid_range_with_star_always_covers_largest() {
        // RFC 9051 §6.4.8: `3291:*` on a mailbox whose largest UID is
        // 30 still covers UID 30.
        let map = SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3))]);
        let got = map.resolve_uid(&SeqSet::parse("3291:*").unwrap(), 31);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, 30);
    }

    #[test]
    fn resolve_uid_empty_mailbox_with_star_uses_uidnext_minus_one() {
        // Empty mailbox, UIDNEXT=42. `1:*` covers [1, 41] but the
        // mailbox is empty so no rows.
        let map = SequenceMap::from_entries::<[(u64, ItemId); 0]>([]);
        let got = map.resolve_uid(&SeqSet::parse("1:*").unwrap(), 42);
        assert!(got.is_empty());
    }

    #[test]
    fn resolve_uid_empty_mailbox_star_alone_is_empty() {
        // Empty + bare `*` — RFC 9051 §9 says * is "UIDNEXT value"
        // when empty, but UIDNEXT itself doesn't refer to any
        // message, so the result is empty.
        let map = SequenceMap::from_entries::<[(u64, ItemId); 0]>([]);
        let got = map.resolve_uid(&SeqSet::parse("*").unwrap(), 5);
        assert!(got.is_empty());
    }

    #[test]
    fn resolve_uid_dedups_across_overlapping_ranges() {
        let map =
            SequenceMap::from_entries([(10, iid(1)), (20, iid(2)), (30, iid(3)), (40, iid(4))]);
        let got = map.resolve_uid(&SeqSet::parse("10:25,20:30,30").unwrap(), 41);
        assert_eq!(
            got.iter().map(|r| r.uid).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn from_entries_sorts_on_construction() {
        // Caller passes UIDs out of order; the map normalises so MSN
        // assignment is deterministic.
        let map = SequenceMap::from_entries([(30, iid(3)), (10, iid(1)), (20, iid(2))]);
        let got = map.resolve_msn(&SeqSet::parse("1:*").unwrap());
        assert_eq!(
            got.iter().map(|r| (r.msn, r.uid)).collect::<Vec<_>>(),
            vec![(1, 10), (2, 20), (3, 30)]
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn from_entries_release_build_coalesces_duplicate_uids() {
        // In release builds the constructor is permissive and the
        // duplicate is dropped (first wins by stable sort + dedup).
        // Debug builds catch the violation via debug_assert.
        let map = SequenceMap::from_entries([(10, iid(1)), (10, iid(2)), (20, iid(3))]);
        assert_eq!(map.exists(), 2);
        let got = map.resolve_uid(&SeqSet::parse("10").unwrap(), 21);
        assert_eq!(got.len(), 1, "duplicate UID 10 must resolve to one row");
    }

    #[test]
    fn exists_and_max_uid_report_snapshot_bounds() {
        let empty = SequenceMap::from_entries::<[(u64, ItemId); 0]>([]);
        assert_eq!(empty.exists(), 0);
        assert_eq!(empty.max_uid(), None);
        let m = SequenceMap::from_entries([(7, iid(1)), (42, iid(2))]);
        assert_eq!(m.exists(), 2);
        assert_eq!(m.max_uid(), Some(42));
    }
}
