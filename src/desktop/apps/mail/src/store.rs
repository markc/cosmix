//! The message boundary between CosMix Mail and wherever mail comes from.
//!
//! The slice needs a store shape it will not have to throw away when JMAP
//! arrives, so the app talks to [`MessageStore`] and nothing else. The two
//! halves are deliberately split by cost:
//!
//! - [`Summary`] is what a list row can afford: already-formatted display
//!   strings, cloned per bind. No body, no parsing.
//! - [`MessageStore::message`] returns the summary together with the *raw*
//!   [`BodySource`]. Sanitisation is the reader's cost at selection time, not
//!   the store's at load time, because sanitising 50 000 bodies to show one is
//!   the wrong trade and because the cost of sanitise-on-select is exactly what
//!   this slice set out to measure.
//!
//! [`FixtureStore`] holds the corpus behind an `Arc<RwLock<_>>` so the same
//! handle backs both the store and the list model, and so a later JMAP fetch
//! can mutate the corpus — under the *write* lock — between the read-locked
//! binds the model takes. (An earlier version of this sentence said the fetch
//! mutates "under the read lock", which `RwLock` does not permit and the code
//! does not do: every mutation goes through [`FixtureStore::write`].)

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use ctk::prelude::{BodySource, RowId};

use crate::fixtures;

/// A [`RowId`] *proven* to lie in the half of the space stores own.
///
/// The type is the enforcement. Documenting "stores must not issue ids with
/// bit 63 set" and checking it in one constructor leaves every other
/// [`MessageStore`] implementation free to break it — and a JMAP store hashing
/// opaque server strings into `u64` will produce a high-bit id roughly half the
/// time. A store cannot return one of these without going through
/// [`MailRowId::new`], and `new` cannot build one that trespasses, so the
/// partition holds for implementations that do not exist yet.
///
/// The refusal is the point of the `Option`: a mapper that hands over a
/// high-bit hash gets `None` at its own call site, where masking is a visible
/// decision it makes, rather than a silent drop deep inside a constructor.
///
/// **What this type does not prove: an id is not a content revision.** It says
/// *which* message, never *which version* of it. A store may rewrite a message
/// in place under the same id — a JMAP push, a draft being saved — so nothing
/// may cache rendered content against an id and skip work because the id
/// matches. Only the read that produced the content can say whether it is
/// current, which is why [`MessageStore::message`] hands back the pair and why
/// the reader has no "already showing this id" fast path (`reader::swap_body`).
/// If a store ever gains a revision counter, this is the type it belongs beside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailRowId(RowId);

impl MailRowId {
    /// Accept `id` if it belongs to the store's half, else refuse it.
    pub fn new(id: RowId) -> Option<Self> {
        (id.0 & PLACEHOLDER_ID_BIT == 0).then_some(Self(id))
    }

    /// The CTK-facing id. Infallible: the check happened at construction.
    pub fn row_id(self) -> RowId {
        self.0
    }
}

/// One message as a list row needs it: display-ready, body-free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub id: MailRowId,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub date: String,
    pub unread: bool,
}

/// One message as the corpus holds it.
#[derive(Clone, Debug)]
pub struct Message {
    pub summary: Summary,
    /// Untrusted. Crosses CTK's trust boundary only via `BodySource::sanitize`.
    ///
    /// Behind an `Arc` because the fixture corpus is five bodies behind 50 000
    /// rows and one of them is 400 paragraphs (~90 KB). Owning it per message
    /// meant 10 000 copies of that body — close to a gigabyte retained, and
    /// four million `format!` calls at startup — while the doc comment on
    /// [`FixtureStore::synthetic`] claimed the opposite was the whole point of
    /// the design. Sharing costs nothing at read time: [`MessageStore::message`]
    /// hands out an owned clone either way, so the per-selection cost the probe
    /// measures is unchanged.
    pub body: Arc<BodySource>,
}

/// Everything the app can ask about messages.
///
/// Implementations are shared across the Bevy world and the list model, so the
/// trait is `Send + Sync + 'static` and every method takes `&self`.
///
/// **Exactly one method takes an index**, and it hands back an identity.
/// Everything that returns *content* is keyed by [`MailRowId`], and the
/// asymmetry is the point: a position is a fact about one instant of a mutable
/// store, so content fetched by index can belong to a different message than
/// the row it lands in. There used to be a `summary(index)` here, the list used
/// it, and that is exactly the bug it caused — CTK recorded `row_id(index)` and
/// `bind` then read `summary(index)`, so an insertion between the two would
/// label a row A while showing B. Every consumer now goes index → id → content,
/// and the by-index accessor is gone rather than merely unused, because the
/// next consumer would have reached for it too.
pub trait MessageStore: Send + Sync + 'static {
    fn len(&self) -> usize;

    /// Stable identity at `index`, or `None` if the corpus shrank under us.
    fn row_id(&self, index: usize) -> Option<MailRowId>;

    /// Current position of `id`, or `None` if it is gone.
    ///
    /// The inverse of [`row_id`](Self::row_id), and it exists for exactly one
    /// caller: CTK's change hints are expressed in index ranges, so a rebind
    /// request that was *raised* by identity has to be *spoken* as a position.
    /// Resolving it here, at the moment the hint is issued, is what keeps a
    /// position from being stored across frames — see `reader::PendingRebinds`.
    fn index_of(&self, id: MailRowId) -> Option<usize>;

    /// Summary for `id`, without knowing its index.
    ///
    /// Exists because a *selection* carries an id, not a position, and the
    /// reading pane needs the summary of whatever was selected. Resolving that
    /// by scanning `row_id` until it matches is an O(n) per selection — a
    /// linear scan over 50 000 messages on every arrow-key press — and it is
    /// the store's job to avoid it, because only the store knows whether it has
    /// an index. [`FixtureStore`] does.
    ///
    /// The returned summary must be the one belonging to `id`. Nothing checks
    /// it — the app has no second source to check it against — so it is stated
    /// here as the contract it is.
    fn summary_by_id(&self, id: MailRowId) -> Option<Summary>;

    /// Summary **and** body of one message, read as one snapshot.
    ///
    /// Not a convenience wrapper — the two separate calls are a bug whenever the
    /// store can change between them, and they compose into it silently. A reply
    /// built from `summary_by_id` then `body` can quote a body the store swapped
    /// in after the summary was read, so the draft carries the new message's
    /// text under the old one's attribution and subject: a misattributed quote,
    /// sent under the user's name, from a race they cannot see. The reading pane
    /// has the same shape with a milder symptom (a heading over the wrong body).
    ///
    /// Both are fixed by making coherence the store's job, because only the
    /// store holds the lock that can guarantee it. Implementations must read
    /// both under a single acquisition; the default cannot be provided here
    /// precisely because a default would have to call two accessors.
    ///
    /// There is deliberately no body-only accessor beside it. There was, every
    /// consumer wanted the pair anyway, and leaving it on the trait would leave
    /// the next consumer a one-line route back into the split this method
    /// exists to close — the same reason the by-index `summary` is gone.
    fn message(&self, id: MailRowId) -> Option<(Summary, BodySource)>;

    /// Clear the unread flag. `true` when the flag actually changed, so the
    /// caller can skip an `Updated` hint the list would spend work on for no
    /// visual change.
    ///
    /// Deliberately does not hand back the index. It used to, and the index was
    /// then carried in a queue until the end of the frame — by which time an
    /// insertion could have made it name a different message, so the wrong row
    /// was refreshed and the one just read stayed visibly unread. The caller
    /// keeps the id and asks [`index_of`](Self::index_of) at the moment it needs
    /// a position.
    fn mark_read(&self, id: MailRowId) -> bool;
}

/// The half of the [`RowId`] space no store may issue.
///
/// The list model must be able to invent an id when the corpus shrinks between
/// `len` and a bind (`list.rs`), and an invented id that collides with a real
/// one makes CTK treat a placeholder and a message as the same row. "Pick
/// something a store would never use" is not an invariant — so the space is
/// *split* instead: stores own ids with the high bit clear, the list owns ids
/// with it set, and [`MailRowId::new`] is where that is enforced — before a
/// mailbox can be built, not inside one, so every store gets the check without
/// having to remember it. A JMAP store hashing opaque server ids into `u64` masks
/// this bit off; that is a 1-bit narrowing of a hash space, not a constraint
/// worth negotiating.
pub const PLACEHOLDER_ID_BIT: u64 = 1 << 63;

/// The corpus itself.
#[derive(Debug, Default)]
pub struct Mailbox {
    messages: Vec<Message>,
    /// `RowId` → index. Kept beside `messages` because `body` and `mark_read`
    /// are called on every selection change and a linear scan over 50 000
    /// messages per keystroke-speed selection is exactly the accidental
    /// quadratic this slice would otherwise measure and blame on CTK.
    index: HashMap<MailRowId, usize>,
}

impl Mailbox {
    /// Build a mailbox, dropping any message whose id is already taken.
    ///
    /// `RowId` uniqueness is a hard requirement one level up: `virtual_list`
    /// keys row identity and selection on it, and this store resolves `body`
    /// and `mark_read` through the index. A duplicate id silently splits those
    /// two — the list would show both rows while every id-keyed lookup
    /// resolved to whichever one the map kept — so selecting one message could
    /// open, mark read, or reply to the other.
    ///
    /// [`FixtureStore::synthetic`] cannot produce a duplicate (ids are
    /// positions), so this is entirely about the JMAP store that replaces it:
    /// its ids come from a server, and "the server would never" is not an
    /// invariant. Dropping the duplicate keeps list and index agreeing, which
    /// is the property that matters; the alternative — indexing the first and
    /// leaving both in `messages` — is exactly the split described above.
    ///
    /// **Dropping is not silent.** Discarding a duplicate makes mail vanish
    /// from a mailbox, and a store that turns an identity failure into
    /// invisible mail without saying so is worse than one that shows a
    /// duplicate. A real JMAP store must surface this to the user, not just to
    /// the log; the warning here is the floor, not the answer.
    ///
    /// Ids in the list model's reserved half are not checked here because
    /// [`MailRowId`] makes them unrepresentable — that is the whole reason it
    /// exists. An earlier version dropped them at this line instead, which both
    /// left every other [`MessageStore`] free to break the partition and made
    /// the failure mode "the mail is simply gone".
    pub fn new(messages: Vec<Message>) -> Self {
        let mut index = HashMap::with_capacity(messages.len());
        let mut kept = Vec::with_capacity(messages.len());
        let mut dropped = 0usize;
        for message in messages {
            let id = message.summary.id;
            if index.contains_key(&id) {
                dropped += 1;
                continue;
            }
            index.insert(id, kept.len());
            kept.push(message);
        }
        if dropped > 0 {
            bevy::log::warn!(
                "mailbox: dropped {dropped} message(s) whose id was already taken — \
                 those messages are not visible in the list"
            );
        }
        Self {
            messages: kept,
            index,
        }
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Restore the mailbox's invariants after a writer panicked mid-update.
    ///
    /// Cheap to skip and cheap to be wrong about, so it checks the invariant
    /// directly rather than trusting a length comparison: an index of the right
    /// size pointing at the wrong rows is exactly what a half-finished
    /// insert-and-remove leaves behind, and it is the shape that silently opens
    /// the wrong mail.
    ///
    /// Rebuilding the map is not enough on its own, and an earlier version
    /// stopped there. A writer that panics between appending a message and
    /// noticing the id was already present leaves a *duplicate row*, and no
    /// amount of re-indexing fixes that: whichever row loses the map is a row
    /// the list still draws and every id-keyed action resolves past — the exact
    /// split [`Mailbox::new`] refuses at construction. So repair enforces the
    /// same rule `new` does, dropping later duplicates, and the corpus comes out
    /// of a repair in a state `new` would have accepted.
    fn repair(&mut self) {
        let intact =
            self.index.len() == self.messages.len()
                && self.messages.iter().enumerate().all(|(position, message)| {
                    self.index.get(&message.summary.id) == Some(&position)
                });
        if intact {
            return;
        }
        bevy::log::warn!(
            "mailbox: rebuilding the id index after a poisoned write lock — a writer \
             panicked partway through an update"
        );
        // First-wins, matching `new` — and the retain is what makes "first
        // wins" true of `messages` and not merely of the map.
        self.index.clear();
        let index = &mut self.index;
        let mut dropped = 0usize;
        self.messages.retain(|message| {
            let id = message.summary.id;
            if index.contains_key(&id) {
                dropped += 1;
                return false;
            }
            index.insert(id, index.len());
            true
        });
        if dropped > 0 {
            bevy::log::warn!(
                "mailbox: repair dropped {dropped} duplicate message(s) left by the \
                 panicking writer — those messages are not visible in the list"
            );
        }
    }
}

/// A [`MessageStore`] over a synthetic corpus.
///
/// Not a mock in the test sense — it is the real store the slice runs on until
/// a JMAP implementation replaces it, and the app cannot tell the difference
/// because it only ever sees the trait.
#[derive(Clone)]
pub struct FixtureStore {
    mailbox: Arc<RwLock<Mailbox>>,
}

impl FixtureStore {
    /// Build `count` messages by cycling the bundled bodies.
    ///
    /// A small body corpus behind a large row count is on purpose: row
    /// recycling wants many rows, body swapping wants realistic bodies, and
    /// nothing is learned by holding 50 000 distinct HTML documents in memory.
    ///
    /// That last clause used to be aspiration rather than fact — the bodies
    /// were *generated* per message, so the corpus held 50 000 of them anyway
    /// and the comment saying otherwise is what stopped anyone looking. They
    /// are now built once and shared (see [`Message::body`]).
    pub fn synthetic(count: usize) -> Self {
        let bodies = fixtures::bodies();
        // Built once, then shared. `Fixture::body()` *generates* the long body
        // paragraph by paragraph, so calling it per message is not a clone —
        // it is 400 `format!` calls, 10 000 times over.
        let sources: Vec<Arc<BodySource>> = bodies.iter().map(|f| Arc::new(f.body())).collect();
        let messages = (0..count)
            .map(|position| {
                let fixture = &bodies[position % bodies.len()];
                // `position` is far below the reserved bit, so this cannot
                // fail — but it goes through the checked constructor anyway,
                // because "this store's ids are obviously fine" is the exact
                // reasoning the partition exists to stop relying on.
                let id = MailRowId::new(RowId(position as u64))
                    .expect("a corpus position is never in the reserved half");
                Message {
                    summary: Summary {
                        id,
                        from: format!("{} <{}>", fixture.from, fixture.address),
                        subject: format!("{} #{position}", fixture.subject),
                        snippet: fixture.snippet.to_string(),
                        // Formatted here, never at bind time: a list row must
                        // not do work proportional to how often it is recycled.
                        date: format!("{:02}:{:02}", (position / 60) % 24, position % 60),
                        unread: position % 7 == 0,
                    },
                    body: Arc::clone(&sources[position % sources.len()]),
                }
            })
            .collect();
        Self {
            mailbox: Arc::new(RwLock::new(Mailbox::new(messages))),
        }
    }

    /// Read under the lock, recovering from poisoning rather than propagating a
    /// panic into a Bevy system: a panic in one selection must not take the
    /// window down.
    ///
    /// Be exact about what recovery means here, because the first version of
    /// this comment was not: `RwLock` is poisoned when a thread panics while
    /// holding the **write** guard — a reader panicking poisons nothing. So
    /// `into_inner` on a poisoned lock hands back a corpus that a *writer* was
    /// halfway through, and this store's whole design is two structures that
    /// must agree (`messages` and `index`). Recovering blind would resume on a
    /// mailbox whose index pointed at the wrong rows, which is the "selecting
    /// one message opens another" failure the index was built to prevent.
    ///
    /// [`Mailbox::repair`] therefore runs on every recovered guard and rebuilds
    /// the index if the two have drifted. Today's only writer flips one `bool`
    /// and cannot desync them; the check is for the JMAP writer that will
    /// insert and remove.
    fn read<T>(&self, f: impl FnOnce(&Mailbox) -> T) -> T {
        match self.mailbox.read() {
            Ok(guard) => f(&guard),
            Err(poisoned) => {
                // A read guard cannot repair, so this upgrades to the write
                // lock — but it must let go first. `RwLock::read` on a poisoned
                // lock **still acquires the guard** and hands it back inside the
                // `PoisonError`; asking the same non-reentrant lock for write
                // access while that guard is alive deadlocks this thread against
                // itself, which for a Bevy exclusive system is the whole window
                // hanging. Dropping it explicitly is the fix, and the explicit
                // `drop` is load-bearing: letting the binding fall out of scope
                // at the end of the arm is too late, because the write call is
                // inside the arm.
                drop(poisoned);
                self.write(|mailbox| f(mailbox))
            }
        }
    }

    fn write<T>(&self, f: impl FnOnce(&mut Mailbox) -> T) -> T {
        match self.mailbox.write() {
            Ok(mut guard) => f(&mut guard),
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.repair();
                // Clear the flag now the corpus is consistent again, because
                // poisoning is otherwise permanent and this store recovers reads
                // by taking the *write* lock. Leaving it set would turn every
                // later read — one per row bind, so 24 per rebind — into an
                // exclusive lock plus `repair`'s O(n) invariant scan over 50 000
                // messages. A single panic would quietly cost the app every
                // frame it has left.
                self.mailbox.clear_poison();
                f(&mut guard)
            }
        }
    }
}

impl MessageStore for FixtureStore {
    fn len(&self) -> usize {
        self.read(Mailbox::len)
    }

    fn row_id(&self, index: usize) -> Option<MailRowId> {
        self.read(|mailbox| mailbox.messages.get(index).map(|m| m.summary.id))
    }

    fn summary_by_id(&self, id: MailRowId) -> Option<Summary> {
        self.read(|mailbox| {
            let position = *mailbox.index.get(&id)?;
            mailbox.messages.get(position).map(|m| m.summary.clone())
        })
    }

    fn index_of(&self, id: MailRowId) -> Option<usize> {
        self.read(|mailbox| mailbox.index.get(&id).copied())
    }

    fn message(&self, id: MailRowId) -> Option<(Summary, BodySource)> {
        // One `read`, therefore one lock acquisition, therefore one snapshot.
        self.read(|mailbox| {
            let position = *mailbox.index.get(&id)?;
            let message = mailbox.messages.get(position)?;
            Some((message.summary.clone(), (*message.body).clone()))
        })
    }

    fn mark_read(&self, id: MailRowId) -> bool {
        self.write(|mailbox| {
            let Some(&position) = mailbox.index.get(&id) else {
                return false;
            };
            let Some(message) = mailbox.messages.get_mut(position) else {
                return false;
            };
            if !message.summary.unread {
                return false;
            }
            message.summary.unread = false;
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: u64) -> MailRowId {
        MailRowId::new(RowId(raw)).expect("test id is in the store's half")
    }

    /// Index → id → summary, which is the only route the trait offers and the
    /// exact route the app takes. Tests that shortcut it would be testing a
    /// path no consumer has.
    fn summary_at(store: &dyn MessageStore, index: usize) -> Option<Summary> {
        store.summary_by_id(store.row_id(index)?)
    }

    /// The body half of the one accessor that returns content by id.
    ///
    /// A projection of `message`, not a route around it: there is no body-only
    /// accessor on the trait any more, and a test that had its own would be
    /// testing a store shape no consumer has.
    fn body_of(store: &dyn MessageStore, id: MailRowId) -> Option<BodySource> {
        store.message(id).map(|(_, body)| body)
    }

    #[test]
    fn row_id_and_body_agree_on_identity() {
        let store = FixtureStore::synthetic(64);
        let id = store.row_id(9).expect("index 9 is populated");
        assert_eq!(summary_at(&store, 9).expect("summary at 9").id, id);
        assert!(body_of(&store, id).is_some());
    }

    #[test]
    fn out_of_range_reads_are_none_not_panics() {
        let store = FixtureStore::synthetic(4);
        assert_eq!(store.row_id(4), None);
        assert_eq!(summary_at(&store, 4), None);
        assert!(body_of(&store, id(999)).is_none());
        assert!(!store.mark_read(id(999)));
    }

    #[test]
    fn the_reserved_half_cannot_be_expressed_as_a_message_id() {
        // The partition is a type, not a check: a store cannot hand back an id
        // that collides with the list model's shrink placeholder because it
        // cannot build one. A mapper hashing opaque server ids gets the refusal
        // at its own call site, where masking is its visible decision.
        assert!(MailRowId::new(RowId(PLACEHOLDER_ID_BIT)).is_none());
        assert!(MailRowId::new(RowId(u64::MAX)).is_none());
        assert!(MailRowId::new(RowId(PLACEHOLDER_ID_BIT | 7)).is_none());
        assert_eq!(
            MailRowId::new(RowId(PLACEHOLDER_ID_BIT - 1)).map(MailRowId::row_id),
            Some(RowId(PLACEHOLDER_ID_BIT - 1)),
            "the largest id a store may issue must still be accepted"
        );
    }

    #[test]
    fn mark_read_reports_a_rebind_once_and_only_once() {
        let store = FixtureStore::synthetic(16);
        // Index 0 is unread by construction (`position % 7 == 0`).
        let id = store.row_id(0).expect("index 0 is populated");
        assert!(summary_at(&store, 0).expect("summary at 0").unread);
        assert!(store.mark_read(id));
        assert!(!summary_at(&store, 0).expect("summary at 0").unread);
        assert!(
            !store.mark_read(id),
            "a second mark_read must not ask the list to rebind an unchanged row"
        );
    }

    #[test]
    fn a_message_read_as_one_snapshot_agrees_with_itself() {
        let store = FixtureStore::synthetic(32);
        let id = store.row_id(5).expect("index 5 is populated");
        let (summary, source) = store.message(id).expect("the id resolves");
        assert_eq!(summary.id, id);
        // The point of the accessor is that these came from one acquisition;
        // what a test can check is that it agrees with the piecemeal route on a
        // store that is not changing, so the combined path cannot quietly
        // return a *different* message than the two calls it replaces.
        assert_eq!(store.summary_by_id(id).expect("summary").id, summary.id);
        assert_eq!(body_of(&store, id).expect("body"), source);
    }

    #[test]
    fn index_of_inverts_row_id() {
        let store = FixtureStore::synthetic(48);
        for index in [0usize, 1, 23, 47] {
            let id = store.row_id(index).expect("populated");
            assert_eq!(store.index_of(id), Some(index));
        }
        assert_eq!(
            store.index_of(id(9_999)),
            None,
            "an unknown id has no place"
        );
    }

    #[test]
    fn a_summary_is_reached_by_the_id_its_row_advertised() {
        let store = FixtureStore::synthetic(64);
        let advertised = store.row_id(41).expect("index 41 is populated");
        let summary = store
            .summary_by_id(advertised)
            .expect("the advertised id resolves");
        assert_eq!(
            summary.id, advertised,
            "a summary must answer to the id it was fetched by — anything else \
             is the divergence the by-index accessor used to allow"
        );
        assert_eq!(store.summary_by_id(id(9_999)), None);
    }

    #[test]
    fn a_duplicate_row_id_is_dropped_rather_than_split_from_its_index() {
        // The list keys identity on `RowId` and the store resolves bodies
        // through it; keeping both would let a selection open the wrong mail.
        let store = FixtureStore::synthetic(4);
        let mut messages: Vec<Message> = (0..4)
            .map(|index| Message {
                summary: summary_at(&store, index).expect("populated"),
                body: Arc::new(
                    body_of(&store, store.row_id(index).expect("populated")).expect("body"),
                ),
            })
            .collect();
        let duplicate = messages[1].clone();
        messages.push(duplicate);
        let mailbox = Mailbox::new(messages);
        assert_eq!(
            mailbox.len(),
            4,
            "the duplicate must not become a fifth row"
        );
        assert_eq!(
            mailbox.index.len(),
            mailbox.messages.len(),
            "every row must be reachable by its id"
        );
        for (position, message) in mailbox.messages.iter().enumerate() {
            assert_eq!(mailbox.index.get(&message.summary.id), Some(&position));
        }
    }

    #[test]
    fn a_repair_restores_the_index_a_panicking_writer_left_wrong() {
        // The shape that matters is not a *missing* index but a wrong one: an
        // index of the right length pointing at the wrong rows is what a
        // half-finished insert-and-remove leaves behind, and it resolves a
        // selection to the wrong mail rather than to nothing.
        let store = FixtureStore::synthetic(8);
        let messages: Vec<Message> = (0..8)
            .map(|index| Message {
                summary: summary_at(&store, index).expect("populated"),
                body: Arc::new(
                    body_of(&store, store.row_id(index).expect("populated")).expect("body"),
                ),
            })
            .collect();
        let mut mailbox = Mailbox::new(messages);
        let scrambled: Vec<MailRowId> = mailbox.index.keys().copied().collect();
        for (position, id) in scrambled.iter().enumerate() {
            mailbox.index.insert(*id, (position + 3) % 8);
        }
        assert_eq!(mailbox.index.len(), mailbox.messages.len());
        mailbox.repair();
        for (position, message) in mailbox.messages.iter().enumerate() {
            assert_eq!(
                mailbox.index.get(&message.summary.id),
                Some(&position),
                "repair must restore every id to its own row"
            );
        }
    }

    #[test]
    fn a_repair_drops_a_duplicate_row_the_panicking_writer_appended() {
        // The wrong index is one shape of half-finished write; a duplicate row
        // is the other, and re-indexing alone cannot fix it — the losing row
        // stays in `messages`, so the list draws two rows that every id-keyed
        // action collapses into one.
        let store = FixtureStore::synthetic(6);
        let messages: Vec<Message> = (0..6)
            .map(|index| Message {
                summary: summary_at(&store, index).expect("populated"),
                body: Arc::new(
                    body_of(&store, store.row_id(index).expect("populated")).expect("body"),
                ),
            })
            .collect();
        let mut mailbox = Mailbox::new(messages);
        let duplicate = mailbox.messages[2].clone();
        mailbox.messages.push(duplicate);

        mailbox.repair();

        assert_eq!(mailbox.messages.len(), 6, "the duplicate row must be gone");
        assert_eq!(mailbox.index.len(), mailbox.messages.len());
        for (position, message) in mailbox.messages.iter().enumerate() {
            assert_eq!(
                mailbox.index.get(&message.summary.id),
                Some(&position),
                "every surviving row must be reachable by its own id"
            );
        }
    }

    #[test]
    fn a_read_after_a_poisoned_write_recovers_instead_of_hanging() {
        // `read` recovers from poison by upgrading to the write lock, and the
        // first version of that recovery deadlocked: `RwLock::read` on a
        // poisoned lock still *acquires* the guard before handing it back inside
        // the error, so asking the same lock for write access while the binding
        // was alive parked the thread against itself forever. In the app that
        // thread is the one running the exclusive system, so the symptom is a
        // frozen window with no panic and no log line — the worst shape of bug
        // to find in the field and, unusually, one a test cannot fail on
        // directly: a deadlocked test hangs the whole harness rather than
        // failing. Hence the deadline.
        let store = FixtureStore::synthetic(8);
        let poisoner = store.clone();
        let died = std::thread::spawn(move || {
            let _guard = poisoner.mailbox.write().expect("the lock starts clean");
            panic!("a writer died mid-update");
        })
        .join();
        assert!(died.is_err(), "the helper thread must actually panic");
        assert!(store.mailbox.is_poisoned());

        let (tx, rx) = std::sync::mpsc::channel();
        let reader = store.clone();
        std::thread::spawn(move || {
            let _ = tx.send(reader.len());
        });
        let len = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a read over a poisoned lock must recover, not deadlock");
        assert_eq!(len, 8);

        // And it recovers *once*. Leaving the flag set would send every later
        // read down the exclusive-lock-plus-full-repair path for the life of the
        // process, which is a permanent per-frame cost paid for a transient
        // fault.
        assert!(
            !store.mailbox.is_poisoned(),
            "recovery must clear the flag, not re-pay for it every read"
        );
        assert_eq!(store.len(), 8);
    }

    #[test]
    fn every_synthetic_id_resolves_through_the_index() {
        let store = FixtureStore::synthetic(200);
        for index in 0..store.len() {
            let id = store.row_id(index).expect("index within len");
            assert!(
                body_of(&store, id).is_some(),
                "id {id:?} at index {index} must resolve"
            );
        }
    }
}
