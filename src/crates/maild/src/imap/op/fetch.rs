//! `FETCH` / `UID FETCH` — RFC 9051 §6.4.5 / §6.4.8.
//!
//! Phase 3 scope so far: metadata atoms (`FLAGS`, `UID`,
//! `RFC822.SIZE`, `INTERNALDATE`) shipped in T2; body atoms
//! `ENVELOPE` (T11a) and `BODYSTRUCTURE` (T11b) shipped via a
//! `MailStore::read_blob` + `mail_parser` round trip; `BODY[*]` /
//! `BODY.PEEK[*]` with top-level `section-msgtext` (empty / HEADER /
//! HEADER.FIELDS\[.NOT\] / TEXT), optional `<offset.length>` partial,
//! and `RFC822` / `RFC822.HEADER` / `RFC822.TEXT` legacy aliases
//! shipped in T12a — including the `\Seen` side-effect for non-PEEK
//! body atoms (RFC 9051 §6.4.5). Still deferred: part-by-part
//! addressing (`BODY[1.2.MIME]`, T12b); the `FAST` / `ALL` / `FULL`
//! macros (depend on body atoms); and `MODSEQ` (gated on `ENABLE
//! CONDSTORE`, Phase 6).
//!
//! ## Wire shape
//!
//! For each resolved message:
//!
//! ```text
//! * <msn> FETCH (<atom1> <value1> <atom2> <value2> ...)\r\n
//! ```
//!
//! followed by a single tagged response (`<tag> OK FETCH completed`).
//! On UID FETCH the server MUST implicitly include the `UID` atom
//! in every response per RFC 9051 §6.4.8; this module honours that
//! whether or not the client explicitly listed it.
//!
//! Atoms are emitted in the order they were parsed from the wire
//! (with an implicit `UID` prepended on UID FETCH if not already
//! present). RFC 9051 §6.4.5 permits any order; clients diff by
//! atom name not position, so preserving wire order keeps test
//! transcripts predictable.
//!
//! ## Sequence resolution
//!
//! The sequence-set is parsed by [`crate::imap::seq::SeqSet`] and
//! resolved against a fresh [`crate::imap::seq::SequenceMap`] built
//! from `MailStore::list_emails_in_mailbox` at FETCH time. Each
//! response carries the resolved msn; the underlying
//! [`crate::mailstore::EmailHandle`] is looked up by `ItemId` to
//! materialise the FLAGS / UID / SIZE / INTERNALDATE values.
//!
//! Building the SequenceMap per-FETCH (rather than at SELECT time
//! and caching on `SelectedMailbox`) keeps T9 small and avoids the
//! cache-coherence question — Phase 5 IDLE replaces this with a
//! snapshot refreshed on notifier deltas. The list call is the
//! same one JMAP `Email/query` issues, so the cost is the existing
//! mailstore read path, not a new hot path.

use std::collections::HashMap;
use std::sync::Arc;

use cosmix_mds::{ContainerId, Flags, ItemId};

use crate::imap::response::{Status, tagged};
use crate::imap::seq::{Resolved, SeqSet, SequenceMap};
use crate::mailstore::{EmailHandle, ListOpts, MailStore, SortKey};

type AccountId = i32;

/// FETCH data items supported in T9. Order in the wire response
/// follows the order the client requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchAtom {
    /// `UID` — RFC 9051 §6.4.5 / §6.4.8. Implicit on UID FETCH.
    Uid,
    /// `FLAGS` — RFC 9051 §6.4.5.
    Flags,
    /// `INTERNALDATE` — server-side received-at, RFC 9051 §6.4.5.
    InternalDate,
    /// `RFC822.SIZE` — on-disk size of the RFC 5322 representation,
    /// RFC 9051 §6.4.5.
    Rfc822Size,
    /// `ENVELOPE` — parsed IMAP envelope structure, RFC 9051 §6.4.5 /
    /// §7.5.2. Requires loading and parsing the RFC 5322 blob; see
    /// [`Self::needs_body`].
    Envelope,
    /// `BODYSTRUCTURE` — recursive MIME structure with extension
    /// fields, RFC 9051 §6.4.5 / §7.5.2. Renders the same data as
    /// `BODY` plus body-fld-md5 / disposition / language / location
    /// per the §7.5.2 `body-ext-1part` / `body-ext-mpart` grammar.
    BodyStructure,
    /// `BODY[<section>]<<partial>>` or `BODY.PEEK[<section>]<<partial>>`
    /// — RFC 9051 §6.4.5 section addressing. T12a scope: top-level
    /// `section-msgtext` (empty / HEADER / HEADER.FIELDS / HEADER.FIELDS.NOT
    /// / TEXT) plus optional `<offset.length>` partial. Part-by-part
    /// addressing (`BODY[1.2.MIME]`) deferred to T12b.
    Body {
        section: BodySection,
        partial: Option<BodyPartial>,
        peek: bool,
        /// `true` if this atom is an `RFC822*` compatibility alias
        /// (renders with the legacy `RFC822*` wire name and, for
        /// non-`HEADER` aliases, marks the message `\Seen`).
        rfc822_alias: Option<Rfc822Alias>,
    },
}

/// Top-level body-section shapes accepted by T12a. RFC 9051 §6.4.5
/// `section-spec`:
///
/// ```text
/// section-msgtext = "HEADER" / "HEADER.FIELDS" SP header-list /
///                   "HEADER.FIELDS.NOT" SP header-list / "TEXT"
/// ```
///
/// `BODY[]` (empty section) is the whole message. Part-addressed
/// sections (`section-part`) defer to T12b.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodySection {
    /// `BODY[]` — entire RFC 5322 message including headers.
    Whole,
    /// `BODY[HEADER]` — header block including the terminating
    /// blank-line CRLF.
    Header,
    /// `BODY[HEADER.FIELDS (Name1 Name2 ...)]` — only the listed
    /// header fields (preserved in message order) plus a trailing
    /// blank-line CRLF.
    HeaderFields(Vec<String>),
    /// `BODY[HEADER.FIELDS.NOT (Name1 Name2 ...)]` — every header
    /// *except* the listed ones, plus the trailing blank-line CRLF.
    HeaderFieldsNot(Vec<String>),
    /// `BODY[TEXT]` — body-text only (everything after the
    /// header/body blank-line separator).
    Text,
}

/// `<offset.length>` partial range. Both are octet counts; `length`
/// is `nz-number` per the grammar (zero-length partial is not
/// permitted by RFC 9051 §9 — the parser rejects it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyPartial {
    pub offset: u32,
    pub length: u32,
}

/// Legacy `RFC822*` atom aliases (RFC 9051 §6.4.5, retained from
/// RFC 3501 for client compatibility). Each maps to a `BODY[*]`
/// shape but renders with the legacy wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rfc822Alias {
    /// `RFC822` ≡ `BODY[]` with `\Seen` side-effect.
    Whole,
    /// `RFC822.HEADER` ≡ `BODY.PEEK[HEADER]` (no `\Seen` side-effect).
    Header,
    /// `RFC822.TEXT` ≡ `BODY[TEXT]` with `\Seen` side-effect.
    Text,
}

impl FetchAtom {
    /// Render the response-name prefix (e.g. `BODY[HEADER]<0>`) into
    /// `out`. The body-section atoms have variable wire names — the
    /// section spec and optional partial offset are echoed verbatim
    /// per RFC 9051 §7.5.2 example traces, so this is not a `&'static
    /// str`.
    ///
    /// Note: `BODY[*]<offset.length>` responses echo only the *offset*
    /// (not the length) — RFC 9051 §6.4.5 says "Server implementations
    /// MUST NOT send any partial body fetch response that contains the
    /// length specifier."
    fn write_wire_name(&self, out: &mut String) {
        match self {
            Self::Uid => out.push_str("UID"),
            Self::Flags => out.push_str("FLAGS"),
            Self::InternalDate => out.push_str("INTERNALDATE"),
            Self::Rfc822Size => out.push_str("RFC822.SIZE"),
            Self::Envelope => out.push_str("ENVELOPE"),
            Self::BodyStructure => out.push_str("BODYSTRUCTURE"),
            Self::Body {
                section,
                partial,
                rfc822_alias,
                ..
            } => {
                if let Some(alias) = rfc822_alias {
                    out.push_str(match alias {
                        Rfc822Alias::Whole => "RFC822",
                        Rfc822Alias::Header => "RFC822.HEADER",
                        Rfc822Alias::Text => "RFC822.TEXT",
                    });
                    // RFC822* aliases do not carry a section / partial
                    // in the response wire-name — the grammar predates
                    // BODY[*] and the section/partial are folded into
                    // the legacy name itself.
                    return;
                }
                out.push_str("BODY[");
                out.push_str(&section.wire_form());
                out.push(']');
                if let Some(p) = partial {
                    out.push('<');
                    out.push_str(&p.offset.to_string());
                    out.push('>');
                }
            }
        }
    }

    /// Whether this atom requires loading the RFC 5322 blob bytes to
    /// render. Metadata atoms (UID / FLAGS / RFC822.SIZE /
    /// INTERNALDATE) read directly from the [`EmailHandle`];
    /// body-derived atoms need a `MailStore::read_blob` +
    /// `mail_parser` round trip. The handler short-circuits the
    /// blob load when *no* atom in the request needs it.
    fn needs_body(&self) -> bool {
        match self {
            Self::Uid | Self::Flags | Self::InternalDate | Self::Rfc822Size => false,
            Self::Envelope | Self::BodyStructure | Self::Body { .. } => true,
        }
    }

    /// True if rendering this atom must mark the message `\Seen` (RFC
    /// 9051 §6.4.5: `BODY[...]` *without* `.PEEK` and the legacy
    /// `RFC822` / `RFC822.TEXT` data items set `\Seen`; `BODY.PEEK[...]`
    /// and `RFC822.HEADER` do not).
    fn sets_seen(&self) -> bool {
        matches!(
            self,
            Self::Body {
                peek: false,
                rfc822_alias: None,
                ..
            } | Self::Body {
                rfc822_alias: Some(Rfc822Alias::Whole | Rfc822Alias::Text),
                ..
            }
        )
    }

    /// Parse a single atom token. Case-insensitive per RFC 9051 §1.2.
    /// Returns the structured [`FetchAtom`] or a human-readable error
    /// that gets wrapped in a tagged `BAD` response upstream.
    fn parse(tok: &str) -> Result<Self, String> {
        // Body / RFC822 family: BODY[...] / BODY.PEEK[...] /
        // RFC822 / RFC822.HEADER / RFC822.TEXT. These need special
        // handling since the `[section]<partial>` suffix is part of
        // the atom syntax (and `RFC822*` aliases lower to body atoms).
        let upper = tok.to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("BODY.PEEK[") {
            return parse_body_atom_tail(tok, rest, true);
        }
        if let Some(rest) = upper.strip_prefix("BODY[") {
            return parse_body_atom_tail(tok, rest, false);
        }
        match upper.as_str() {
            "UID" => Ok(Self::Uid),
            "FLAGS" => Ok(Self::Flags),
            "INTERNALDATE" => Ok(Self::InternalDate),
            "RFC822.SIZE" => Ok(Self::Rfc822Size),
            "ENVELOPE" => Ok(Self::Envelope),
            "BODYSTRUCTURE" => Ok(Self::BodyStructure),
            "RFC822" => Ok(Self::Body {
                section: BodySection::Whole,
                partial: None,
                peek: false,
                rfc822_alias: Some(Rfc822Alias::Whole),
            }),
            "RFC822.HEADER" => Ok(Self::Body {
                section: BodySection::Header,
                partial: None,
                // RFC 9051 §6.4.5: RFC822.HEADER does NOT set \Seen.
                peek: true,
                rfc822_alias: Some(Rfc822Alias::Header),
            }),
            "RFC822.TEXT" => Ok(Self::Body {
                section: BodySection::Text,
                partial: None,
                peek: false,
                rfc822_alias: Some(Rfc822Alias::Text),
            }),
            "BODY" => Err(
                "FETCH atom BODY without [section] is not the BODYSTRUCTURE alias; \
                 use BODYSTRUCTURE explicitly"
                    .to_string(),
            ),
            "BODY.PEEK" => Err("FETCH atom BODY.PEEK requires a [section]".to_string()),
            // The macros expand to atom lists, some of which include
            // body atoms; defer until those atoms are wired.
            "FAST" | "ALL" | "FULL" => {
                Err(format!("FETCH macro {tok:?} not implemented in this phase"))
            }
            "MODSEQ" => Err(
                "FETCH MODSEQ requires ENABLE CONDSTORE (not advertised in this phase)".to_string(),
            ),
            _ => Err(format!("FETCH atom {tok:?} not recognised")),
        }
    }
}

impl BodySection {
    /// Render the section into its `BODY[<section>]` form (without
    /// the outer `BODY[`/`]`). Used by [`FetchAtom::write_wire_name`].
    fn wire_form(&self) -> String {
        match self {
            Self::Whole => String::new(),
            Self::Header => "HEADER".to_string(),
            Self::HeaderFields(names) => format!("HEADER.FIELDS ({})", names.join(" ")),
            Self::HeaderFieldsNot(names) => format!("HEADER.FIELDS.NOT ({})", names.join(" ")),
            Self::Text => "TEXT".to_string(),
        }
    }
}

/// Parse a `BODY[...]` / `BODY.PEEK[...]` atom tail. `tail` is the
/// substring after the opening `[` (with the `BODY[` / `BODY.PEEK[`
/// prefix already stripped) — case-folded to ASCII upper-case.
/// `original` is the verbatim token used in error messages so the
/// client sees what it sent. The HEADER.FIELDS list of names is
/// pulled from `original` rather than `tail` so we preserve sender
/// casing in the wire echo (RFC 9051 §7.5.2 examples preserve case).
fn parse_body_atom_tail(original: &str, tail: &str, peek: bool) -> Result<FetchAtom, String> {
    // Find the matching ']' that closes the section. Header lists
    // contain parens but never another ']' (per RFC 9051 §9 the
    // header-fld-name token excludes brackets), so the *first* ']' is
    // the closer.
    let close_idx = tail
        .find(']')
        .ok_or_else(|| format!("FETCH atom {original:?} missing closing ']'"))?;
    let after_bracket = &tail[close_idx + 1..];

    // Slice the corresponding section out of the *original*
    // (non-uppercased) token so header field names preserve sender
    // casing when echoed back. The prefix length matches in both
    // strings since `to_ascii_uppercase` is byte-length-preserving
    // for ASCII (and BODY[*] tokens are all ASCII at this stage).
    let prefix_len = original.len() - tail.len();
    let original_section = &original[prefix_len..prefix_len + close_idx];

    let section = parse_body_section(original_section)?;
    let partial = parse_partial(after_bracket)?;
    Ok(FetchAtom::Body {
        section,
        partial,
        peek,
        rfc822_alias: None,
    })
}

fn parse_body_section(s: &str) -> Result<BodySection, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(BodySection::Whole);
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper == "HEADER" {
        return Ok(BodySection::Header);
    }
    if upper == "TEXT" {
        return Ok(BodySection::Text);
    }
    // HEADER.FIELDS / HEADER.FIELDS.NOT — case-insensitive on the
    // keyword, parenthesised name list. The list itself is
    // whitespace-separated; we preserve sender casing for the names
    // because the echo in the response is expected to match.
    let (kw, rest) = match split_at_first_paren(trimmed) {
        Some(p) => p,
        None => return Err(format!("FETCH section {s:?} not recognised")),
    };
    let kw_upper = kw.trim().to_ascii_uppercase();
    let names = parse_header_field_list(rest)?;
    if names.is_empty() {
        return Err(format!("FETCH section {s:?} empty header field list"));
    }
    match kw_upper.as_str() {
        "HEADER.FIELDS" => Ok(BodySection::HeaderFields(names)),
        "HEADER.FIELDS.NOT" => Ok(BodySection::HeaderFieldsNot(names)),
        _ => Err(format!(
            "FETCH section {s:?} unsupported (T12a accepts HEADER / HEADER.FIELDS[.NOT] / TEXT / part-spec is T12b)"
        )),
    }
}

/// Split at the first `(` and return `(prefix-before, inside-and-closer)`.
/// Returns `None` if there is no `(`.
fn split_at_first_paren(s: &str) -> Option<(&str, &str)> {
    let idx = s.find('(')?;
    Some((&s[..idx], &s[idx..]))
}

/// Parse `(Name1 Name2 ...)`. Returns the names verbatim.
fn parse_header_field_list(s: &str) -> Result<Vec<String>, String> {
    let s = s.trim();
    let s = s
        .strip_prefix('(')
        .ok_or_else(|| format!("FETCH header-list missing opening '(' in {s:?}"))?;
    let s = s
        .strip_suffix(')')
        .ok_or_else(|| "FETCH header-list missing closing ')'".to_string())?;
    let names: Vec<String> = s.split_ascii_whitespace().map(str::to_string).collect();
    Ok(names)
}

/// Parse an optional `<offset.length>` partial trailer. Empty input →
/// `Ok(None)`; otherwise the grammar requires `<n.m>` with both
/// numbers present (`offset` ≥ 0, `length` > 0).
fn parse_partial(s: &str) -> Result<Option<BodyPartial>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let inner = s
        .strip_prefix('<')
        .and_then(|t| t.strip_suffix('>'))
        .ok_or_else(|| format!("FETCH partial {s:?} not in <offset.length> form"))?;
    let (off, len) = inner
        .split_once('.')
        .ok_or_else(|| format!("FETCH partial {s:?} missing '.'"))?;
    let offset: u32 = off
        .parse()
        .map_err(|_| format!("FETCH partial offset {off:?} not a number"))?;
    let length: u32 = len
        .parse()
        .map_err(|_| format!("FETCH partial length {len:?} not a number"))?;
    if length == 0 {
        return Err("FETCH partial length must be > 0".to_string());
    }
    Ok(Some(BodyPartial { offset, length }))
}

/// Whether this is `FETCH` (msn-set) or `UID FETCH` (uid-set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    Msn,
    Uid,
}

/// Parsed `FETCH` / `UID FETCH` arguments.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub set: SeqSet,
    /// Atoms in wire order. UID FETCH may prepend `Uid` to honour the
    /// implicit-UID rule; see [`parse_args`].
    pub atoms: Vec<FetchAtom>,
    pub mode: FetchMode,
}

/// Parse the argument tail of `FETCH <set> <items>` /
/// `UID FETCH <set> <items>`.
///
/// `<items>` is either a single atom (`FLAGS`) or a parenthesised
/// space-separated list (`(FLAGS UID INTERNALDATE)`). RFC 9051 §6.4.5
/// permits no whitespace inside the sequence-set token. UID FETCH
/// (`mode == FetchMode::Uid`) implicitly includes `UID` per §6.4.8;
/// if the client did not list it the parser prepends it so the wire
/// emitter doesn't need to special-case the UID path.
///
/// Returns `Err(String)` on grammar violation — caller maps to
/// `<tag> BAD <message>\r\n`.
pub fn parse_args(args: &str, mode: FetchMode) -> Result<FetchRequest, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("FETCH requires arguments".to_string());
    }
    // Split off the sequence-set: it ends at the first space (the
    // sequence-set grammar permits no whitespace internally — see
    // RFC 9051 §9 / `imap::seq::SeqSet::parse` rejection tests).
    let (set_tok, rest) = match s.split_once(' ') {
        Some(p) => p,
        None => return Err("FETCH missing data-item list".to_string()),
    };
    if set_tok.is_empty() {
        return Err("FETCH sequence-set is empty".to_string());
    }
    let set = SeqSet::parse(set_tok)?;
    let atoms_src = rest.trim();
    if atoms_src.is_empty() {
        return Err("FETCH missing data-item list".to_string());
    }
    let atom_tokens = parse_atom_list(atoms_src)?;
    if atom_tokens.is_empty() {
        return Err("FETCH data-item list is empty".to_string());
    }
    let mut atoms = Vec::with_capacity(atom_tokens.len() + 1);
    for tok in &atom_tokens {
        atoms.push(FetchAtom::parse(tok)?);
    }
    // RFC 9051 §6.4.8: UID FETCH responses MUST include UID. Prepend
    // it if the client did not list it explicitly so downstream code
    // doesn't need to special-case the implicit path.
    if mode == FetchMode::Uid && !atoms.contains(&FetchAtom::Uid) {
        atoms.insert(0, FetchAtom::Uid);
    }
    Ok(FetchRequest { set, atoms, mode })
}

/// Split a FETCH data-item list into its constituent atom tokens.
///
/// Accepts both forms:
/// * single atom: `FLAGS` / `BODY[HEADER.FIELDS (Subject From)]<0.4096>`
/// * parenthesised list: `(FLAGS UID BODY.PEEK[HEADER.FIELDS (...)])`
///
/// The tokenizer respects `[...]`, `(...)`, and `<...>` nesting so that
/// whitespace inside a body atom's section / header-list / partial
/// does not split the token. Body-atom syntax shipped in T12a.
fn parse_atom_list(s: &str) -> Result<Vec<String>, String> {
    if let Some(rest) = s.strip_prefix('(') {
        let close = rest
            .rfind(')')
            .ok_or_else(|| "FETCH atom list missing closing ')'".to_string())?;
        let tail = rest[close + 1..].trim();
        if !tail.is_empty() {
            return Err(format!(
                "FETCH atom list has trailing tokens after ')': {tail:?}"
            ));
        }
        let inner = &rest[..close];
        tokenize_atoms(inner)
    } else {
        let tokens = tokenize_atoms(s)?;
        if tokens.len() > 1 {
            return Err(
                "FETCH data items must be in parentheses when more than one is requested"
                    .to_string(),
            );
        }
        Ok(tokens)
    }
}

/// Bracket / paren / angle-aware splitter for the FETCH data-item
/// list. Tokens are whitespace-separated *at depth 0* only — whitespace
/// inside `[...]`, `(...)`, or `<...>` is preserved so atoms like
/// `BODY.PEEK[HEADER.FIELDS (Subject From)]<0.4096>` survive as one
/// token. Unbalanced delimiters are a parse error.
fn tokenize_atoms(s: &str) -> Result<Vec<String>, String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut bracket = 0i32;
    let mut paren = 0i32;
    let mut angle = 0i32;
    for c in s.chars() {
        let in_nest = bracket > 0 || paren > 0 || angle > 0;
        if c.is_ascii_whitespace() && !in_nest {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        match c {
            '[' => bracket += 1,
            ']' => {
                bracket -= 1;
                if bracket < 0 {
                    return Err("FETCH atom list: unbalanced ']'".to_string());
                }
            }
            '(' => paren += 1,
            ')' => {
                paren -= 1;
                if paren < 0 {
                    return Err("FETCH atom list: unbalanced ')'".to_string());
                }
            }
            '<' => angle += 1,
            '>' => {
                angle -= 1;
                if angle < 0 {
                    return Err("FETCH atom list: unbalanced '>'".to_string());
                }
            }
            _ => {}
        }
        cur.push(c);
    }
    if bracket != 0 || paren != 0 || angle != 0 {
        return Err(
            "FETCH atom list: unbalanced bracket / paren / angle in data-items".to_string(),
        );
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    Ok(tokens)
}

/// Execute FETCH / UID FETCH for a selected mailbox.
///
/// Caller has already validated `Selected(_)` state and resolved the
/// selected container. T9 does not refresh `EXISTS` / `UIDVALIDITY`
/// from `MailboxRecord` here — those are stamped at SELECT time onto
/// `SelectedMailbox`; Phase 5 IDLE wires the refresh path.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    container: ContainerId,
    store: Arc<dyn MailStore>,
    mode: FetchMode,
) -> Vec<u8> {
    let req = match parse_args(args, mode) {
        Ok(r) => r,
        Err(e) => return tagged(tag, Status::Bad, None, &e).into_bytes(),
    };

    // Build the SequenceMap by listing every email in the container,
    // sorted by `seq` (= UID = MSN ordering per RFC 9051 §2.3.1.1).
    // Use a full snapshot — `limit: u32::MAX` — because FETCH must
    // see the whole mailbox to honour `1:*` and msn references.
    let opts = ListOpts {
        sort_by: SortKey::Seq,
        limit: u32::MAX,
        offset: 0,
    };
    let list_store = store.clone();
    let handles = match tokio::task::spawn_blocking(move || {
        list_store.list_emails_in_mailbox(account_id, container, opts)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "FETCH list_emails_in_mailbox failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "fetch failed").into_bytes();
        }
        Err(e) => {
            tracing::warn!(error = %e, "FETCH spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "fetch failed").into_bytes();
        }
    };

    // SequenceMap takes (uid, item_id); also keep a side-index from
    // item_id → handle so per-row atom emission can read FLAGS /
    // SIZE / INTERNALDATE without a second mailstore round-trip.
    let by_id: HashMap<ItemId, EmailHandle> = handles.iter().map(|h| (h.id, h.clone())).collect();
    let map = SequenceMap::from_entries(handles.iter().map(|h| (h.uid, h.id)));

    // UID FETCH `*` and `<n>:*` rely on UIDNEXT - 1 on an empty
    // mailbox. We don't have a fresh `MailboxRecord` here, but
    // `resolve_uid` only consults `uid_next` when the snapshot is
    // empty *and* the wire references `*`. The empty-snapshot path
    // produces no rows regardless of the sentinel (see seq.rs
    // resolver semantics); pass 0 — the sub-zero `checked_sub` in
    // `resolve_uid` clamps to `Some(0)`.
    let resolved: Vec<Resolved> = match req.mode {
        FetchMode::Msn => map.resolve_msn(&req.set),
        FetchMode::Uid => map.resolve_uid(&req.set, 0),
    };

    let needs_body = req.atoms.iter().any(|a| a.needs_body());
    let needs_seen = req.atoms.iter().any(|a| a.sets_seen());

    let mut out: Vec<u8> = Vec::new();
    for r in &resolved {
        let handle = by_id
            .get(&r.item_id)
            .expect("resolver returned ItemId absent from snapshot");
        // Per-row blob load when any requested atom needs it.
        // Phase 5 IDLE may add a per-mailbox cache, but the per-FETCH
        // path keeps T11a small. A miss / panic logs and emits NIL
        // for the body-derived atoms rather than aborting the whole
        // response — partial FETCH is wire-legal (RFC 9051 §6.4.5
        // does not require a row per requested message) but
        // truncating mid-response is not.
        let body_bytes: Option<Vec<u8>> = if needs_body {
            let store = store.clone();
            let hash = handle.blob_hash;
            match tokio::task::spawn_blocking(move || store.read_blob(hash)).await {
                Ok(Ok(b)) => Some(b),
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        item_id = %handle.id.0,
                        "FETCH read_blob failed; emitting NIL body atoms"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        item_id = %handle.id.0,
                        "FETCH read_blob spawn_blocking panicked; emitting NIL body atoms"
                    );
                    None
                }
            }
        } else {
            None
        };

        // \Seen side-effect (RFC 9051 §6.4.5): BODY[...] without PEEK
        // and the legacy RFC822 / RFC822.TEXT data items mark the
        // message \Seen. Apply the update *before* rendering so the
        // FLAGS atom in this response reflects the new state — clients
        // typically diff returned flags against their local snapshot.
        //
        // Use the per-membership read-modify-write primitive so any
        // peer session's intervening STORE (e.g. \Flagged, \Answered)
        // is preserved — `set_keywords_in_mailbox` would replace the
        // full flag set with the stale snapshot bits and silently
        // clobber concurrent updates.
        //
        // We deliberately do *not* gate the call on the snapshot's
        // `\Seen` bit: a concurrent `STORE -FLAGS (\Seen)` could
        // clear \Seen after the list_emails_in_mailbox snapshot but
        // before this FETCH's side-effect point, in which case the
        // RFC still requires us to mark the message \Seen. The
        // primitive is internally idempotent (it skips the write and
        // the notifier wakeup when every requested bit is already
        // set), so the unconditional call is cheap for the common
        // re-fetch-of-already-seen case.
        let effective_handle: EmailHandle = if needs_seen {
            let store2 = store.clone();
            let item_id = handle.id;
            let container2 = container;
            let aid = account_id;
            match tokio::task::spawn_blocking(move || {
                store2.add_flags_in_mailbox(aid, item_id, container2, Flags(Flags::SEEN))
            })
            .await
            {
                Ok(Ok(merged)) => {
                    let mut updated = handle.clone();
                    updated.keywords = merged;
                    updated
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        item_id = %handle.id.0,
                        "FETCH \\Seen side-effect failed; emitting pre-FETCH flags"
                    );
                    handle.clone()
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        item_id = %handle.id.0,
                        "FETCH \\Seen spawn_blocking panicked; emitting pre-FETCH flags"
                    );
                    handle.clone()
                }
            }
        } else {
            handle.clone()
        };

        out.extend_from_slice(&render_fetch_row(
            r.msn,
            &effective_handle,
            &req.atoms,
            body_bytes.as_deref(),
        ));
    }

    let text = if req.mode == FetchMode::Uid {
        "UID FETCH completed"
    } else {
        "FETCH completed"
    };
    out.extend_from_slice(tagged(tag, Status::Ok, None, text).as_bytes());
    out
}

/// Render a single FETCH response row.
///
/// Returns raw bytes (not `String`) because `BODY[*]` / `RFC822*`
/// literal values can contain arbitrary octets — non-UTF-8 8bit
/// transfer-encoded bodies, raw MIME parts, etc. — which the wire
/// must transmit verbatim. All other atom values (FLAGS, ENVELOPE,
/// BODYSTRUCTURE, INTERNALDATE, etc.) emit pure ASCII so their
/// `String` builders can be appended via `as_bytes()` without loss.
fn render_fetch_row(
    msn: u32,
    handle: &EmailHandle,
    atoms: &[FetchAtom],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut s: Vec<u8> = Vec::with_capacity(96);
    s.extend_from_slice(format!("* {msn} FETCH (").as_bytes());
    let mut first = true;
    // Parse the body lazily — only when a body-derived atom is
    // actually requested. `body == None` means either no body atom
    // was requested or the blob load failed; the second case still
    // emits the atom name with `NIL` so the response shape stays
    // legal and the client can react.
    let parsed: Option<mail_parser::Message<'_>> = match body {
        Some(bytes) => mail_parser::MessageParser::default().parse(bytes),
        None => None,
    };
    for atom in atoms {
        if !first {
            s.push(b' ');
        }
        first = false;
        let mut name = String::new();
        atom.write_wire_name(&mut name);
        s.extend_from_slice(name.as_bytes());
        s.push(b' ');
        match atom {
            FetchAtom::Uid => s.extend_from_slice(handle.uid.to_string().as_bytes()),
            FetchAtom::Flags => s.extend_from_slice(
                super::flags::render_keywords(handle.keywords, &handle.tags).as_bytes(),
            ),
            FetchAtom::Rfc822Size => s.extend_from_slice(handle.size_bytes.to_string().as_bytes()),
            FetchAtom::InternalDate => {
                s.extend_from_slice(render_internaldate(handle.received_at).as_bytes())
            }
            FetchAtom::Envelope => match parsed.as_ref() {
                Some(m) => s.extend_from_slice(render_envelope(m).as_bytes()),
                // RFC 9051 §7.5.2: the `ENVELOPE` *value* is the
                // 10-field parenthesised envelope-structure — the
                // individual fields may be NIL, but the structure
                // itself cannot be a bare NIL. Emit an all-NIL
                // envelope so the wire shape stays grammar-legal
                // even when the body load / parse failed.
                None => s.extend_from_slice(ENVELOPE_ALL_NIL.as_bytes()),
            },
            FetchAtom::BodyStructure => match (parsed.as_ref(), body) {
                (Some(m), Some(bytes)) => {
                    s.extend_from_slice(render_bodystructure(m, bytes).as_bytes())
                }
                // Same rationale as ENVELOPE fallback: bare NIL is
                // not a legal `BODYSTRUCTURE` value — every `body`
                // (§7.5.2 grammar) is parenthesised. Emit the
                // minimal grammar-legal placeholder.
                _ => s.extend_from_slice(BODYSTRUCTURE_FALLBACK.as_bytes()),
            },
            FetchAtom::Body {
                section, partial, ..
            } => match (parsed.as_ref(), body) {
                (Some(m), Some(bytes)) => {
                    s.extend_from_slice(&render_body_section(section, partial.as_ref(), m, bytes));
                }
                // Blob load / parse failed — emit a zero-length literal
                // so the response shape stays legal (`BODY[<section>]
                // <0>` is a valid value for an empty section).
                _ => s.extend_from_slice(b"{0}\r\n"),
            },
        }
    }
    s.extend_from_slice(b")\r\n");
    s
}

/// Render `EmailHandle::received_at` (unix-millis i64) as the
/// IMAP date-time literal `"dd-Mmm-yyyy HH:MM:SS +ZZZZ"`.
///
/// RFC 9051 §9 grammar:
/// ```text
/// date-time = DQUOTE date-day-fixed "-" date-month "-" date-year SP time SP zone DQUOTE
/// date-day-fixed = (SP DIGIT) / 2DIGIT
/// ```
///
/// We emit in UTC (`+0000`); the substrate stores received-at as a
/// UTC unix timestamp so there's no origin-zone information to
/// reproduce. Single-digit days are space-padded per the
/// `date-day-fixed` ABNF.
/// Render an IMAP `nstring` — `NIL` for `None`, `"quoted"` when the
/// content is 7-bit printable without CR/LF/quote/backslash, and a
/// `{N}\r\nbytes` literal otherwise.
///
/// RFC 9051 §4.3 distinguishes quoted strings (no CR/LF, no NUL, no
/// raw `"` / `\`, ≤1000 octets) from literals (everything else). UTF-8
/// header content (decoded by `mail_parser`) routinely triggers the
/// literal branch; the wire shape stays legal in either branch.
fn render_nstring(s: Option<&str>) -> String {
    match s {
        None => "NIL".to_string(),
        Some(v) => render_string(v),
    }
}

/// Render a non-NIL IMAP string per RFC 9051 §4.3. See [`render_nstring`].
fn render_string(s: &str) -> String {
    let bytes = s.as_bytes();
    let needs_literal = bytes.len() > 1000
        || bytes.iter().any(|&b| {
            b == 0
                || b == b'\r'
                || b == b'\n'
                || b == b'"'
                || b == b'\\'
                || !(32..=126).contains(&b)
        });
    if needs_literal {
        format!("{{{}}}\r\n{}", bytes.len(), s)
    } else {
        format!("\"{}\"", s)
    }
}

/// Render an IMAP envelope address-list per RFC 9051 §7.5.2:
///
/// ```text
/// address     = "(" addr-name SP addr-adl SP addr-mailbox SP addr-host ")"
/// addr-list   = "(" 1*address ")" / NIL
/// ```
///
/// - `addr-name` is the display name (personal/full name), `NIL` if absent.
/// - `addr-adl` is the SMTP at-domain-list (source route), almost
///   always `NIL` in modern mail.
/// - `addr-mailbox` is the local-part (before `@`).
/// - `addr-host` is the domain (after `@`).
///
/// `mail_parser::Address` decodes RFC 2047 / RFC 5335 encoded display
/// names automatically; we pass the decoded UTF-8 through
/// [`render_nstring`] so non-ASCII headers project as IMAP literals.
fn render_address_list(addr: Option<&mail_parser::Address<'_>>) -> String {
    let Some(addr) = addr else {
        return "NIL".to_string();
    };
    let list = addr.clone().into_list();
    if list.is_empty() {
        return "NIL".to_string();
    }
    let mut out = String::from("(");
    for a in &list {
        let name = a.name.as_deref();
        let email = a.address.as_deref().unwrap_or("");
        // Split on the *last* '@' so quoted local-parts containing
        // '@' (rare but RFC-legal) project to the right side.
        let (mailbox, host) = match email.rfind('@') {
            Some(idx) => (&email[..idx], &email[idx + 1..]),
            None => (email, ""),
        };
        out.push('(');
        out.push_str(&render_nstring(name));
        out.push(' ');
        // Source-route (addr-adl): not surfaced by mail_parser; always NIL.
        out.push_str("NIL");
        out.push(' ');
        out.push_str(&render_nstring(if mailbox.is_empty() {
            None
        } else {
            Some(mailbox)
        }));
        out.push(' ');
        out.push_str(&render_nstring(if host.is_empty() {
            None
        } else {
            Some(host)
        }));
        out.push(')');
    }
    out.push(')');
    out
}

/// All-NIL envelope rendering, used as the fallback when the body
/// blob fails to load or `mail_parser` rejects the bytes. RFC 9051
/// §7.5.2 requires `ENVELOPE`'s *value* to be the 10-field
/// parenthesised structure — bare `NIL` is not a legal envelope
/// value, even though individual fields may be `NIL`.
const ENVELOPE_ALL_NIL: &str = "(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)";

/// Render the IMAP `ENVELOPE` parenthesised structure for a parsed
/// message (RFC 9051 §7.5.2). The field order is fixed:
///
/// ```text
/// ENVELOPE (date subject from sender reply-to to cc bcc in-reply-to message-id)
/// ```
///
/// Per RFC 9051: `sender` and `reply-to` default to `from` when the
/// corresponding header is absent (rather than `NIL`). `in-reply-to`
/// is a single string carrying the verbatim header value (multiple
/// references separated by SP), `message-id` is a single string
/// including the angle brackets.
///
/// **Known T11a limitation — group addresses.** RFC 9051 §7.5.2
/// represents group addresses (`To: undisclosed-recipients:;`) using
/// a group-start address (`(NIL NIL "groupname" NIL)`) and a
/// group-end address (`(NIL NIL NIL NIL)`) bracketing the group
/// members. We flatten via `mail_parser::Address::into_list()`,
/// which drops the group markers; populated groups lose the group
/// name and empty groups collapse to `NIL`. Acceptable for Thunderbird
/// MVP (group syntax is uncommon in 2026-era mail); revisit if a
/// downstream client surfaces a regression.
fn render_envelope(message: &mail_parser::Message<'_>) -> String {
    // `date` — the literal `Date:` header value, IMAP-string. We use
    // the *raw* header text rather than re-serialising the parsed
    // DateTime so clients see exactly what the sender wrote (some
    // clients diff verbatim). Falls back to NIL when absent.
    let date_raw = message
        .header_raw("Date")
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let subject = message.subject();
    let from = message.from();
    let sender_or_from = message.sender().or(from);
    let reply_to_or_from = message.reply_to().or(from);
    let to = message.to();
    let cc = message.cc();
    let bcc = message.bcc();

    // `in-reply-to` projects as a single string. mail_parser exposes
    // `in_reply_to()` as a `MessageIds` reference iterator; we
    // re-serialise into `<id1> <id2> ...` form (matching the legacy
    // RFC 3501 examples). Empty → NIL.
    let in_reply_to_raw = message.header_raw("In-Reply-To").map(str::trim);
    let in_reply_to = in_reply_to_raw.filter(|s| !s.is_empty());

    let message_id_raw = message.header_raw("Message-ID").map(str::trim);
    let message_id = message_id_raw.filter(|s| !s.is_empty());

    let mut out = String::from("(");
    out.push_str(&render_nstring(date_raw));
    out.push(' ');
    out.push_str(&render_nstring(subject));
    out.push(' ');
    out.push_str(&render_address_list(from));
    out.push(' ');
    out.push_str(&render_address_list(sender_or_from));
    out.push(' ');
    out.push_str(&render_address_list(reply_to_or_from));
    out.push(' ');
    out.push_str(&render_address_list(to));
    out.push(' ');
    out.push_str(&render_address_list(cc));
    out.push(' ');
    out.push_str(&render_address_list(bcc));
    out.push(' ');
    out.push_str(&render_nstring(in_reply_to));
    out.push(' ');
    out.push_str(&render_nstring(message_id));
    out.push(')');
    out
}

/// Fallback `BODYSTRUCTURE` value when the body blob can't be loaded
/// or `mail_parser` rejects the bytes. Synthesises a minimal but
/// grammar-legal `body-type-text` per RFC 9051 §7.5.2:
/// `text/plain` with empty parameters, NIL id/desc, `7BIT` encoding,
/// 0 octets, 0 lines, NIL md5/disposition/language/location.
const BODYSTRUCTURE_FALLBACK: &str =
    "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 0 0 NIL NIL NIL NIL)";

/// Render `BODYSTRUCTURE` for a parsed RFC 5322 message (RFC 9051 §7.5.2).
///
/// The output is a recursive `body` value:
///
/// ```text
/// body            = "(" (body-type-1part / body-type-mpart) ")"
/// body-type-1part = (body-type-basic / body-type-msg / body-type-text)
///                   [SP body-ext-1part]
/// body-type-mpart = 1*body SP media-subtype [SP body-ext-mpart]
/// ```
///
/// `BODYSTRUCTURE` differs from `BODY` only by always emitting the
/// extension fields (`body-ext-1part` / `body-ext-mpart`) per
/// §6.4.5. We emit md5/disposition/language/location for single
/// parts and parameter/disposition/language/location for multipart
/// parts; trailing optional extensions are omitted (clients tolerant
/// per the grammar — extensions are right-extensible).
///
/// `raw` is the verbatim RFC 5322 bytes; `mail_parser`'s
/// `offset_body` / `offset_end` indices index into `raw` and are
/// used to compute `body-fld-octets` (encoded byte count) and
/// `body-fld-lines` (LF count, for TEXT / MESSAGE/RFC822 only).
fn render_bodystructure(message: &mail_parser::Message<'_>, raw: &[u8]) -> String {
    let mut out = String::new();
    render_part(&mut out, message, 0, raw);
    out
}

/// Recursive part renderer. `idx` is an index into `msg.parts`; for
/// nested `MESSAGE/RFC822` parts we recurse into a freshly-parsed
/// inner `Message` (whose own `parts[0]` is its root), since
/// `mail_parser`'s flat-vec model embeds nested messages by value
/// rather than by index.
fn render_part(out: &mut String, msg: &mail_parser::Message<'_>, idx: usize, raw: &[u8]) {
    use mail_parser::{MimeHeaders, PartType};
    let Some(part) = msg.parts.get(idx) else {
        // Defensive: an index past the end can only happen if a
        // multipart's child list references a missing part. Emit the
        // fallback so the wire shape stays legal.
        out.push_str(BODYSTRUCTURE_FALLBACK);
        return;
    };

    match &part.body {
        PartType::Multipart(child_ids) => {
            out.push('(');
            if child_ids.is_empty() {
                // A multipart with no children is malformed; the
                // grammar requires `1*body`. Emit the single-part
                // fallback so we satisfy the `1*body` minimum.
                out.push_str(BODYSTRUCTURE_FALLBACK);
            } else {
                for &cid in child_ids {
                    render_part(out, msg, cid as usize, raw);
                }
            }
            out.push(' ');
            // media-subtype for the multipart (e.g. "MIXED", "ALTERNATIVE")
            let subtype = part
                .content_type()
                .and_then(|ct| ct.c_subtype.as_deref())
                .unwrap_or("MIXED");
            out.push_str(&render_string(&subtype.to_ascii_uppercase()));
            // body-ext-mpart: body-fld-param SP body-fld-dsp SP
            //                 body-fld-lang SP body-fld-loc
            out.push(' ');
            out.push_str(&render_param_list(part.content_type()));
            out.push(' ');
            out.push_str(&render_disposition(part));
            out.push(' ');
            out.push_str(&render_language(part));
            out.push(' ');
            out.push_str(&render_nstring(part.content_location()));
            out.push(')');
        }
        PartType::Text(_) | PartType::Html(_) => {
            let (type_, default_subtype) = match part.body {
                PartType::Text(_) => ("TEXT", "PLAIN"),
                PartType::Html(_) => ("TEXT", "HTML"),
                _ => unreachable!(),
            };
            let subtype = part
                .content_type()
                .and_then(|ct| ct.c_subtype.as_deref())
                .map(|s| s.to_ascii_uppercase())
                .unwrap_or_else(|| default_subtype.to_string());
            render_one_part_header(out, part, type_, &subtype, raw);
            // body-fld-lines for TEXT
            out.push(' ');
            out.push_str(&count_lines(raw, part.offset_body, part.offset_end).to_string());
            // body-ext-1part
            render_ext_1part(out, part);
            out.push(')');
        }
        PartType::Message(inner) => {
            out.push('(');
            out.push_str("\"MESSAGE\" \"RFC822\" ");
            out.push_str(&render_param_list(part.content_type()));
            out.push(' ');
            out.push_str(&render_nstring(part.content_id()));
            out.push(' ');
            out.push_str(&render_nstring(part.content_description()));
            out.push(' ');
            out.push_str(&render_string(
                &part
                    .content_transfer_encoding()
                    .unwrap_or("7BIT")
                    .to_ascii_uppercase(),
            ));
            out.push(' ');
            out.push_str(&octets_of(part).to_string());
            out.push(' ');
            // body-type-msg = ... SP envelope SP body SP body-fld-lines
            //
            // The embedded `Message` has its own `raw_message` buffer
            // (mail_parser 0.11 stores nested messages with local
            // offsets), so the recursion MUST pass the inner buffer —
            // outer offsets would index into the wrong slice and
            // produce nonsense `body-fld-lines` for the inner parts.
            out.push_str(&render_envelope(inner));
            out.push(' ');
            let inner_raw = inner.raw_message.as_ref();
            render_part(out, inner, 0, inner_raw);
            out.push(' ');
            // The wrapper's own `body-fld-lines` (the MESSAGE/RFC822
            // body, i.e. the embedded message viewed as a single blob)
            // is still measured in the outer `raw` slice — that range
            // is what the wrapper's offset_body / offset_end refer to.
            out.push_str(&count_lines(raw, part.offset_body, part.offset_end).to_string());
            render_ext_1part(out, part);
            out.push(')');
        }
        PartType::Binary(_) | PartType::InlineBinary(_) => {
            let (type_, subtype) = match part.content_type() {
                Some(ct) => (
                    ct.c_type.to_ascii_uppercase(),
                    ct.c_subtype
                        .as_deref()
                        .map(|s| s.to_ascii_uppercase())
                        .unwrap_or_else(|| "OCTET-STREAM".to_string()),
                ),
                None => ("APPLICATION".to_string(), "OCTET-STREAM".to_string()),
            };
            render_one_part_header(out, part, &type_, &subtype, raw);
            render_ext_1part(out, part);
            out.push(')');
        }
    }
}

/// Emit the opening `(media-type SP media-subtype SP body-fields)`
/// portion of a single-part body — common to TEXT, MESSAGE/RFC822,
/// and basic (APPLICATION/IMAGE/etc) parts. Closing `)` and any
/// per-type trailing fields (lines, envelope, nested body) are the
/// caller's responsibility.
fn render_one_part_header(
    out: &mut String,
    part: &mail_parser::MessagePart<'_>,
    type_: &str,
    subtype: &str,
    _raw: &[u8],
) {
    use mail_parser::MimeHeaders;
    out.push('(');
    out.push_str(&render_string(type_));
    out.push(' ');
    out.push_str(&render_string(subtype));
    out.push(' ');
    out.push_str(&render_param_list(part.content_type()));
    out.push(' ');
    out.push_str(&render_nstring(part.content_id()));
    out.push(' ');
    out.push_str(&render_nstring(part.content_description()));
    out.push(' ');
    out.push_str(&render_string(
        &part
            .content_transfer_encoding()
            .unwrap_or("7BIT")
            .to_ascii_uppercase(),
    ));
    out.push(' ');
    out.push_str(&octets_of(part).to_string());
}

/// Render the `body-ext-1part` trailing extension fields for a
/// non-multipart body: `SP body-fld-md5 SP body-fld-dsp SP
/// body-fld-lang SP body-fld-loc`. `body-fld-md5` is `NIL` —
/// `mail_parser` does not surface a Content-MD5 header and the
/// substrate has no canonical place to compute one yet (T11b
/// scope cap).
fn render_ext_1part(out: &mut String, part: &mail_parser::MessagePart<'_>) {
    use mail_parser::MimeHeaders;
    out.push(' ');
    out.push_str("NIL"); // body-fld-md5
    out.push(' ');
    out.push_str(&render_disposition(part));
    out.push(' ');
    out.push_str(&render_language(part));
    out.push(' ');
    out.push_str(&render_nstring(part.content_location()));
}

/// Render a `body-fld-param` parenthesised key/value pair list, or
/// `NIL` when there are no parameters. RFC 9051 §7.5.2:
///
/// ```text
/// body-fld-param = "(" string SP string *(SP string SP string) ")" / NIL
/// ```
///
/// Parameter names are upper-cased (mainstream IMAP servers emit
/// `("CHARSET" "us-ascii")`); values are passed through verbatim.
fn render_param_list(ct: Option<&mail_parser::ContentType<'_>>) -> String {
    let Some(ct) = ct else {
        return "NIL".to_string();
    };
    let Some(attrs) = ct.attributes.as_ref() else {
        return "NIL".to_string();
    };
    if attrs.is_empty() {
        return "NIL".to_string();
    }
    let mut out = String::from("(");
    let mut first = true;
    for a in attrs {
        if !first {
            out.push(' ');
        }
        first = false;
        out.push_str(&render_string(&a.name.to_ascii_uppercase()));
        out.push(' ');
        out.push_str(&render_string(&a.value));
    }
    out.push(')');
    out
}

/// Render `body-fld-dsp`: `NIL` or `("disp-type" body-fld-param)`.
/// `disp-type` is one of `inline` / `attachment` (case-insensitive
/// per RFC 2183 §2.1); we upper-case for wire consistency with
/// param-name handling.
fn render_disposition(part: &mail_parser::MessagePart<'_>) -> String {
    use mail_parser::MimeHeaders;
    let Some(dsp) = part.content_disposition() else {
        return "NIL".to_string();
    };
    let mut out = String::from("(");
    out.push_str(&render_string(&dsp.c_type.to_ascii_uppercase()));
    out.push(' ');
    // The disposition's own parameters (filename / size / etc.)
    // share the body-fld-param grammar.
    out.push_str(&render_param_list(Some(dsp)));
    out.push(')');
    out
}

/// Render `body-fld-lang`: `NIL` / `nstring` / `(string +(SP string))`.
/// `mail_parser` exposes Content-Language as a `HeaderValue::Text` or
/// list; we project a single value as `"en"` and a list as `("en" "fr")`.
fn render_language(part: &mail_parser::MessagePart<'_>) -> String {
    use mail_parser::{HeaderValue, MimeHeaders};
    match part.content_language() {
        HeaderValue::Text(t) => render_string(t),
        HeaderValue::TextList(list) if !list.is_empty() => {
            let mut out = String::from("(");
            let mut first = true;
            for s in list {
                if !first {
                    out.push(' ');
                }
                first = false;
                out.push_str(&render_string(s));
            }
            out.push(')');
            out
        }
        _ => "NIL".to_string(),
    }
}

/// Encoded byte count of the part body (`body-fld-octets`).
///
/// Uses `offset_end - offset_body` rather than the decoded payload
/// length — IMAP requires the wire-encoded count (so a base64 part
/// reports its base64 size, not the decoded binary size).
fn octets_of(part: &mail_parser::MessagePart<'_>) -> u64 {
    u64::from(part.offset_end.saturating_sub(part.offset_body))
}

/// LF-count of `raw[start..end]` for `body-fld-lines`. RFC 9051
/// §7.5.2 says line count is for TEXT and MESSAGE/RFC822 only and
/// counts CRLF-terminated lines. Counting LFs is the standard
/// approximation — bare-CR text is non-conforming, and the resulting
/// count is what every mainstream server emits.
fn count_lines(raw: &[u8], start: u32, end: u32) -> u64 {
    let start = start as usize;
    let end = end as usize;
    let end = end.min(raw.len());
    if start >= end {
        return 0;
    }
    raw[start..end].iter().filter(|&&b| b == b'\n').count() as u64
}

/// Render `BODY[<section>]<<partial>>` atom value as the IMAP literal
/// `{N}\r\n<bytes>` form per RFC 9051 §4.3 / §6.4.5.
///
/// Returns raw bytes, not `String`, so 8bit-transfer-encoded bodies
/// and non-UTF-8 octets transit the wire verbatim. The previous
/// `from_utf8_lossy` path substituted U+FFFD for invalid bytes,
/// corrupting the message — `BODY[]` / `RFC822` MUST return the exact
/// bytes the server holds (§6.4.5 "the data items returned by FETCH
/// are exactly the data items requested").
fn render_body_section(
    section: &BodySection,
    partial: Option<&BodyPartial>,
    msg: &mail_parser::Message<'_>,
    raw: &[u8],
) -> Vec<u8> {
    let bytes = body_section_bytes(section, msg, raw);
    let sliced: &[u8] = apply_partial(bytes.as_ref(), partial);
    let mut out = Vec::with_capacity(sliced.len() + 16);
    out.extend_from_slice(format!("{{{}}}\r\n", sliced.len()).as_bytes());
    out.extend_from_slice(sliced);
    out
}

/// Extract the raw bytes for a `BODY[<section>]` request from the
/// top-level (root) part of the message. Part-by-part addressing is
/// T12b scope; T12a only handles message-level section atoms.
fn body_section_bytes<'a>(
    section: &BodySection,
    msg: &'a mail_parser::Message<'_>,
    raw: &'a [u8],
) -> std::borrow::Cow<'a, [u8]> {
    use std::borrow::Cow;
    let Some(root) = msg.parts.first() else {
        return Cow::Borrowed(&[]);
    };
    let raw_len = raw.len();
    let body_start = (root.offset_body as usize).min(raw_len);
    let body_end = (root.offset_end as usize).min(raw_len);
    match section {
        BodySection::Whole => Cow::Borrowed(&raw[..body_end]),
        BodySection::Header => {
            // Header block including the blank-line CRLF separator
            // (which is what offset_body points just past — the body
            // starts after the CRLF). Slicing `raw[..body_start]` thus
            // includes the terminating blank line.
            let slice = &raw[..body_start];
            if has_bare_lf(slice) {
                let mut out = Vec::with_capacity(slice.len() + 8);
                append_crlf_normalised(&mut out, slice);
                Cow::Owned(out)
            } else {
                Cow::Borrowed(slice)
            }
        }
        BodySection::Text => Cow::Borrowed(&raw[body_start..body_end]),
        BodySection::HeaderFields(names) | BodySection::HeaderFieldsNot(names) => {
            let want_match = matches!(section, BodySection::HeaderFields(_));
            let mut out: Vec<u8> = Vec::new();
            for h in &root.headers {
                let name_match = names
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(h.name.as_str()));
                if name_match == want_match {
                    let start = (h.offset_field as usize).min(raw_len);
                    let end = (h.offset_end as usize).min(raw_len);
                    if start >= end {
                        continue;
                    }
                    // Blobs ingested from LF-only sources (maildir
                    // import) carry bare-LF line endings; copying them
                    // verbatim and appending CRLF yields `\n\r\n`,
                    // which strict clients read as an empty line = end
                    // of headers (Thunderbird then shows blank
                    // Subject/From for every such message). Normalise
                    // to CRLF while copying — this also covers folded
                    // continuation lines inside one header.
                    append_crlf_normalised(&mut out, &raw[start..end]);
                    // `offset_end` may or may not include the trailing
                    // CRLF depending on the parser path. Ensure each
                    // emitted header line ends with CRLF so the
                    // section value is RFC-conformant.
                    if !out.ends_with(b"\r\n") {
                        out.extend_from_slice(b"\r\n");
                    }
                }
            }
            // RFC 9051 §6.4.5: HEADER.FIELDS / HEADER.FIELDS.NOT
            // responses include a trailing blank-line CRLF after the
            // selected headers, mirroring the structural shape of the
            // BODY[HEADER] response.
            out.extend_from_slice(b"\r\n");
            Cow::Owned(out)
        }
    }
}

/// True when `bytes` contains a bare LF (one not preceded by CR) —
/// the signature of an LF-only blob that needs CRLF normalisation
/// before being emitted inside an IMAP literal.
fn has_bare_lf(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\n' && (i == 0 || bytes[i - 1] != b'\r'))
}

/// Append `bytes` to `out`, converting every bare LF to CRLF. CRs
/// already present are preserved (an existing CRLF passes through
/// unchanged).
fn append_crlf_normalised(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' && out.last() != Some(&b'\r') {
            out.push(b'\r');
        }
        out.push(b);
    }
}

/// Apply an optional `<offset.length>` partial range, clamping to the
/// available bytes. RFC 9051 §6.4.5: an out-of-range offset returns an
/// empty section value rather than an error.
fn apply_partial<'a>(bytes: &'a [u8], partial: Option<&BodyPartial>) -> &'a [u8] {
    match partial {
        None => bytes,
        Some(p) => {
            let offset = p.offset as usize;
            if offset >= bytes.len() {
                return &[];
            }
            let end = offset.saturating_add(p.length as usize).min(bytes.len());
            &bytes[offset..end]
        }
    }
}

fn render_internaldate(received_at_ms: i64) -> String {
    use chrono::{TimeZone, Utc};
    // EmailHandle.received_at is unix-millis (see Mds spec and the
    // SqliteMailStore populator). Out-of-range timestamps fall back
    // to the unix epoch — a synthetic INTERNALDATE is more useful to
    // clients than crashing or omitting the atom.
    let dt = Utc
        .timestamp_millis_opt(received_at_ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    // `%e` is space-padded day (matches `(SP DIGIT) / 2DIGIT`);
    // `%b` is the English abbreviated month name (RFC 9051 lists
    // `Jan` `Feb` ... `Dec` verbatim).
    format!("\"{}\"", dt.format("%e-%b-%Y %H:%M:%S +0000"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mds::{BlobHash, Seq, Tags};
    use uuid::Uuid;

    fn iid(seed: u64) -> ItemId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&seed.to_be_bytes());
        ItemId(Uuid::from_bytes(bytes))
    }

    fn handle(uid: u64, flags_bits: u32, size: u64, received_ms: i64) -> EmailHandle {
        EmailHandle {
            id: iid(uid),
            blob_hash: BlobHash([0u8; 32]),
            keywords: Flags(flags_bits),
            tags: Tags::new(),
            received_at: received_ms,
            seq: Seq(uid as u32),
            uid,
            mod_seq: 0,
            size_bytes: size,
        }
    }

    // ---- parse_args ----

    #[test]
    fn parse_args_single_atom() {
        let r = parse_args("1:* FLAGS", FetchMode::Msn).unwrap();
        assert_eq!(r.atoms, vec![FetchAtom::Flags]);
        assert_eq!(r.mode, FetchMode::Msn);
    }

    #[test]
    fn parse_args_parenthesised_list_preserves_order() {
        let r = parse_args("1:* (FLAGS UID RFC822.SIZE INTERNALDATE)", FetchMode::Msn).unwrap();
        assert_eq!(
            r.atoms,
            vec![
                FetchAtom::Flags,
                FetchAtom::Uid,
                FetchAtom::Rfc822Size,
                FetchAtom::InternalDate,
            ]
        );
    }

    #[test]
    fn parse_args_uid_mode_prepends_implicit_uid() {
        // UID FETCH 1:* (FLAGS) — UID atom prepended even though
        // client didn't list it.
        let r = parse_args("1:* (FLAGS)", FetchMode::Uid).unwrap();
        assert_eq!(r.atoms, vec![FetchAtom::Uid, FetchAtom::Flags]);
    }

    #[test]
    fn parse_args_uid_mode_does_not_duplicate_explicit_uid() {
        // Client listed UID explicitly — don't insert a second one.
        let r = parse_args("1:* (UID FLAGS)", FetchMode::Uid).unwrap();
        assert_eq!(r.atoms, vec![FetchAtom::Uid, FetchAtom::Flags]);
    }

    #[test]
    fn parse_args_case_insensitive_atom_names() {
        // RFC 9051 §1.2 — keywords are case-insensitive.
        let r = parse_args("1 (flags Uid)", FetchMode::Msn).unwrap();
        assert_eq!(r.atoms, vec![FetchAtom::Flags, FetchAtom::Uid]);
    }

    #[test]
    fn parse_args_rejects_missing_atoms() {
        // `1:*` alone, no data-item list.
        let e = parse_args("1:*", FetchMode::Msn).unwrap_err();
        assert!(e.contains("missing"), "{e}");
    }

    #[test]
    fn parse_args_rejects_empty_input() {
        assert!(parse_args("", FetchMode::Msn).is_err());
        assert!(parse_args("   ", FetchMode::Msn).is_err());
    }

    #[test]
    fn parse_args_rejects_unparen_multi_atom() {
        // `FETCH 1 FLAGS UID` (no parens) is grammar-invalid.
        let e = parse_args("1 FLAGS UID", FetchMode::Msn).unwrap_err();
        assert!(e.contains("parentheses"), "{e}");
    }

    #[test]
    fn parse_args_accepts_rfc822_aliases() {
        // T12a: RFC822 / RFC822.HEADER / RFC822.TEXT are accepted as
        // BODY[*] aliases per RFC 9051 §6.4.5 (retained from RFC 3501
        // for client compatibility).
        let r = parse_args("1 RFC822", FetchMode::Msn).unwrap();
        assert!(matches!(
            r.atoms[0],
            FetchAtom::Body {
                rfc822_alias: Some(Rfc822Alias::Whole),
                peek: false,
                ..
            }
        ));
        let r = parse_args("1 RFC822.HEADER", FetchMode::Msn).unwrap();
        assert!(matches!(
            r.atoms[0],
            FetchAtom::Body {
                rfc822_alias: Some(Rfc822Alias::Header),
                peek: true, // RFC822.HEADER MUST NOT set \Seen
                ..
            }
        ));
        let r = parse_args("1 RFC822.TEXT", FetchMode::Msn).unwrap();
        assert!(matches!(
            r.atoms[0],
            FetchAtom::Body {
                rfc822_alias: Some(Rfc822Alias::Text),
                peek: false,
                ..
            }
        ));
    }

    #[test]
    fn parse_args_accepts_body_atom_shapes() {
        // T12a: BODY[] / BODY[HEADER] / BODY.PEEK[HEADER.FIELDS (...)]
        // with optional <offset.length> partial.
        let r = parse_args("1 BODY[]", FetchMode::Msn).unwrap();
        assert!(matches!(
            r.atoms[0],
            FetchAtom::Body {
                section: BodySection::Whole,
                partial: None,
                peek: false,
                rfc822_alias: None,
            }
        ));
        let r = parse_args("1 BODY[HEADER]", FetchMode::Msn).unwrap();
        assert!(matches!(
            r.atoms[0],
            FetchAtom::Body {
                section: BodySection::Header,
                peek: false,
                ..
            }
        ));
        let r = parse_args("1 BODY.PEEK[HEADER]<0.1024>", FetchMode::Msn).unwrap();
        match &r.atoms[0] {
            FetchAtom::Body {
                section: BodySection::Header,
                partial:
                    Some(BodyPartial {
                        offset: 0,
                        length: 1024,
                    }),
                peek: true,
                rfc822_alias: None,
            } => {}
            other => panic!("unexpected atom shape: {other:?}"),
        }
    }

    #[test]
    fn parse_args_accepts_body_header_fields_with_inner_parens() {
        let r = parse_args(
            "1 (UID BODY.PEEK[HEADER.FIELDS (Subject From)] FLAGS)",
            FetchMode::Msn,
        )
        .unwrap();
        assert_eq!(r.atoms.len(), 3);
        assert_eq!(r.atoms[0], FetchAtom::Uid);
        match &r.atoms[1] {
            FetchAtom::Body {
                section: BodySection::HeaderFields(names),
                peek: true,
                rfc822_alias: None,
                partial: None,
            } => {
                assert_eq!(names, &vec!["Subject".to_string(), "From".to_string()]);
            }
            other => panic!("unexpected atom: {other:?}"),
        }
        assert_eq!(r.atoms[2], FetchAtom::Flags);
    }

    #[test]
    fn parse_args_rejects_zero_length_partial() {
        let e = parse_args("1 BODY[]<0.0>", FetchMode::Msn).unwrap_err();
        assert!(e.contains("length must be > 0"), "{e}");
    }

    #[test]
    fn parse_args_rejects_macros() {
        for m in ["FAST", "ALL", "FULL"] {
            let e = parse_args(&format!("1 {m}"), FetchMode::Msn).unwrap_err();
            assert!(
                e.contains("macro") && e.contains("not implemented"),
                "{m} → {e}"
            );
        }
    }

    #[test]
    fn parse_args_rejects_modseq_without_condstore() {
        let e = parse_args("1 MODSEQ", FetchMode::Msn).unwrap_err();
        assert!(e.contains("CONDSTORE"), "{e}");
    }

    #[test]
    fn parse_args_rejects_bad_sequence_set() {
        // Sequence-set parse errors bubble up unmodified — RFC 9051
        // §9 grammar violations are BAD just like atom errors.
        let e = parse_args("0 FLAGS", FetchMode::Msn).unwrap_err();
        assert!(e.contains("zero"), "{e}");
    }

    #[test]
    fn parse_args_rejects_trailing_tokens_after_paren() {
        let e = parse_args("1 (FLAGS) UID", FetchMode::Msn).unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unclosed_paren_list() {
        let e = parse_args("1 (FLAGS UID", FetchMode::Msn).unwrap_err();
        assert!(e.contains("missing closing"), "{e}");
    }

    // ---- render_internaldate ----

    #[test]
    fn render_internaldate_pads_single_digit_day() {
        // 2023-01-07 12:34:56 UTC — single-digit day must be
        // SP-padded per RFC 9051 §9 date-day-fixed.
        let ms = 1_673_094_896_000_i64;
        let out = render_internaldate(ms);
        assert_eq!(out, "\" 7-Jan-2023 12:34:56 +0000\"");
    }

    #[test]
    fn render_internaldate_two_digit_day() {
        // 2023-11-14 22:13:20 UTC.
        let ms = 1_700_000_000_000_i64;
        let out = render_internaldate(ms);
        assert_eq!(out, "\"14-Nov-2023 22:13:20 +0000\"");
    }

    #[test]
    fn render_internaldate_falls_back_on_invalid_timestamp() {
        // i64::MAX millis is far past chrono's representable range;
        // we fall back to the unix epoch rather than panic.
        let out = render_internaldate(i64::MAX);
        // `%e` for day-of-month: `" 1"` (SP + 1).
        assert_eq!(out, "\" 1-Jan-1970 00:00:00 +0000\"");
    }

    // ---- render_fetch_row ----

    #[test]
    fn render_row_metadata_atoms_in_order() {
        // 2023-11-14 22:13:20 UTC, flags=\Seen, size=4321, uid=42.
        let h = handle(42, 0b0001, 4321, 1_700_000_000_000);
        let row = render_fetch_row(
            7,
            &h,
            &[
                FetchAtom::Uid,
                FetchAtom::Flags,
                FetchAtom::Rfc822Size,
                FetchAtom::InternalDate,
            ],
            None,
        );
        assert_eq!(
            row,
            b"* 7 FETCH (UID 42 FLAGS (\\Seen) RFC822.SIZE 4321 INTERNALDATE \"14-Nov-2023 22:13:20 +0000\")\r\n"
                as &[u8]
        );
    }

    #[test]
    fn render_row_no_atoms_emits_empty_parens() {
        // Empty atom list cannot arise from `parse_args` (which
        // rejects it), but the renderer must still be total — this
        // guards against future callers building requests by hand
        // and emitting malformed wire.
        let h = handle(1, 0, 0, 0);
        let row = render_fetch_row(1, &h, &[], None);
        assert_eq!(row, b"* 1 FETCH ()\r\n" as &[u8]);
    }

    // ---- parse_args / ENVELOPE acceptance ----

    #[test]
    fn parse_args_accepts_envelope() {
        let r = parse_args("1 ENVELOPE", FetchMode::Msn).unwrap();
        assert_eq!(r.atoms, vec![FetchAtom::Envelope]);
        let r = parse_args("1 (UID ENVELOPE FLAGS)", FetchMode::Msn).unwrap();
        assert_eq!(
            r.atoms,
            vec![FetchAtom::Uid, FetchAtom::Envelope, FetchAtom::Flags]
        );
    }

    // ---- render_nstring / render_string ----

    #[test]
    fn render_nstring_none_emits_nil() {
        assert_eq!(render_nstring(None), "NIL");
    }

    #[test]
    fn render_nstring_quoted_ascii() {
        assert_eq!(render_nstring(Some("hello world")), "\"hello world\"");
    }

    #[test]
    fn render_nstring_literal_for_non_ascii() {
        // UTF-8 → literal branch. Byte length is 5 (1+1+3) for "a é".
        let out = render_nstring(Some("a é"));
        assert!(out.starts_with("{4}\r\n"), "{out}");
        assert!(out.ends_with("a é"), "{out}");
    }

    #[test]
    fn render_nstring_literal_for_embedded_quote() {
        let out = render_nstring(Some("she said \"hi\""));
        assert!(out.starts_with("{13}\r\n"));
        assert!(out.ends_with("she said \"hi\""));
    }

    #[test]
    fn render_nstring_literal_for_crlf() {
        let out = render_nstring(Some("a\r\nb"));
        assert!(out.starts_with("{4}\r\n"));
    }

    // ---- render_address_list ----

    #[test]
    fn render_address_list_none_is_nil() {
        assert_eq!(render_address_list(None), "NIL");
    }

    #[test]
    fn render_address_list_single_address_with_name() {
        // Build a minimal mail_parser address by parsing a header.
        let msg = mail_parser::MessageParser::default()
            .parse(b"From: Alice Example <alice@example.com>\r\n\r\nbody")
            .unwrap();
        let out = render_address_list(msg.from());
        assert_eq!(out, "((\"Alice Example\" NIL \"alice\" \"example.com\"))");
    }

    #[test]
    fn render_address_list_multi_address_no_names() {
        let msg = mail_parser::MessageParser::default()
            .parse(b"To: a@x.test, b@y.test\r\n\r\n")
            .unwrap();
        let out = render_address_list(msg.to());
        assert_eq!(
            out,
            "((NIL NIL \"a\" \"x.test\")(NIL NIL \"b\" \"y.test\"))"
        );
    }

    // ---- render_envelope ----

    #[test]
    fn render_envelope_full_message() {
        let raw = b"Date: Wed, 14 Nov 2023 22:13:20 +0000\r\n\
                    Subject: Hello\r\n\
                    From: Alice <alice@x.test>\r\n\
                    Sender: Bot <bot@x.test>\r\n\
                    Reply-To: replies@x.test\r\n\
                    To: Bob <bob@y.test>\r\n\
                    Cc: cc@z.test\r\n\
                    Bcc: bcc@w.test\r\n\
                    In-Reply-To: <prev@x.test>\r\n\
                    Message-ID: <self@x.test>\r\n\
                    \r\n\
                    body";
        let msg = mail_parser::MessageParser::default().parse(raw).unwrap();
        let out = render_envelope(&msg);
        assert_eq!(
            out,
            "(\"Wed, 14 Nov 2023 22:13:20 +0000\" \"Hello\" \
             ((\"Alice\" NIL \"alice\" \"x.test\")) \
             ((\"Bot\" NIL \"bot\" \"x.test\")) \
             ((NIL NIL \"replies\" \"x.test\")) \
             ((\"Bob\" NIL \"bob\" \"y.test\")) \
             ((NIL NIL \"cc\" \"z.test\")) \
             ((NIL NIL \"bcc\" \"w.test\")) \
             \"<prev@x.test>\" \"<self@x.test>\")"
        );
    }

    #[test]
    fn render_envelope_sender_and_reply_to_default_to_from() {
        // RFC 9051 §7.5.2: missing sender / reply-to default to from.
        let raw = b"From: Alice <alice@x.test>\r\nSubject: Hi\r\n\r\n";
        let msg = mail_parser::MessageParser::default().parse(raw).unwrap();
        let out = render_envelope(&msg);
        // sender (3rd addr-list) and reply-to (4th) must match from (2nd).
        assert!(
            out.contains(
                "((\"Alice\" NIL \"alice\" \"x.test\")) \
                 ((\"Alice\" NIL \"alice\" \"x.test\")) \
                 ((\"Alice\" NIL \"alice\" \"x.test\"))"
            ),
            "{out}"
        );
    }

    #[test]
    fn render_envelope_minimal_message_emits_nils() {
        // No headers at all → every field is NIL (mail_parser still
        // parses an empty message into a valid `Message`).
        let msg = mail_parser::MessageParser::default()
            .parse(b"\r\nbody")
            .unwrap();
        let out = render_envelope(&msg);
        assert_eq!(out, "(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)");
    }

    // ---- render_bodystructure ----

    fn parse(raw: &[u8]) -> mail_parser::Message<'_> {
        mail_parser::MessageParser::default().parse(raw).unwrap()
    }

    #[test]
    fn render_bodystructure_text_plain_basic() {
        let raw = b"Content-Type: text/plain; charset=us-ascii\r\n\r\nhello\r\nworld\r\n";
        let msg = parse(raw);
        let out = render_bodystructure(&msg, raw);
        // body-fld-octets = end-body = 14 ("hello\r\nworld\r\n"),
        // body-fld-lines = 2 LFs.
        assert_eq!(
            out,
            "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 14 2 NIL NIL NIL NIL)"
        );
    }

    #[test]
    fn render_bodystructure_text_html_default_charset() {
        let raw = b"Content-Type: text/html\r\n\r\n<p>x</p>";
        let msg = parse(raw);
        let out = render_bodystructure(&msg, raw);
        assert!(
            out.starts_with("(\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\""),
            "{out}"
        );
        assert!(out.ends_with(" NIL NIL NIL NIL)"), "{out}");
    }

    #[test]
    fn render_bodystructure_no_content_type_defaults_to_text_plain() {
        // RFC 5322 / RFC 2045: a message without Content-Type defaults
        // to text/plain; us-ascii.
        let raw = b"\r\nhello\r\n";
        let msg = parse(raw);
        let out = render_bodystructure(&msg, raw);
        assert!(out.starts_with("(\"TEXT\" \"PLAIN\""), "{out}");
    }

    #[test]
    fn render_bodystructure_binary_attachment_with_disposition() {
        let raw = b"Content-Type: application/pdf; name=\"doc.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
                    \r\n\
                    JVBERi0xLjQK\r\n";
        let msg = parse(raw);
        let out = render_bodystructure(&msg, raw);
        assert!(
            out.starts_with("(\"APPLICATION\" \"PDF\" (\"NAME\" \"doc.pdf\") NIL NIL \"BASE64\""),
            "{out}"
        );
        // body-ext-1part: md5 NIL, disposition ("ATTACHMENT" (...)), lang NIL, loc NIL.
        assert!(
            out.contains("(\"ATTACHMENT\" (\"FILENAME\" \"doc.pdf\"))"),
            "{out}"
        );
    }

    #[test]
    fn render_bodystructure_multipart_alternative() {
        let raw = b"Content-Type: multipart/alternative; boundary=\"BB\"\r\n\
                    \r\n\
                    --BB\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    plain\r\n\
                    --BB\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    <p>html</p>\r\n\
                    --BB--\r\n";
        let msg = parse(raw);
        let out = render_bodystructure(&msg, raw);
        // Two children inside the outer parens, subtype "ALTERNATIVE",
        // mpart-ext: param ("BOUNDARY" "BB") NIL NIL NIL.
        assert!(out.starts_with('('), "{out}");
        assert!(out.contains("\"TEXT\" \"PLAIN\""), "{out}");
        assert!(out.contains("\"TEXT\" \"HTML\""), "{out}");
        assert!(out.contains("\"ALTERNATIVE\""), "{out}");
        assert!(out.contains("(\"BOUNDARY\" \"BB\")"), "{out}");
    }

    #[test]
    fn render_bodystructure_nested_message_rfc822_uses_inner_raw_for_lines() {
        // Embedded MESSAGE/RFC822: the wrapper sits inside the outer
        // blob's parts, but the inner Message has its own raw_message
        // buffer with local offsets. The inner text/plain's
        // body-fld-lines MUST be computed against the inner buffer,
        // not the outer blob. We pin this by giving the wrapper a
        // long preamble that, if used as the slice, would inflate the
        // LF count for the inner part.
        let raw = b"Content-Type: message/rfc822\r\n\
                    \r\n\
                    From: inner@x.test\r\n\
                    Subject: inner\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    line one\r\n\
                    line two\r\n\
                    line three\r\n";
        let msg = parse(raw);
        let out = render_bodystructure(&msg, raw);
        // Outer is MESSAGE/RFC822; inner is TEXT/PLAIN with 3 LFs.
        assert!(out.starts_with("(\"MESSAGE\" \"RFC822\""), "{out}");
        assert!(out.contains("\"TEXT\" \"PLAIN\""), "{out}");
        // The inner text/plain has 3 lines. The outer prefix is 2
        // CRLFs; if the renderer reused the outer buffer for the
        // inner part's line count, we'd see 5 instead of 3.
        // Match " 3 NIL" so we don't accidentally hit a different "3" elsewhere.
        assert!(
            out.contains(" 3 NIL NIL NIL NIL)"),
            "expected inner part to report 3 lines, got: {out}"
        );
    }

    #[test]
    fn render_bodystructure_empty_message_emits_text_plain_zero() {
        // Bodyless message: still a valid text/plain with 0 octets / 0 lines.
        let raw = b"\r\n";
        let msg = parse(raw);
        let out = render_bodystructure(&msg, raw);
        assert!(out.starts_with("(\"TEXT\" \"PLAIN\""), "{out}");
        // octets and lines should both be 0.
        assert!(out.contains(" 0 0 NIL NIL NIL NIL)"), "{out}");
    }

    #[test]
    fn render_row_bodystructure_with_nil_blob_emits_fallback() {
        let h = handle(1, 0, 0, 0);
        let row = render_fetch_row(1, &h, &[FetchAtom::BodyStructure], None);
        assert_eq!(
            row,
            b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 0 0 NIL NIL NIL NIL))\r\n"
                as &[u8]
        );
    }

    #[test]
    fn parse_args_accepts_bodystructure() {
        let r = parse_args("1 BODYSTRUCTURE", FetchMode::Msn).unwrap();
        assert_eq!(r.atoms, vec![FetchAtom::BodyStructure]);
    }

    #[test]
    fn render_row_envelope_with_nil_blob_emits_all_nil_envelope() {
        // Body load failed → parsed=None → ENVELOPE renders the
        // all-NIL 10-field envelope structure. Bare `NIL` is *not*
        // a legal envelope value per RFC 9051 §7.5.2 (individual
        // fields may be NIL, the structure itself may not).
        let h = handle(1, 0, 0, 0);
        let row = render_fetch_row(1, &h, &[FetchAtom::Envelope], None);
        assert_eq!(
            row,
            b"* 1 FETCH (ENVELOPE (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL))\r\n" as &[u8]
        );
    }

    // ---- render_body_section (T12a) ----

    fn body_section_str(out: &[u8]) -> String {
        // Test helper: assertions use ASCII fixtures so the literal
        // contents are valid UTF-8 even though the production path
        // returns raw bytes.
        String::from_utf8(out.to_vec()).expect("test fixture is ASCII")
    }

    #[test]
    fn render_body_section_whole_emits_entire_blob() {
        let raw = b"Subject: hi\r\n\r\nhello world";
        let msg = parse(raw);
        let out = render_body_section(&BodySection::Whole, None, &msg, raw);
        assert_eq!(
            body_section_str(&out),
            format!("{{{}}}\r\n{}", raw.len(), std::str::from_utf8(raw).unwrap())
        );
    }

    #[test]
    fn render_body_section_header_includes_blank_line_separator() {
        let raw = b"Subject: hi\r\nFrom: a@b.test\r\n\r\nbody";
        let msg = parse(raw);
        let out = render_body_section(&BodySection::Header, None, &msg, raw);
        // Header block is everything up to and including the blank
        // CRLF separator: "Subject: hi\r\nFrom: a@b.test\r\n\r\n" — 31 octets.
        let header = "Subject: hi\r\nFrom: a@b.test\r\n\r\n";
        assert_eq!(
            body_section_str(&out),
            format!("{{{}}}\r\n{}", header.len(), header)
        );
    }

    #[test]
    fn render_body_section_text_emits_body_only() {
        let raw = b"Subject: hi\r\n\r\nbody bytes\r\n";
        let msg = parse(raw);
        let out = render_body_section(&BodySection::Text, None, &msg, raw);
        let body = "body bytes\r\n";
        assert_eq!(
            body_section_str(&out),
            format!("{{{}}}\r\n{}", body.len(), body)
        );
    }

    #[test]
    fn render_body_section_header_fields_selects_matching_case_insensitive() {
        let raw = b"Subject: hi\r\nFrom: a@b.test\r\nTo: c@d.test\r\n\r\nbody";
        let msg = parse(raw);
        let section = BodySection::HeaderFields(vec!["subject".to_string(), "FROM".to_string()]);
        let out = render_body_section(&section, None, &msg, raw);
        // Expect Subject + From in message order, each terminated by
        // CRLF, plus the trailing blank-line CRLF.
        let want = "Subject: hi\r\nFrom: a@b.test\r\n\r\n";
        assert_eq!(
            body_section_str(&out),
            format!("{{{}}}\r\n{}", want.len(), want)
        );
    }

    #[test]
    fn render_body_section_header_fields_not_excludes_matching() {
        let raw = b"Subject: hi\r\nFrom: a@b.test\r\nTo: c@d.test\r\n\r\nbody";
        let msg = parse(raw);
        let section = BodySection::HeaderFieldsNot(vec!["subject".to_string()]);
        let out = render_body_section(&section, None, &msg, raw);
        // Expect From + To preserved, Subject dropped, trailing CRLF.
        let want = "From: a@b.test\r\nTo: c@d.test\r\n\r\n";
        assert_eq!(
            body_section_str(&out),
            format!("{{{}}}\r\n{}", want.len(), want)
        );
    }

    #[test]
    fn render_body_section_header_fields_normalises_bare_lf_blobs() {
        // LF-only blob, as produced by the 2026-07-12 maildir import.
        // Without normalisation each copied header ends `\n` and the
        // appended CRLF yields `\n\r\n` — an empty line that strict
        // clients (Thunderbird) treat as end-of-headers.
        let raw = b"Subject: hi\nFrom: a@b.test\nTo: c@d.test\n\nbody";
        let msg = parse(raw);
        let section = BodySection::HeaderFields(vec!["subject".to_string(), "from".to_string()]);
        let out = render_body_section(&section, None, &msg, raw);
        let want = "Subject: hi\r\nFrom: a@b.test\r\n\r\n";
        assert_eq!(
            body_section_str(&out),
            format!("{{{}}}\r\n{}", want.len(), want)
        );
    }

    #[test]
    fn render_body_section_header_normalises_bare_lf_blobs() {
        let raw = b"Subject: hi\nFrom: a@b.test\n\nbody";
        let msg = parse(raw);
        let out = render_body_section(&BodySection::Header, None, &msg, raw);
        let want = "Subject: hi\r\nFrom: a@b.test\r\n\r\n";
        assert_eq!(
            body_section_str(&out),
            format!("{{{}}}\r\n{}", want.len(), want)
        );
    }

    #[test]
    fn render_body_section_partial_truncates_and_clamps() {
        let raw = b"Subject: hi\r\n\r\n0123456789";
        let msg = parse(raw);
        // Offset 0, length 5 → first 5 octets of body.
        let p = BodyPartial {
            offset: 0,
            length: 5,
        };
        let out = render_body_section(&BodySection::Text, Some(&p), &msg, raw);
        assert_eq!(out, b"{5}\r\n01234" as &[u8]);
        // Offset 7, length 100 → tail "789" (length clamped).
        let p = BodyPartial {
            offset: 7,
            length: 100,
        };
        let out = render_body_section(&BodySection::Text, Some(&p), &msg, raw);
        assert_eq!(out, b"{3}\r\n789" as &[u8]);
        // Offset beyond end → empty literal.
        let p = BodyPartial {
            offset: 999,
            length: 10,
        };
        let out = render_body_section(&BodySection::Text, Some(&p), &msg, raw);
        assert_eq!(out, b"{0}\r\n" as &[u8]);
    }

    #[test]
    fn render_body_section_preserves_non_utf8_bytes_verbatim() {
        // Regression for the previous `from_utf8_lossy` path that
        // substituted U+FFFD for invalid bytes — corrupting any 8bit
        // transfer-encoded body or raw binary part. The literal byte
        // count must equal the input slice length and the bytes must
        // transit verbatim.
        let mut raw: Vec<u8> = b"Subject: hi\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0xC3, 0x28, 0xA0, 0xFF, 0x00, 0xFE]); // invalid UTF-8 mix
        let msg = parse(&raw);
        let out = render_body_section(&BodySection::Text, None, &msg, &raw);
        let header = b"{6}\r\n";
        assert!(out.starts_with(header), "{out:?}");
        assert_eq!(&out[header.len()..], &[0xC3, 0x28, 0xA0, 0xFF, 0x00, 0xFE]);
    }

    #[test]
    fn render_row_body_with_nil_blob_emits_empty_literal() {
        // Blob load failed (body=None) → BODY[*] atom value is the
        // zero-length literal `{0}\r\n` so the wire response stays
        // grammatically valid.
        let h = handle(1, 0, 0, 0);
        let atom = FetchAtom::Body {
            section: BodySection::Header,
            partial: None,
            peek: true,
            rfc822_alias: None,
        };
        let row = render_fetch_row(1, &h, &[atom], None);
        assert_eq!(row, b"* 1 FETCH (BODY[HEADER] {0}\r\n)\r\n" as &[u8]);
    }

    #[test]
    fn write_wire_name_body_header_fields_echoes_case() {
        let atom = FetchAtom::Body {
            section: BodySection::HeaderFields(vec!["Subject".to_string(), "From".to_string()]),
            partial: Some(BodyPartial {
                offset: 0,
                length: 1024,
            }),
            peek: true,
            rfc822_alias: None,
        };
        let mut out = String::new();
        atom.write_wire_name(&mut out);
        // RFC 9051 §6.4.5: partial response echoes only <offset>, not <offset.length>.
        assert_eq!(out, "BODY[HEADER.FIELDS (Subject From)]<0>");
    }

    #[test]
    fn write_wire_name_rfc822_alias_omits_brackets() {
        // RFC 822 aliases render as the legacy bare name even though
        // they're modelled internally as Body atoms.
        for (alias, name) in [
            (Rfc822Alias::Whole, "RFC822"),
            (Rfc822Alias::Header, "RFC822.HEADER"),
            (Rfc822Alias::Text, "RFC822.TEXT"),
        ] {
            let atom = FetchAtom::Body {
                section: BodySection::Whole,
                partial: None,
                peek: alias == Rfc822Alias::Header,
                rfc822_alias: Some(alias),
            };
            let mut out = String::new();
            atom.write_wire_name(&mut out);
            assert_eq!(out, name);
        }
    }

    #[test]
    fn sets_seen_matches_rfc_9051_semantics() {
        let body_no_peek = FetchAtom::Body {
            section: BodySection::Header,
            partial: None,
            peek: false,
            rfc822_alias: None,
        };
        assert!(body_no_peek.sets_seen());

        let body_peek = FetchAtom::Body {
            section: BodySection::Header,
            partial: None,
            peek: true,
            rfc822_alias: None,
        };
        assert!(!body_peek.sets_seen());

        let rfc822 = FetchAtom::Body {
            section: BodySection::Whole,
            partial: None,
            peek: false,
            rfc822_alias: Some(Rfc822Alias::Whole),
        };
        assert!(rfc822.sets_seen());

        let rfc822_header = FetchAtom::Body {
            section: BodySection::Header,
            partial: None,
            peek: true,
            rfc822_alias: Some(Rfc822Alias::Header),
        };
        // RFC 9051 §6.4.5: RFC822.HEADER does NOT set \Seen.
        assert!(!rfc822_header.sets_seen());

        let rfc822_text = FetchAtom::Body {
            section: BodySection::Text,
            partial: None,
            peek: false,
            rfc822_alias: Some(Rfc822Alias::Text),
        };
        assert!(rfc822_text.sets_seen());

        // Metadata atoms never set \Seen.
        assert!(!FetchAtom::Uid.sets_seen());
        assert!(!FetchAtom::Flags.sets_seen());
        assert!(!FetchAtom::Envelope.sets_seen());
        assert!(!FetchAtom::BodyStructure.sets_seen());
    }
}
