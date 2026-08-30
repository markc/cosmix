//! JMAP Email methods (RFC 8621).

use std::sync::Arc;

use anyhow::Result;
use cosmix_maild_bayesian::{
    DefaultClassifier,
    classifier::Classifier,
    types::{Label, RetrainRequest},
};
use cosmix_maild_rules::AccountId;
use cosmix_mds::{BlobHash, ContainerId, Flags, ItemId, Mds, Tags};
use mail_parser::HeaderValue;
use uuid::Uuid;

use super::types::*;
use crate::db::{self, Db};
use crate::mailstore::{
    EmailEnvelope, EmailQueryFilter, EmailRecord, EmailSort, ListOpts, MailStore, MailboxRole,
    SqliteMailStore,
};

/// Project [`EmailRecord`] onto the JMAP `Email/get` wire shape (RFC
/// 8621 §4.1).
///
/// **Wire-shape contract during migration.** The full JMAP key set
/// is emitted (so clients see a stable shape), but several values
/// that have not yet migrated to the mds `mail_envelopes` sidecar
/// project as `null` / `false`:
///
/// - `threadId`, `inReplyTo`, `cc`, `preview`, `hasAttachment`,
///   `spamScore`, `spamVerdict` — these lived only on the legacy
///   `db::email` row and have no counterpart in `mail_envelopes`
///   v1.2.
/// - `from` / `to` project as bare strings / string arrays from
///   `EmailEnvelope`, not the RFC 8621 §4.1.2 `[EmailAddress]`
///   object form the legacy table stored.
///
/// A naïve fix — overlaying values from `db::email::get_by_ids`
/// keyed by `EmailRecord.id` — does **not** work: legacy
/// `db::email::create` allocates ids via `Uuid::new_v4()`
/// (`db/email.rs:235`), while mds `MailStore::create_email`
/// allocates via `Uuid::now_v7()`
/// (`cosmix-mds/src/container.rs:2248`). The two id spaces are
/// disjoint; no row is ever looked up by the wrong id.
///
/// The proper fix is substrate-shaping work: extend
/// `mail_envelopes` to carry the missing fields and add a reverse
/// `thread_for_email` MailStore method. That work is tracked as
/// C6 in `_journal/2026-05-08-codex-replay-sonnet-provisional.md`
/// (Email-side substrate task). Until then this projection
/// preserves the wire keys so JMAP clients do not see fields
/// silently disappear; the values fill in once the substrate
/// catches up.
///
/// `keywords` collapses [`Flags`] into the JMAP keyword map. Bits
/// allocated by `cosmix-mds` (see [`Flags`] doc-comment in
/// `cosmix-mds/src/types.rs`) — `\Seen`, `\Flagged`, `\Answered`,
/// `\Draft` — project as `$seen`, `$flagged`, `$answered`, `$draft`.
/// `\Deleted` has no JMAP counterpart (RFC 8621 routes deletion
/// through `Email/set destroy`, not a keyword toggle) and is
/// dropped. RFC 8621 §4.1.4 keywords without a substrate bit
/// (`$forwarded`, `$junk`, `$notjunk`, `$phishing`) require
/// additional bit allocations in `cosmix-mds` plus per-membership
/// `Tags` projection — tracked as C6 alongside the envelope-field
/// substrate work.
///
/// `blobId` is the CAS hash hex — stable across accounts and the
/// post-migration form. The companion `blob_download` handler
/// accepts both the legacy UUID form (for emails uploaded via
/// `db::blob`) and the CAS hex form, gating the CAS branch on
/// per-account ownership via `db::blob`.
///
/// `receivedAt` and `date` project as JMAP `UTCDate` strings (RFC
/// 8620 §1.4 / RFC 3339, `Z`-suffixed) rather than raw integers.
/// `EmailRecord.received_at` is unix-millis, `EmailEnvelope.date` is
/// unix-seconds — both are normalised here.
fn record_to_jmap(r: &EmailRecord) -> serde_json::Value {
    // RFC 8621 §4.1.1: `mailboxIds` is `Id[Boolean]` — an object whose
    // keys are mailbox ids and whose values are always `true`. Set's
    // `update` patch already parses the map shape on the input side
    // (see `email.rs:set` mailboxIds branch), so emitting the map shape
    // here keeps the round-trip symmetric.
    let mut mailbox_ids = serde_json::Map::with_capacity(r.mailbox_ids.len());
    for c in &r.mailbox_ids {
        mailbox_ids.insert(c.0.to_string(), serde_json::Value::Bool(true));
    }
    let message_id = r
        .envelope
        .message_id
        .as_ref()
        .map(|m| serde_json::Value::Array(vec![serde_json::Value::String(m.clone())]))
        .unwrap_or(serde_json::Value::Null);

    let to_value = serde_json::Value::Array(
        r.envelope
            .to
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );
    let cc_value = serde_json::Value::Array(
        r.envelope
            .cc
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );
    let bcc_value = serde_json::Value::Array(
        r.envelope
            .bcc
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );
    let reply_to_value = serde_json::Value::Array(
        r.envelope
            .reply_to
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );

    let received_at = utc_millis_to_jmap(r.received_at);
    let date = utc_seconds_to_jmap(r.envelope.date);

    serde_json::json!({
        "id": r.id.0.to_string(),
        "blobId": cosmix_mds::blob::hex(&r.blob_hash),
        // Substrate-pending (C6) — see fn doc.
        "threadId": serde_json::Value::Null,
        "size": r.size_bytes,
        "receivedAt": received_at,
        "mailboxIds": serde_json::Value::Object(mailbox_ids),
        "keywords": flags_and_tags_to_keywords(r.keywords, &r.tags),
        "subject": r.envelope.subject,
        "messageId": message_id,
        // Substrate-pending (C6).
        "inReplyTo": serde_json::Value::Null,
        // `from` / `to` / `cc` / `bcc` / `replyTo` are projected as
        // bare strings / arrays of bare addr-spec strings — the RFC
        // 8621 §4.1.2 `[EmailAddress]` object form (with separate
        // `name` / `email` fields) remains substrate-pending C6.
        // The recipient lists themselves are now substrate-backed
        // (cosmix-mds v1.3 schema).
        "from": r.envelope.from,
        "to": to_value,
        "cc": cc_value,
        "bcc": bcc_value,
        "replyTo": reply_to_value,
        "date": date,
        // Substrate-pending (C6).
        "preview": serde_json::Value::Null,
        // `hasAttachment` is substrate-pending (C6): MIME structure
        // is not yet derived during ingest. Projecting `false` would
        // be a definitive negative answer that would silently flip
        // for messages that genuinely had attachments under the
        // legacy projection — null preserves the wire key while
        // signaling "unknown" until C6 derives it.
        "hasAttachment": serde_json::Value::Null,
        "spamScore": serde_json::Value::Null,
        "spamVerdict": serde_json::Value::Null,
    })
}

/// Format a unix-millis timestamp as a JMAP `UTCDate` (RFC 3339
/// with a literal `Z` suffix). Out-of-range inputs (extremely large
/// negatives / positives that overflow `chrono::DateTime`) project
/// as `null`; the alternative — surfacing an error — would force
/// every `Email/get` row through `Result` plumbing for a case that
/// can only arise from corrupted state.
fn utc_millis_to_jmap(millis: i64) -> serde_json::Value {
    match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis) {
        Some(dt) => serde_json::Value::String(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
        None => serde_json::Value::Null,
    }
}

/// Format a unix-seconds timestamp as a JMAP `UTCDate`.
/// See [`utc_millis_to_jmap`] for the out-of-range disposition.
fn utc_seconds_to_jmap(secs: i64) -> serde_json::Value {
    match chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) {
        Some(dt) => serde_json::Value::String(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        None => serde_json::Value::Null,
    }
}

/// Apply the JMAP `properties` filter (RFC 8620 §5.1) to a built
/// `Email/get` row. `id` is always retained per the spec ("the id
/// property MUST always be returned"). When `properties` is `None`
/// (caller omitted the argument) the full projection is returned
/// unchanged. An empty list is honored as "id only".
fn apply_email_properties(
    val: serde_json::Value,
    properties: Option<&[String]>,
) -> serde_json::Value {
    let Some(props) = properties else {
        return val;
    };
    let serde_json::Value::Object(map) = val else {
        return val;
    };
    let mut out = serde_json::Map::new();
    if let Some(id) = map.get("id") {
        out.insert("id".to_string(), id.clone());
    }
    for p in props {
        if p == "id" {
            continue;
        }
        if let Some(v) = map.get(p) {
            out.insert(p.clone(), v.clone());
        }
    }
    serde_json::Value::Object(out)
}

/// Map an MDS [`Flags`] bitmap onto a JMAP keyword object.
///
/// Allocated bits (see [`Flags`] doc-comment in
/// `cosmix-mds/src/types.rs`):
///   * bit 0 `\Seen`     → `$seen`
///   * bit 1 `\Flagged`  → `$flagged`
///   * bit 2 `\Answered` → `$answered`
///   * bit 3 `\Draft`    → `$draft`
///
/// `\Deleted` (bit 4) has no JMAP keyword equivalent (RFC 8621
/// intentionally omits it; deletion is `Email/set destroy`, not a
/// keyword toggle) and is dropped on the JMAP projection. An empty
/// `Flags` projects to `{}` (a JMAP-conformant empty keyword set).
fn flags_to_keywords(flags: Flags) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if flags.0 & Flags::SEEN != 0 {
        map.insert("$seen".into(), serde_json::Value::Bool(true));
    }
    if flags.0 & Flags::FLAGGED != 0 {
        map.insert("$flagged".into(), serde_json::Value::Bool(true));
    }
    if flags.0 & Flags::ANSWERED != 0 {
        map.insert("$answered".into(), serde_json::Value::Bool(true));
    }
    if flags.0 & Flags::DRAFT != 0 {
        map.insert("$draft".into(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(map)
}

/// Project the full JMAP `Email.keywords` object: the system flags (via
/// [`flags_to_keywords`]) unioned with the user keywords (`Tags`), each as a
/// `"<name>": true` entry (RFC 8621 §4.1.4 — `keywords` carries both system
/// and user keyword names in one object). A user keyword can never collide
/// with a `$`-system name because the set path rejects the four system
/// `$`-names as user keywords (see [`crate::keyword`]).
fn flags_and_tags_to_keywords(flags: Flags, tags: &Tags) -> serde_json::Value {
    let mut v = flags_to_keywords(flags);
    if let Some(map) = v.as_object_mut() {
        for t in tags.iter() {
            map.insert(t.clone(), serde_json::Value::Bool(true));
        }
    }
    v
}

/// Split a JMAP `Email.keywords` value (RFC 8621 §4.1.4 — an `Id[Boolean]`
/// object whose keys are keyword names and whose `true` values mean "set")
/// into the system [`Flags`] bitmap and the user-keyword [`Tags`] set.
///
/// The four allocated system keywords (`$seen`, `$flagged`, `$answered`,
/// `$draft`) route to `Flags`; every other name — non-`$`, OR a registry
/// `$`-keyword like `$forwarded` / `$label2` that real clients use — is
/// normalised (NFC + lowercase, see [`crate::keyword`]) into `Tags`. A
/// malformed keyword (atom-special/whitespace), a mixed-case variant of a
/// system name, or more than [`crate::keyword::MAX_KEYWORDS_PER_MESSAGE`] are
/// **rejected** ([`crate::keyword::KeywordError`], which the caller maps to
/// `invalidProperties`). `$deleted` is not part of the JMAP keyword surface
/// (deletion is `Email/set destroy`).
///
/// A non-object `keywords` value (e.g. `null`, an array, a string) folds to
/// the empty keyword set rather than erroring; the caller has already
/// shape-validated that, if present, the value is an object whose every entry
/// maps to `true`, and "missing/empty `keywords`" is the JMAP "no keywords"
/// wire form. Entries whose value is not `true` are skipped (a no-op removal),
/// matching the prior projection contract.
fn keywords_json_to_flags_and_tags(
    v: &serde_json::Value,
) -> Result<(Flags, Tags), crate::keyword::KeywordError> {
    let Some(obj) = v.as_object() else {
        return Ok((Flags(0), Tags::new()));
    };
    let mut bits: u32 = 0;
    let mut tags = Tags::new();
    for (k, val) in obj {
        if val.as_bool() != Some(true) {
            continue;
        }
        match crate::keyword::classify_jmap_keyword(k)? {
            crate::keyword::KeywordClass::System(bit) => bits |= bit,
            crate::keyword::KeywordClass::User(name) => {
                tags.insert(name);
            }
        }
    }
    if tags.len() > crate::keyword::MAX_KEYWORDS_PER_MESSAGE {
        return Err(crate::keyword::KeywordError::TooMany);
    }
    Ok((Flags(bits), tags))
}

/// Project a parsed [`mail_parser::Message`] onto an [`EmailEnvelope`].
///
/// The JMAP-side wire shape (`from` is a single string, `to`/`cc`/
/// `bcc`/`reply_to` are arrays of strings) folds RFC 5322 address-list
/// syntax through [`mail_parser::Address`] iteration and concatenates
/// each address's `address` field. Empty `from` becomes `""`; empty
/// list fields become `vec![]` — matching `EmailEnvelope`'s carry
/// shape. `subject` defaults to `""` when absent so the sidecar row
/// is always populated. `date` is unix-seconds (RFC 5322 origination
/// time), distinct from the item's `received_at` (delivery time, in
/// unix-millis) which the caller supplies separately.
///
/// `bcc` is captured if the source message carries the header (e.g.
/// for sender-side drafts written through `Email/set` or
/// `EmailSubmission/set`); externally-received messages typically
/// strip Bcc at delivery, so the field will commonly be empty for
/// inbound mail. The substrate stores whatever the writer hands us.
pub(crate) fn build_envelope(message: &mail_parser::Message<'_>) -> EmailEnvelope {
    let from = collect_envelope_addresses(message.from());
    let from_first = from.into_iter().next().unwrap_or_default();
    let to = collect_envelope_addresses(message.to());
    let cc = collect_envelope_addresses(message.cc());
    let bcc = collect_envelope_addresses(message.bcc());
    let reply_to = collect_envelope_addresses(message.reply_to());
    let subject = message.subject().map(|s| s.to_string()).unwrap_or_default();
    let date = message.date().map(|d| d.to_timestamp()).unwrap_or(0);
    let message_id = message.message_id().map(|s| s.to_string());
    EmailEnvelope {
        from: from_first,
        to,
        cc,
        bcc,
        reply_to,
        subject,
        date,
        message_id,
    }
}

/// Extract the parsed `In-Reply-To:` and `References:` Message-IDs
/// from a message, in the order JMAP threading needs to walk them: the
/// `In-Reply-To` first (the message this is a direct reply to), then
/// the `References` chain (older ancestors). Match-first-wins inside
/// `MailStore::create_email{,_with_memberships}` joins this email to
/// the closest existing ancestor; the additional `References` entries
/// catch the case where the immediate parent has been deleted but an
/// older ancestor is still present.
///
/// Returns the bare addr-spec strings — `mail_parser` strips the
/// surrounding `<…>` already, so `mail_envelopes.message_id` (also
/// stored bare) compares directly.
pub(crate) fn extract_in_reply_to(message: &mail_parser::Message<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let HeaderValue::Text(s) = message.in_reply_to() {
        out.push(s.to_string());
    } else if let HeaderValue::TextList(list) = message.in_reply_to() {
        out.extend(list.iter().map(|s| s.to_string()));
    }
    if let HeaderValue::Text(s) = message.references() {
        out.push(s.to_string());
    } else if let HeaderValue::TextList(list) = message.references() {
        out.extend(list.iter().map(|s| s.to_string()));
    }
    out
}

/// Flatten a [`mail_parser::Address`] (single, list, or grouped) into
/// the bare addr-spec strings the [`EmailEnvelope`] wire shape carries.
/// Addresses without an `address` field are dropped (display-name-only
/// entries are not addressable and would produce an empty string in
/// the array, which neither IMAP nor JMAP clients accept).
fn collect_envelope_addresses(addr: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    let Some(addr) = addr else {
        return Vec::new();
    };
    match addr {
        mail_parser::Address::List(list) => list
            .iter()
            .filter_map(|a| a.address.as_ref().map(|s| s.to_string()))
            .collect(),
        mail_parser::Address::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .filter_map(|a| a.address.as_ref().map(|s| s.to_string()))
            .collect(),
    }
}

/// Email/get — returns email metadata and optionally body values.
pub async fn get(
    _db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let fetch_text = args
        .get("fetchTextBodyValues")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let fetch_html = args
        .get("fetchHTMLBodyValues")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let requested_ids: Option<Vec<String>> = args
        .get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // RFC 8620 §5.1 `properties` filter. `None` = full projection;
    // `Some(empty)` = id-only; otherwise the named subset (plus id,
    // which is always returned per RFC). Parse strictly: a non-array
    // value surfaces `invalidArguments` rather than being silently
    // treated as full projection (legacy handler swallowed shape
    // errors via `serde_json::from_value(...).ok()`).
    let properties: Option<Vec<String>> = match args.get("properties") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(arr)) => {
            let mut v = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    serde_json::Value::String(s) => v.push(s.clone()),
                    other => {
                        return Err(JmapMethodError {
                            error_type: "invalidArguments".into(),
                            description: Some(format!(
                                "properties[] entry not a string: {}",
                                type_name_of_json(other)
                            )),
                        }
                        .into());
                    }
                }
            }
            Some(v)
        }
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "properties not an array: {}",
                    type_name_of_json(other)
                )),
            }
            .into());
        }
    };

    // Snapshot the mds-side `Email`-type state *before* any reads.
    // JMAP §5.2 says the returned `state` should reflect the data
    // version observed in `list`. Fetching at the earliest moment
    // guarantees that any write landing during record/blob reads
    // will be visible to the client via a subsequent
    // `Email/changes(sinceState = state)` — clients may over-fetch
    // but never under-fetch. Fetching after the reads would create
    // a TOCTOU where the state advances past a write whose effects
    // are not reflected in `list`.
    //
    // Phase 3 Task 3.7: state is now read from `MailStore::email_state`
    // (per-set `set_change_seq` head) rather than the legacy
    // `db::changelog::current_state(_, "Email")` row, since the
    // `Email`-type changelog stops being written once `Email/set`
    // and `Email/import` migrate to MDS. Reading the legacy row
    // here would silently freeze at a stale value, breaking the
    // `Email/changes(sinceState=state)` round-trip.
    let ms_state = mailstore.clone();
    let state = tokio::task::spawn_blocking(move || ms_state.email_state(account_id))
        .await?
        .map_err(|e| anyhow::anyhow!("serverFail: email_state read: {e}"))?;
    let state = state.to_string();

    let ms = mailstore.clone();
    let (records, not_found): (Vec<EmailRecord>, Vec<String>) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<EmailRecord>, Vec<String>)> {
            match requested_ids {
                Some(ids) => {
                    let mut out = Vec::with_capacity(ids.len());
                    let mut nf = Vec::new();
                    for id_str in ids {
                        let item_id = match id_str.parse::<Uuid>() {
                            Ok(u) => ItemId(u),
                            Err(_) => {
                                nf.push(id_str);
                                continue;
                            }
                        };
                        match ms.get_email(account_id, item_id) {
                            Ok(r) => out.push(r),
                            Err(e) => {
                                // Classify not-found via error-message
                                // prefix per the `MailStore::get_email`
                                // contract (`mailstore/mod.rs` doc on
                                // `get_email`: "errors start with `email
                                // not found` / `email envelope
                                // missing`"). This is a prose contract,
                                // not a typed variant — a future
                                // `MailStoreError` enum is queued as
                                // substrate-shaping work; until then
                                // any new MailStore impl MUST honour
                                // these prefixes or this branch
                                // silently misclassifies storage errors
                                // as JMAP notFound.
                                let msg = format!("{e}");
                                if msg.starts_with("email not found")
                                    || msg.starts_with("email envelope missing")
                                {
                                    nf.push(id_str);
                                } else {
                                    return Err(e);
                                }
                            }
                        }
                    }
                    Ok((out, nf))
                }
                None => {
                    // Legacy fallback — when `ids` is omitted, surface
                    // the most-recent 100 emails account-wide. This
                    // matches `db::email::query_ids(None, true, 0, 100)`
                    // shape from the pre-migration handler.
                    // `query_emails` ignores `ListOpts::sort_by` —
                    // the sort discriminator comes from the explicit
                    // `EmailSort` argument below. Only `limit` and
                    // `offset` are read from `ListOpts` here.
                    let opts = ListOpts {
                        limit: 100,
                        offset: 0,
                        ..Default::default()
                    };
                    let ids = ms.query_emails(
                        account_id,
                        EmailQueryFilter::default(),
                        EmailSort::ReceivedAtDesc,
                        opts,
                    )?;
                    let mut out = Vec::with_capacity(ids.len());
                    for id in ids {
                        // A concurrent destroy between the query and
                        // the per-id read is rare but possible; drop
                        // silently rather than failing the whole
                        // request.
                        match ms.get_email(account_id, id) {
                            Ok(r) => out.push(r),
                            Err(e) => {
                                let msg = format!("{e}");
                                if msg.starts_with("email not found")
                                    || msg.starts_with("email envelope missing")
                                {
                                    continue;
                                }
                                return Err(e);
                            }
                        }
                    }
                    Ok((out, Vec::new()))
                }
            }
        })
        .await??;

    let list: Vec<serde_json::Value> = if fetch_text || fetch_html {
        // Spawn the per-record blob reads up-front so they queue into
        // the tokio blocking pool together rather than one-await-at-
        // a-time. Note: `SqliteCasMds::get_blob` takes the per-set
        // connection mutex (cosmix-mds/src/sqlite_mds.rs `with_conn`),
        // so true I/O parallelism is bounded by the per-set lock —
        // what this fan-out actually buys is overlapped wakeup +
        // scheduler interleaving with other async work, not unbounded
        // disk concurrency. Worth revisiting if blob fetches become
        // the bottleneck.
        let mut blob_handles = Vec::with_capacity(records.len());
        for record in &records {
            let ms = mailstore.clone();
            let blob_hash = record.blob_hash;
            blob_handles.push(tokio::task::spawn_blocking(move || {
                ms.mds().get_blob(&blob_hash)
            }));
        }

        let mut result = Vec::with_capacity(records.len());
        for (record, handle) in records.iter().zip(blob_handles) {
            let mut val = record_to_jmap(record);
            // Distinguish the two error classes:
            //   - `JoinError` (panic in the blocking task) is a real
            //     bug — propagate so callers see a 500, not a
            //     silently-bodyless email row.
            //   - `Err(_)` from `get_blob` (missing blob, IO error)
            //     elides body parts only; metadata projection still
            //     ships.
            // The legacy `if let Ok(Ok(...)) = handle.await` swallowed
            // both, hiding panics behind "looks fine to the client".
            match handle.await {
                Ok(Ok(blob_data)) => {
                    add_body_parts(&mut val, &blob_data, fetch_text, fetch_html);
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        item_id = %record.id.0,
                        error = %e,
                        "blob fetch failed for Email/get; eliding body parts"
                    );
                }
                Err(join_err) => return Err(join_err.into()),
            }
            val = apply_email_properties(val, properties.as_deref());
            result.push(val);
        }
        result
    } else {
        records
            .iter()
            .map(|r| apply_email_properties(record_to_jmap(r), properties.as_deref()))
            .collect()
    };

    let resp = serde_json::json!({
        "accountId": acct,
        "state": state,
        "list": list,
        "notFound": not_found,
    });

    Ok(resp)
}

/// Parse a message and add textBody, htmlBody, bodyValues to the JSON response.
fn add_body_parts(val: &mut serde_json::Value, data: &[u8], fetch_text: bool, fetch_html: bool) {
    use mail_parser::{MessageParser, PartType};

    let parser = MessageParser::default();
    let Some(msg) = parser.parse(data) else {
        return;
    };

    let mut body_values = serde_json::Map::new();
    let mut text_body = Vec::new();
    let mut html_body = Vec::new();
    let mut part_idx = 0u32;

    for part in msg.parts.iter() {
        let (is_text, is_html, body_text) = match &part.body {
            PartType::Text(text) => (true, false, text.as_ref().to_string()),
            PartType::Html(html) => (false, true, html.as_ref().to_string()),
            _ => continue,
        };

        if body_text.is_empty() {
            continue;
        }

        let pid = part_idx.to_string();
        let mime_type = if is_text { "text/plain" } else { "text/html" };

        if is_text {
            text_body.push(serde_json::json!({
                "partId": pid,
                "type": mime_type,
            }));
            if fetch_text {
                body_values.insert(
                    pid.clone(),
                    serde_json::json!({
                        "value": body_text,
                        "isEncodingProblem": false,
                        "isTruncated": false,
                    }),
                );
            }
        }

        if is_html {
            html_body.push(serde_json::json!({
                "partId": pid,
                "type": mime_type,
            }));
            if fetch_html {
                body_values.insert(
                    pid.clone(),
                    serde_json::json!({
                        "value": body_text,
                        "isEncodingProblem": false,
                        "isTruncated": false,
                    }),
                );
            }
        }

        part_idx += 1;
    }

    val["textBody"] = serde_json::Value::Array(text_body);
    val["htmlBody"] = serde_json::Value::Array(html_body);
    val["bodyValues"] = serde_json::Value::Object(body_values);
}

/// Email/query (RFC 8621 §4.4) — translates the JMAP filter / sort
/// shape into [`EmailQueryFilter`] / [`EmailSort`] and dispatches to
/// [`MailStore::query_emails`].
///
/// **Capability surface.** The filter properties `inMailbox`,
/// `inMailboxOtherThan`, `before`, `after`, `minSize`, `maxSize`,
/// `hasKeyword`, `notKeyword` are honoured — these fall out of
/// `Mds::list_items` rows (`received_at`, `size_bytes`) plus
/// `item_memberships` (union flags / membership-set predicates), the
/// same surface `EmailQueryFilter` exposes (`mailstore/mod.rs:158`).
/// `text` (whole-message full-text) is also honoured, via the
/// `mail_search` FTS5 projection (`Mds::search_items`). Field-scoped
/// substring/full-text filters (`from`/`to`/`cc`/`subject`/`body`/
/// `header`), attachment predicates, thread-keyword filters, and
/// `FilterOperator` (`AND`/`OR`/`NOT`) all surface a method-level
/// `unsupportedFilter` per RFC 8621 §4.4.2 — better than the legacy
/// handler's silent shape-mismatch swallow (`serde_json::from_value(...)
/// .ok()` collapsed any malformed condition to `None`, hiding client bugs).
///
/// Sort accepts a single comparator on `receivedAt`, `subject` (RFC
/// 5256 base subject — prefix-stripped, lowercased), or `from` (bare
/// address, lowercased); multi-comparator requests, explicit
/// `collation`, or any other property surface `unsupportedSort` (RFC
/// 8620 §5.5).
/// Missing/empty/null `sort` defaults to `receivedAt` descending —
/// RFC 8620 §5.5 leaves the default unspecified; most-recent-first
/// matches the legacy handler and typical mailbox UI ordering. A
/// future capability advertisement can pin this for clients that
/// need cross-server stability.
///
/// State token is snapshotted *before* the storage read for the same
/// TOCTOU reason as `Email/get` (`email::get` doc above): a write
/// landing during the query is recoverable via a subsequent
/// `Email/changes(sinceState=state)` round-trip.
pub async fn query(
    _db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    // Snapshot mds-side state *before* the read. A write landing
    // during the query advances `state` past data not reflected in
    // `ids`; clients recover via `Email/changes(sinceState=state)`.
    // Fetching after the read would create the inverse TOCTOU where
    // `state` reflects a write whose ids did not appear.
    //
    // Phase 3 Task 3.7: state derives from `MailStore::email_state`
    // (per-set `set_change_seq` head); the legacy
    // `db::changelog::current_state(_, "Email")` row stops being
    // written once `Email/set` and `Email/import` migrate to MDS.
    let ms_state = mailstore.clone();
    let state = tokio::task::spawn_blocking(move || ms_state.email_state(account_id))
        .await?
        .map_err(|e| anyhow::anyhow!("serverFail: email_state read: {e}"))?;
    let state = state.to_string();

    let filter = parse_query_filter(args.get("filter"))?;
    let sort = parse_query_sort(args.get("sort"))?;

    let position = match args.get("position") {
        None | Some(serde_json::Value::Null) => 0i64,
        Some(serde_json::Value::Number(n)) => {
            let v = n.as_i64().ok_or_else(|| JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!("position not a representable integer: {n}")),
            })?;
            if v < 0 {
                // RFC 8620 §5.5 allows negative position (from-end). The
                // storage layer doesn't compute end-relative offsets in
                // Phase A; surfacing `invalidArguments` is the documented
                // refusal rather than the legacy handler's silent
                // coercion to 0 (which gave the wrong page for any
                // negative request without telling the client).
                return Err(JmapMethodError {
                    error_type: "invalidArguments".into(),
                    description: Some(
                        "negative position (from-end pagination) not supported in Phase A".into(),
                    ),
                }
                .into());
            }
            v
        }
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "position not a number: {}",
                    type_name_of_json(other)
                )),
            }
            .into());
        }
    };
    let limit = match args.get("limit") {
        None | Some(serde_json::Value::Null) => 50u64,
        Some(serde_json::Value::Number(n)) => n.as_u64().ok_or_else(|| JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some(format!("limit not a non-negative integer: {n}")),
        })?,
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!("limit not a number: {}", type_name_of_json(other))),
            }
            .into());
        }
    };

    // `MailStore::query_emails` only returns `Vec<ItemId>` — the JMAP
    // `total` field requires the count of all matching rows, not just
    // the page. Pull the full sorted survivor list and slice locally;
    // the storage layer already gathers all candidates in memory before
    // pagination (`mailstore/mod.rs:1559+`), so passing
    // `limit = u32::MAX` adds zero work beyond the slice that we'd
    // otherwise do at the storage edge. A future revision can extend
    // the trait to return `(Vec<ItemId>, u64)` and push the slice down.
    let opts = ListOpts {
        limit: u32::MAX,
        offset: 0,
        ..Default::default()
    };

    let ms = mailstore.clone();
    let all_ids =
        match tokio::task::spawn_blocking(move || ms.query_emails(account_id, filter, sort, opts))
            .await?
        {
            Ok(v) => v,
            Err(e) => {
                // Storage-layer "mailbox not found" is a JMAP-level
                // `invalidArguments`, not `serverFail` — the client passed
                // an `inMailbox` id that does not exist in the account's
                // mailbox set (RFC 8621 §4.4.2). `query_emails` returns
                // this case as a bare `anyhow!("mailbox not found: ...")`
                // (`mailstore/mod.rs:1530`); we re-raise it as a typed
                // method error so the dispatcher emits the correct error
                // triple. The classification by message-prefix mirrors the
                // existing convention in `email::get` (see the doc comment
                // around `email not found` / `email envelope missing` at
                // line ~145) — a future `MailStoreError` enum subsumes
                // both sites.
                let msg = format!("{e}");
                if msg.starts_with("mailbox not found") {
                    return Err(JmapMethodError {
                        error_type: "invalidArguments".into(),
                        description: Some(msg),
                    }
                    .into());
                }
                return Err(e);
            }
        };

    // `limit` is parsed as `u64` to keep the wire surface honest, but
    // pagination indices into the in-memory `all_ids` Vec are `usize`.
    // On a 32-bit target a client-supplied limit > u32::MAX would
    // silently truncate the cast and return an empty page despite the
    // client asking for "everything"; clamping to `usize::MAX` here
    // keeps the slice math exact on every supported target. Cosmix's
    // mesh nodes are 64-bit, but the clamp is free insurance against
    // future cross-compiles or sandbox runs.
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let total = all_ids.len();
    let start = (position as usize).min(all_ids.len());
    let end = start.saturating_add(limit_usize).min(all_ids.len());
    let page = &all_ids[start..end];

    let resp = QueryResponse {
        account_id: acct,
        query_state: state,
        can_calculate_changes: false,
        // RFC 8620 §5.5: the response `position` is the zero-based
        // index of the *first returned id* in the full result list, not
        // an echo of the request. When `position` exceeds `total` the
        // page is empty and `start == total`; emitting the request
        // value would tell a paginating client "you are at offset N"
        // when it is actually at the end of the list.
        position: start as u64,
        ids: page.iter().map(|id| id.0.to_string()).collect(),
        total: total as u64,
    };
    Ok(serde_json::to_value(resp)?)
}

/// `Email/queryChanges` (RFC 8621 §4.5 / RFC 8620 §5.6).
///
/// Phase A returns `cannotCalculateChanges` whenever `sinceQueryState`
/// is supplied. The base `Email/query` advertises `canCalculateChanges:
/// false`, so a compliant client SHOULD NOT call this method — but per
/// RFC 8620 §5.6 the server MUST still respond with the typed error
/// rather than `serverFail` when it does.
///
/// Argument validation runs *before* the unconditional refusal so the
/// error-type precedence in RFC 8620 §3.6.1 is preserved: missing
/// required arguments raise `invalidArguments`, which is what a client
/// has to react to first regardless of the server's delta-calculation
/// capability. When Phase B grows real delta calculation this contract
/// stays intact — only the final `cannotCalculateChanges` arm narrows.
///
/// Auth/account binding follows the dispatcher convention used by every
/// other handler (`email::get`, `email::query`, `email::changes`,
/// etc.): the authenticated `account_id` is bound at the dispatch
/// boundary and the request `accountId` field is not cross-checked
/// here. Since the handler returns the typed error before any storage
/// touch, `_db` / `_mailstore` / `_account_id` are intentionally
/// unused.
///
/// Computing a real delta requires either a stored prior result set per
/// `(filter, sort)` tuple, or a server-side index that can answer
/// "what (filter, sort) results changed since `sinceQueryState`?".
/// Neither exists today; recording the prior set is also incompatible
/// with the changelog's per-id granularity. Until that work lands the
/// honest answer is the typed refusal — clients fall back to a fresh
/// `Email/query`, which is the documented contract.
pub async fn query_changes(
    _db: &Db,
    _mailstore: &Arc<SqliteMailStore>,
    _account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    // RFC 8620 §5.6 lists `sinceQueryState` as a required argument.
    // Surface its absence as `invalidArguments` first — the precedence
    // matters for clients that retry on `cannotCalculateChanges` but
    // surface `invalidArguments` to the user, since a missing required
    // field is a programming error, not a stale-state condition.
    match args.get("sinceQueryState") {
        Some(serde_json::Value::String(_)) => {}
        None => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some("sinceQueryState is required".into()),
            }
            .into());
        }
        Some(serde_json::Value::Null) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some("sinceQueryState must be a string, got null".into()),
            }
            .into());
        }
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "sinceQueryState must be a string, got {}",
                    type_name_of_json(other)
                )),
            }
            .into());
        }
    }

    Err(JmapMethodError {
        error_type: "cannotCalculateChanges".into(),
        description: Some(
            "Email/queryChanges is not supported; Email/query advertises canCalculateChanges=false. \
             Re-issue Email/query to refresh the result set."
                .into(),
        ),
    }
    .into())
}

/// Translate the JMAP `Email/query` `filter` argument into an
/// [`EmailQueryFilter`].
///
/// Returns `Ok(EmailQueryFilter::default())` for missing/null filter.
/// `text` (whole-message full-text) is supported; the field-scoped
/// filters (envelope substring, per-column full-text, thread-keyword,
/// attachment) and `FilterOperator` shapes raise `unsupportedFilter`
/// per RFC 8621 §4.4.2. Malformed values for supported properties
/// raise `invalidArguments`.
fn parse_query_filter(filter: Option<&serde_json::Value>) -> Result<EmailQueryFilter> {
    let Some(filter) = filter else {
        return Ok(EmailQueryFilter::default());
    };
    if filter.is_null() {
        return Ok(EmailQueryFilter::default());
    }
    let obj = filter.as_object().ok_or_else(|| JmapMethodError {
        error_type: "invalidArguments".into(),
        description: Some(format!(
            "filter must be an object or null, got {}",
            type_name_of_json(filter)
        )),
    })?;

    // FilterOperator (RFC 8620 §5.5): {operator, conditions} — the
    // operator/conditions keys identify it without needing a full
    // schema match (a client passing a stray `operator: "AND"` alongside
    // condition fields gets the same refusal, which is the safe answer).
    if obj.contains_key("operator") || obj.contains_key("conditions") {
        return Err(JmapMethodError {
            error_type: "unsupportedFilter".into(),
            description: Some("FilterOperator (AND/OR/NOT) not supported in Phase A".into()),
        }
        .into());
    }

    // Reject unknown / unsupported condition keys up front so the client
    // sees a single decisive error instead of a partial filter that
    // silently dropped half the predicates. `text` (whole-message FTS) is
    // backed by the `mail_search` projection; the remaining cosmix-mds
    // surface (`mail_envelopes` for per-field envelope substring, the
    // `attachments` projection, thread-keyword aggregates) is not yet
    // wired — until it lands, refusing those is better than guessing.
    for k in obj.keys() {
        match k.as_str() {
            "inMailbox" | "inMailboxOtherThan" | "before" | "after" | "minSize" | "maxSize"
            | "hasKeyword" | "notKeyword" | "text" => {}
            "from"
            | "to"
            | "cc"
            | "bcc"
            | "subject"
            | "body"
            | "header"
            | "hasAttachment"
            | "attachmentName"
            | "attachmentType"
            | "allInThreadHaveKeyword"
            | "someInThreadHaveKeyword"
            | "noneInThreadHaveKeyword" => {
                return Err(JmapMethodError {
                    error_type: "unsupportedFilter".into(),
                    description: Some(format!("filter property `{k}` not supported in Phase A")),
                }
                .into());
            }
            other => {
                return Err(JmapMethodError {
                    error_type: "unsupportedFilter".into(),
                    description: Some(format!("unknown filter property `{other}`")),
                }
                .into());
            }
        }
    }

    let mut out = EmailQueryFilter::default();
    if let Some(v) = obj.get("inMailbox") {
        out.in_mailbox = Some(ContainerId(parse_id(v, "inMailbox")?));
    }
    if let Some(v) = obj.get("inMailboxOtherThan") {
        let arr = v.as_array().ok_or_else(|| JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some(format!(
                "inMailboxOtherThan must be an array, got {}",
                type_name_of_json(v)
            )),
        })?;
        // An empty array is a no-op filter (the JMAP semantics — "the
        // email has at least one mailbox not in []" is satisfied by any
        // item with at least one membership). Drop it to `None` here so
        // the storage layer skips the per-survivor `item_memberships`
        // lookup that `Some(vec![])` would otherwise force.
        if !arr.is_empty() {
            let mut ids = Vec::with_capacity(arr.len());
            for item in arr {
                ids.push(ContainerId(parse_id(item, "inMailboxOtherThan element")?));
            }
            out.in_mailbox_other_than = Some(ids);
        }
    }
    if let Some(v) = obj.get("before") {
        out.before = Some(parse_utcdate_ms(v, "before")?);
    }
    if let Some(v) = obj.get("after") {
        out.after = Some(parse_utcdate_ms(v, "after")?);
    }
    if let Some(v) = obj.get("minSize") {
        out.min_size = Some(parse_unsigned_int(v, "minSize")?);
    }
    if let Some(v) = obj.get("maxSize") {
        out.max_size = Some(parse_unsigned_int(v, "maxSize")?);
    }
    if let Some(v) = obj.get("hasKeyword") {
        match parse_keyword_filter(v, "hasKeyword")? {
            KeywordFilterTarget::System(f) => out.has_keyword = Some(f),
            KeywordFilterTarget::User(t) => out.has_tag = Some(t),
        }
    }
    if let Some(v) = obj.get("notKeyword") {
        match parse_keyword_filter(v, "notKeyword")? {
            KeywordFilterTarget::System(f) => out.not_keyword = Some(f),
            KeywordFilterTarget::User(t) => out.not_tag = Some(t),
        }
    }
    // `text`: whole-message full-text search over the `mail_search` FTS5
    // projection (folded headers + subject + plain-text body + normalized
    // addrs). The storage layer (`Mds::search_items`) builds a safe FTS5
    // MATCH from this raw needle. Field-scoped variants (`from`/`subject`/
    // `body`/…) remain unsupported — they'd need per-column FTS scoping.
    if let Some(v) = obj.get("text") {
        let s = v.as_str().ok_or_else(|| JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some(format!(
                "text must be a string, got {}",
                type_name_of_json(v)
            )),
        })?;
        out.text = Some(s.to_string());
    }
    Ok(out)
}

/// Translate JMAP `sort` (array of `{property, isAscending, collation}`
/// comparators) into [`EmailSort`].
///
/// A single comparator on `receivedAt`, `subject`, or `from`.
/// Multi-comparator requests, comparators on other properties, and
/// explicit `collation` raise `unsupportedSort`.
fn parse_query_sort(sort: Option<&serde_json::Value>) -> Result<EmailSort> {
    let Some(sort) = sort else {
        return Ok(EmailSort::ReceivedAtDesc);
    };
    if sort.is_null() {
        return Ok(EmailSort::ReceivedAtDesc);
    }
    let arr = sort.as_array().ok_or_else(|| JmapMethodError {
        error_type: "invalidArguments".into(),
        description: Some(format!(
            "sort must be an array of comparators, got {}",
            type_name_of_json(sort)
        )),
    })?;
    if arr.is_empty() {
        return Ok(EmailSort::ReceivedAtDesc);
    }
    if arr.len() > 1 {
        return Err(JmapMethodError {
            error_type: "unsupportedSort".into(),
            description: Some("multi-comparator sort not supported in Phase A".into()),
        }
        .into());
    }
    let cmp = arr[0].as_object().ok_or_else(|| JmapMethodError {
        error_type: "invalidArguments".into(),
        description: Some(format!(
            "sort comparator must be an object, got {}",
            type_name_of_json(&arr[0])
        )),
    })?;
    let property = cmp
        .get("property")
        .and_then(|v| v.as_str())
        .ok_or_else(|| JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some("sort comparator missing string `property`".into()),
        })?;
    if !matches!(property, "receivedAt" | "subject" | "from") {
        return Err(JmapMethodError {
            error_type: "unsupportedSort".into(),
            description: Some(format!("sort property `{property}` not supported")),
        }
        .into());
    }
    if cmp.contains_key("collation") {
        // RFC 8620 §5.5: server MAY refuse sorts with explicit
        // collation. Phase A doesn't apply collations to receivedAt
        // (it's a numeric ordering), so any non-default value is a
        // mismatch worth surfacing rather than silently ignoring.
        return Err(JmapMethodError {
            error_type: "unsupportedSort".into(),
            description: Some("explicit collation not supported in Phase A".into()),
        }
        .into());
    }
    let is_ascending = match cmp.get("isAscending") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "isAscending must be a boolean, got {}",
                    type_name_of_json(other)
                )),
            }
            .into());
        }
    };
    Ok(match (property, is_ascending) {
        ("receivedAt", true) => EmailSort::ReceivedAtAsc,
        ("receivedAt", false) => EmailSort::ReceivedAtDesc,
        ("subject", true) => EmailSort::SubjectAsc,
        ("subject", false) => EmailSort::SubjectDesc,
        ("from", true) => EmailSort::FromAsc,
        ("from", false) => EmailSort::FromDesc,
        // The property allowlist above guarantees no other value reaches here.
        _ => unreachable!("sort property validated above"),
    })
}

fn parse_id(v: &serde_json::Value, field: &str) -> Result<Uuid> {
    let s = v.as_str().ok_or_else(|| JmapMethodError {
        error_type: "invalidArguments".into(),
        description: Some(format!(
            "{field} must be a string id, got {}",
            type_name_of_json(v)
        )),
    })?;
    s.parse::<Uuid>().map_err(|_| {
        JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some(format!("{field}: not a valid id: {s:?}")),
        }
        .into()
    })
}

/// Parse a JMAP `UTCDate` (RFC 8620 §1.4 — RFC 3339 with mandatory
/// timezone) into milliseconds since the Unix epoch — the unit
/// `Mds::list_items` exposes for `received_at` and the unit
/// `EmailQueryFilter::before`/`after` are matched against.
fn parse_utcdate_ms(v: &serde_json::Value, field: &str) -> Result<i64> {
    let s = v.as_str().ok_or_else(|| JmapMethodError {
        error_type: "invalidArguments".into(),
        description: Some(format!(
            "{field} must be a UTCDate string, got {}",
            type_name_of_json(v)
        )),
    })?;
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp_millis())
        .map_err(|e| {
            JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!("{field}: invalid UTCDate {s:?}: {e}")),
            }
            .into()
        })
}

fn parse_unsigned_int(v: &serde_json::Value, field: &str) -> Result<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| {
            JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!("{field}: not a non-negative integer: {n}")),
            }
            .into()
        }),
        other => Err(JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some(format!(
                "{field} must be a number, got {}",
                type_name_of_json(other)
            )),
        }
        .into()),
    }
}

/// Where a JMAP `hasKeyword` / `notKeyword` value routes in the storage
/// filter: a system keyword drives the [`Flags`] predicate, a user keyword the
/// `Tags` predicate.
#[derive(Debug, PartialEq, Eq)]
enum KeywordFilterTarget {
    System(Flags),
    User(String),
}

/// Translate a JMAP keyword-filter name into the storage predicate it drives.
/// The four allocated system `$`-names (`$seen / $flagged / $answered /
/// $draft`) become a [`Flags`] mask; any other name (non-`$`, or a registry
/// `$`-keyword) is normalised (NFC + lowercase, see [`crate::keyword`]) into a
/// `Tags` predicate string. Only a malformed keyword (atom-special/whitespace)
/// or a mixed-case variant of a system name surfaces `unsupportedFilter` —
/// refusing is preferable to silently returning an empty result (`hasKeyword`)
/// or every email (`notKeyword`), both of which a client would read as
/// authoritative.
fn parse_keyword_filter(v: &serde_json::Value, field: &str) -> Result<KeywordFilterTarget> {
    let s = v.as_str().ok_or_else(|| JmapMethodError {
        error_type: "invalidArguments".into(),
        description: Some(format!(
            "{field} must be a keyword name string, got {}",
            type_name_of_json(v)
        )),
    })?;
    if let Some(bit) = crate::keyword::jmap_system_keyword_bit(s) {
        return Ok(KeywordFilterTarget::System(Flags(bit)));
    }
    match crate::keyword::normalise_user_keyword(s) {
        Ok(name) => Ok(KeywordFilterTarget::User(name)),
        Err(e) => Err(JmapMethodError {
            error_type: "unsupportedFilter".into(),
            description: Some(format!("{field}: keyword {s:?} not supported: {e}")),
        }
        .into()),
    }
}

/// Email/set (RFC 8621 §4.6) — Migrated to the cosmix-mds-backed
/// MailStore as part of Tasks 3.3 / 3.4 / 3.5 of the JMAP→MDS
/// migration plan.
///
/// **Reduced-atomic landing (option 1b).** Multi-mailbox replacement
/// in the `update` branch is gated behind
/// [`MailStore::set_mailboxes`], which is not yet implemented. Until
/// the follow-up Task 18c lands that primitive, an `update` patch
/// whose `mailboxIds` would result in more than one mailbox (or that
/// changes membership for an email currently in more than one
/// mailbox) surfaces as `invalidArguments` with the description
/// `multi-mailbox replacement not yet supported`. Single-mailbox
/// replacement (the dominant inbox↔junk / inbox↔archive workflow)
/// goes through [`MailStore::move_email`].
///
/// **State tokens** are derived from the per-set `set_change_seq`
/// (via [`MailStore::email_state`]); the legacy SQLite `changelog`
/// table is no longer consulted for the `Email` type. Clients
/// observe `oldState` ≤ `newState`; absent any changes,
/// `oldState == newState`. Both are stringified `u64`s per RFC 8620
/// §1.3 and round-trip through [`changes`]'s `sinceState`.
///
/// **Junk-boundary retrain.** When a single-mailbox move crosses the
/// `\Junk` role boundary (entering or leaving the Junk mailbox), the
/// bayesian classifier is retrained inline before the move commits.
/// The non-retrain [`MailStore::move_email`] path is used (the
/// retrain is performed at the JMAP layer rather than via the
/// `move_message` outbox row, since no worker drains
/// `mail_retrain_outbox` today).
pub async fn set(
    db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
    classifier: &Arc<DefaultClassifier>,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let old_state_seq = {
        let ms = mailstore.clone();
        tokio::task::spawn_blocking(move || ms.email_state(account_id)).await??
    };

    let mut created_map = std::collections::HashMap::new();
    let mut not_created = std::collections::HashMap::new();
    let mut updated_map = std::collections::HashMap::new();
    let mut destroyed_list = Vec::new();
    let mut not_updated = std::collections::HashMap::new();
    let mut not_destroyed = std::collections::HashMap::new();

    // Snapshot the account's mailboxes once. The set is used for two
    // purposes in the update branch: (1) finding the Junk mailbox so
    // a junk-boundary move can pre-compute its retrain label, and
    // (2) prevalidating that a requested mailboxIds destination
    // actually exists so a keyword change is not committed against
    // an email whose mailbox-replacement step is destined to fail.
    // Concurrent mailbox deletion between this snapshot and the move
    // remains a residual race; closing it would require a
    // MailStore::patch_email substrate primitive (out of option 1b
    // scope).
    let account_mailboxes = {
        let ms = mailstore.clone();
        tokio::task::spawn_blocking(move || ms.list_mailboxes(account_id)).await??
    };
    let junk_mailbox_id: Option<ContainerId> = account_mailboxes
        .iter()
        .find(|m| m.role == Some(MailboxRole::Junk))
        .map(|m| m.id);
    let known_mailbox_ids: std::collections::HashSet<ContainerId> =
        account_mailboxes.iter().map(|m| m.id).collect();

    // ---- create ----
    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, entry) in create {
            // `Email/set create` is non-idempotent: two `create`
            // entries for the same blob produce two distinct emails
            // (matches both RFC 8621 default semantics and the
            // pre-migration legacy behaviour, which allocated a
            // fresh `Uuid::new_v4()` per `db::email::create` call).
            // The blob-hash idempotency contract is import-only.
            match create_from_blob(db, mailstore, account_id, entry, false).await {
                Ok(item_id) => {
                    created_map.insert(
                        client_id.clone(),
                        serde_json::json!({ "id": item_id.0.to_string() }),
                    );
                }
                Err(e) => {
                    not_created.insert(client_id.clone(), classify_create_error(&e));
                }
            }
        }
    }

    // ---- update ----
    if let Some(update) = args.get("update").and_then(|v| v.as_object()) {
        for (id_str, patch) in update {
            let Ok(item_uuid) = id_str.parse::<Uuid>() else {
                not_updated.insert(
                    id_str.clone(),
                    SetError {
                        error_type: "invalidArguments".into(),
                        description: Some("Invalid id".into()),
                    },
                );
                continue;
            };
            let item_id = ItemId(item_uuid);

            let mut changed = false;
            let mut patch_failed: Option<SetError> = None;
            // The complete new keyword set to write: system flags AND user
            // keywords (`Tags`) bundled so the two can never drift apart and
            // a write can never wipe one of them. `set_keywords` replaces
            // both columns, so both halves must be the full intended value.
            let mut new_keywords: Option<(Flags, Tags)> = None;
            let mut mailbox_replacement: Option<Vec<ContainerId>> = None;
            // True when `new_keywords` came from a top-level
            // `keywords` replacement (RFC 8620 §5.3 whole-object replace)
            // rather than from a pointer-path delta. Replacement bitmaps
            // only carry JMAP-visible bits ($seen/$flagged/$answered/$draft);
            // substrate bits with no JMAP counterpart (today: `\Deleted`)
            // must be preserved across the write, else a JMAP client
            // round-tripping the keywords object would silently clear
            // the IMAP `\Deleted` marker. Phase 3 merges current
            // substrate state when this is true.
            let mut keywords_from_replace = false;

            // ---- Phase 0: RFC 8620 §5.3 PatchObject pointer-paths.
            // Keys of the form `keywords/<name>` and `mailboxIds/<uuid>`
            // describe partial updates: value `true` sets the entry,
            // value `null` clears it. RFC 8620 §5.3 forbids mixing a
            // pointer-path with the equivalent top-level whole-object
            // replace (`keywords` / `mailboxIds`) in the same patch
            // (`invalidPatch`). Pointer-path deltas are folded into
            // `new_keywords` / `mailbox_replacement` so the existing
            // Phase 2/3 mutation logic commits them through the same
            // code paths as the top-level form.
            let mut keyword_deltas: Vec<(String, bool)> = Vec::new();
            let mut mailbox_deltas: Vec<(ContainerId, bool)> = Vec::new();
            let patch_obj = patch.as_object();
            let has_top_keywords = patch_obj.is_some_and(|m| m.contains_key("keywords"));
            let has_top_mailbox = patch_obj.is_some_and(|m| m.contains_key("mailboxIds"));
            // RFC 8620 §5.3 / RFC 6901: pointer paths address a single
            // member of an object property. Email/set only exposes one
            // level of nesting under `keywords` / `mailboxIds`, so any
            // `/` past the first segment is `invalidPatch` (would
            // otherwise be silently accepted as a keyword/UUID name
            // containing `/`, which round-trips to nothing).
            for (k, v) in patch_obj.into_iter().flatten() {
                if let Some(name) = k.strip_prefix("keywords/") {
                    if has_top_keywords {
                        patch_failed = Some(SetError {
                            error_type: "invalidPatch".into(),
                            description: Some(
                                "cannot mix top-level `keywords` and `keywords/*` pointer-paths"
                                    .into(),
                            ),
                        });
                        break;
                    }
                    if name.is_empty() || name.contains('/') {
                        patch_failed = Some(SetError {
                            error_type: "invalidPatch".into(),
                            description: Some(format!(
                                "keywords/{name}: pointer-path must address exactly one keyword segment"
                            )),
                        });
                        break;
                    }
                    // RFC 6901 §3: decode the pointer segment — `~1` → `/`,
                    // then `~0` → `~`. A literal `/` is a multi-segment path
                    // (rejected above), but an *escaped* `~1` is a valid
                    // keyword character, so `keywords/a~1b` must address the
                    // keyword `a/b` — the same name GET emits and a full
                    // `keywords` replace would carry literally.
                    let name = name.replace("~1", "/").replace("~0", "~");
                    match v {
                        serde_json::Value::Bool(true) => {
                            keyword_deltas.push((name.clone(), true));
                        }
                        serde_json::Value::Null => {
                            keyword_deltas.push((name.clone(), false));
                        }
                        _ => {
                            patch_failed = Some(SetError {
                                error_type: "invalidPatch".into(),
                                description: Some(format!(
                                    "keywords/{name}: pointer-path value must be true or null"
                                )),
                            });
                            break;
                        }
                    }
                } else if let Some(idstr) = k.strip_prefix("mailboxIds/") {
                    if has_top_mailbox {
                        patch_failed = Some(SetError {
                            error_type: "invalidPatch".into(),
                            description: Some(
                                "cannot mix top-level `mailboxIds` and `mailboxIds/*` pointer-paths"
                                    .into(),
                            ),
                        });
                        break;
                    }
                    if idstr.is_empty() || idstr.contains('/') {
                        patch_failed = Some(SetError {
                            error_type: "invalidPatch".into(),
                            description: Some(format!(
                                "mailboxIds/{idstr}: pointer-path must address exactly one mailbox id segment"
                            )),
                        });
                        break;
                    }
                    let Ok(uuid) = idstr.parse::<Uuid>() else {
                        patch_failed = Some(SetError {
                            error_type: "invalidPatch".into(),
                            description: Some(format!(
                                "mailboxIds/{idstr}: pointer-path key must be a UUID"
                            )),
                        });
                        break;
                    };
                    match v {
                        serde_json::Value::Bool(true) => {
                            mailbox_deltas.push((ContainerId(uuid), true));
                        }
                        serde_json::Value::Null => {
                            mailbox_deltas.push((ContainerId(uuid), false));
                        }
                        _ => {
                            patch_failed = Some(SetError {
                                error_type: "invalidPatch".into(),
                                description: Some(format!(
                                    "mailboxIds/{idstr}: pointer-path value must be true or null"
                                )),
                            });
                            break;
                        }
                    }
                }
            }

            // Fold pointer-path deltas into new_keywords /
            // mailbox_replacement by reading current state once. Order
            // matters: the existing `new_keywords` / `mailbox_replacement`
            // parsers below assign from top-level keys only, and the
            // mixed-form check above guarantees those keys are absent
            // when we get here, so this single read is unambiguous.
            if patch_failed.is_none() && (!keyword_deltas.is_empty() || !mailbox_deltas.is_empty())
            {
                let ms = mailstore.clone();
                let cur =
                    tokio::task::spawn_blocking(move || ms.get_email(account_id, item_id)).await?;
                match cur {
                    Err(e) => {
                        patch_failed = Some(classify_email_not_found(&e));
                    }
                    Ok(current) => {
                        if !keyword_deltas.is_empty() {
                            // Multi-mailbox keyword updates are
                            // rejected: `MailStore::set_keywords` fans
                            // out to every membership, and
                            // `get_email`'s keyword projection is a
                            // union across memberships. Applying a
                            // delta starting from the union would leak
                            // per-mailbox IMAP-only bits (e.g.
                            // `\Deleted` set only on INBOX) to every
                            // other membership. Until per-membership
                            // JMAP keyword writes land as a substrate
                            // primitive, refuse the patch rather than
                            // silently corrupting flag state.
                            if current.mailbox_ids.len() > 1 {
                                patch_failed = Some(SetError {
                                    error_type: "invalidPatch".into(),
                                    description: Some(
                                        "keyword updates on multi-mailbox emails are not supported until per-membership JMAP keyword writes preserve IMAP-only flags"
                                            .into(),
                                    ),
                                });
                            } else {
                                // Apply each `keywords/<name>` delta starting
                                // from current state: a system `$`-name toggles
                                // its `Flags` bit; a user keyword is normalised
                                // (NFC + lowercase) and added/removed from the
                                // `Tags` set. An unknown `$`-reserved name (or a
                                // malformed user keyword) rejects the patch
                                // rather than silently no-op'ing it.
                                let mut bits = current.keywords.0;
                                let mut tags = current.tags.clone();
                                let mut delta_err: Option<crate::keyword::KeywordError> = None;
                                for (name, set) in &keyword_deltas {
                                    match crate::keyword::classify_jmap_keyword(name) {
                                        Ok(crate::keyword::KeywordClass::System(bit)) => {
                                            if *set {
                                                bits |= bit;
                                            } else {
                                                bits &= !bit;
                                            }
                                        }
                                        Ok(crate::keyword::KeywordClass::User(n)) => {
                                            if *set {
                                                tags.insert(n);
                                            } else {
                                                tags.0.remove(&n);
                                            }
                                        }
                                        Err(e) => {
                                            delta_err = Some(e);
                                            break;
                                        }
                                    }
                                }
                                if let Some(e) = delta_err {
                                    patch_failed = Some(SetError {
                                        error_type: "invalidProperties".into(),
                                        description: Some(format!("invalid keyword in patch: {e}")),
                                    });
                                } else if tags.len() > crate::keyword::MAX_KEYWORDS_PER_MESSAGE {
                                    patch_failed = Some(SetError {
                                        error_type: "invalidProperties".into(),
                                        description: Some(format!(
                                            "{}",
                                            crate::keyword::KeywordError::TooMany
                                        )),
                                    });
                                } else {
                                    new_keywords = Some((Flags(bits), tags));
                                }
                            }
                        }
                        if patch_failed.is_none() && !mailbox_deltas.is_empty() {
                            let mut set: std::collections::HashSet<ContainerId> =
                                current.mailbox_ids.iter().copied().collect();
                            for (id, add) in &mailbox_deltas {
                                if *add {
                                    set.insert(*id);
                                } else {
                                    set.remove(id);
                                }
                            }
                            mailbox_replacement = Some(set.into_iter().collect());
                        }
                    }
                }
            }

            // ---- Phase 1: parse + structurally validate all
            // requested properties before any mutation. This bounds
            // the residual non-atomicity of option 1b: a structurally
            // invalid patch never observes a partial commit. The only
            // remaining race window is between the keywords mutation
            // and the move mutation in Phase 3 (concurrent destroy /
            // mailbox-delete) — closing that requires a substrate
            // primitive (`MailStore::patch_email`) which is out of
            // scope for option 1b.
            //
            // Parse `keywords` strictly: must be an object whose every
            // value is `true` (RFC 8621 §4.6 — Set-style id-keyed
            // map). A non-object payload or any non-`true` value
            // rejects the whole patch rather than silently degrading
            // to an empty keyword set and clearing the email's keywords.
            // The four system `$`-names route to flag bits; non-`$`
            // names are normalised into `Tags`; an unknown `$`-reserved
            // or malformed name is rejected (`invalidProperties`) —
            // see `keywords_json_to_flags_and_tags`.
            if patch_failed.is_none() {
                new_keywords = match patch.get("keywords") {
                    None => new_keywords,
                    Some(v) => match v.as_object() {
                        None => {
                            patch_failed = Some(SetError {
                                error_type: "invalidProperties".into(),
                                description: Some("keywords must be an object".into()),
                            });
                            None
                        }
                        Some(obj) => {
                            let mut bad: Option<String> = None;
                            for (k, val) in obj {
                                if val.as_bool() != Some(true) {
                                    bad = Some(k.clone());
                                    break;
                                }
                            }
                            if let Some(k) = bad {
                                patch_failed = Some(SetError {
                                    error_type: "invalidProperties".into(),
                                    description: Some(format!(
                                        "Invalid keywords entry: {k} must map to true"
                                    )),
                                });
                                None
                            } else {
                                // Whole-object replace: split into (Flags, Tags),
                                // normalising user keywords and rejecting an
                                // unknown `$`-reserved or malformed name.
                                match keywords_json_to_flags_and_tags(v) {
                                    Ok((f, t)) => {
                                        keywords_from_replace = true;
                                        Some((f, t))
                                    }
                                    Err(e) => {
                                        patch_failed = Some(SetError {
                                            error_type: "invalidProperties".into(),
                                            description: Some(format!("{e}")),
                                        });
                                        None
                                    }
                                }
                            }
                        }
                    },
                };
            }

            // Parse `mailboxIds` strictly: must be an object, every
            // key must parse as a UUID, every value must be `true`
            // (RFC 8621 §4.6 — Set-style id-keyed map). Non-object
            // payload and malformed entries are reported as
            // `invalidProperties` rather than silently dropped (which
            // would let a mixed valid+invalid payload commit a
            // partial replacement).
            if patch_failed.is_none() {
                mailbox_replacement = match patch.get("mailboxIds") {
                    None => mailbox_replacement,
                    Some(v) => match v.as_object() {
                        None => {
                            patch_failed = Some(SetError {
                                error_type: "invalidProperties".into(),
                                description: Some("mailboxIds must be an object".into()),
                            });
                            None
                        }
                        Some(obj) => {
                            let mut acc: Vec<ContainerId> = Vec::with_capacity(obj.len());
                            let mut bad_key: Option<String> = None;
                            for (k, val) in obj {
                                if val.as_bool() != Some(true) {
                                    bad_key = Some(k.clone());
                                    break;
                                }
                                match k.parse::<Uuid>() {
                                    Ok(u) => acc.push(ContainerId(u)),
                                    Err(_) => {
                                        bad_key = Some(k.clone());
                                        break;
                                    }
                                }
                            }
                            if let Some(k) = bad_key {
                                patch_failed = Some(SetError {
                                    error_type: "invalidProperties".into(),
                                    description: Some(format!("Invalid mailboxIds entry: {k}")),
                                });
                                None
                            } else {
                                Some(acc)
                            }
                        }
                    },
                };
            }

            // Prevalidate every mailboxIds destination (whether sourced
            // from a top-level whole-object replace or a pointer-path
            // delta) against the account's mailbox snapshot so a
            // phantom destination is rejected *before* the keywords
            // mutation runs in Phase 3 (closes the "keywords commits,
            // move fails on bad dest" leak Codex flagged). Concurrent
            // mailbox deletion between this snapshot and the move
            // remains a residual race window.
            if patch_failed.is_none()
                && let Some(acc) = mailbox_replacement.as_ref()
            {
                let missing: Option<ContainerId> = acc
                    .iter()
                    .find(|id| !known_mailbox_ids.contains(*id))
                    .copied();
                if let Some(id) = missing {
                    patch_failed = Some(SetError {
                        error_type: "invalidArguments".into(),
                        description: Some(format!("mailbox not found: {}", id.0)),
                    });
                    mailbox_replacement = None;
                }
            }

            // ---- Phase 2: state-dependent validation. Only fires if
            // a mailboxIds replacement was requested. Reads the
            // current record once, validates the single→single
            // transition feasibility, and pre-computes the
            // junk-boundary label for Phase 3.
            //
            // `planned_move` carries the destination, the email's
            // blob hash (for the retrain pull-from-CAS), and the
            // retrain label (or `None` if the move doesn't cross the
            // Junk boundary). `None` overall means no move (either
            // mailboxIds wasn't patched, or src == dest).
            let mut planned_move: Option<(ContainerId, cosmix_mds::BlobHash, Option<Label>)> = None;
            if patch_failed.is_none()
                && let Some(new_mbox) = mailbox_replacement.as_ref()
            {
                let ms = mailstore.clone();
                let current =
                    tokio::task::spawn_blocking(move || ms.get_email(account_id, item_id)).await?;
                match current {
                    Err(e) => {
                        patch_failed = Some(classify_email_not_found(&e));
                    }
                    Ok(current_record) => {
                        if current_record.mailbox_ids.is_empty() {
                            // Defensive: MailStore should not return a
                            // record with zero memberships, but if it
                            // does (concurrent destroy race, schema
                            // bug) treat it as notFound rather than
                            // panicking on `[0]` indexing.
                            patch_failed = Some(SetError {
                                error_type: "notFound".into(),
                                description: Some("email not found".into()),
                            });
                        } else if current_record.mailbox_ids.len() > 1 || new_mbox.len() > 1 {
                            patch_failed = Some(SetError {
                                error_type: "invalidArguments".into(),
                                description: Some(
                                    "multi-mailbox replacement not yet supported".into(),
                                ),
                            });
                        } else if new_mbox.is_empty() {
                            // RFC 8621 §4.6: an email with zero
                            // mailbox memberships is ill-formed under
                            // the JMAP data model. Refuse the patch
                            // rather than dispatch a nominal-no-op
                            // move that would delete the email's only
                            // membership without a destroy entry in
                            // the changelog.
                            patch_failed = Some(SetError {
                                error_type: "invalidProperties".into(),
                                description: Some(
                                    "mailboxIds must contain at least one mailbox".into(),
                                ),
                            });
                        } else {
                            let src = current_record.mailbox_ids[0];
                            let dest = new_mbox[0];
                            if src != dest {
                                let retrain_label = junk_mailbox_id.and_then(|junk_id| {
                                    let was = src == junk_id;
                                    let now = dest == junk_id;
                                    if was == now {
                                        None
                                    } else if now {
                                        Some(Label::Spam)
                                    } else {
                                        Some(Label::Ham)
                                    }
                                });
                                planned_move =
                                    Some((dest, current_record.blob_hash, retrain_label));
                            }
                            // src == dest is a structural no-op;
                            // leave `planned_move = None` so Phase 3
                            // skips the move call.
                        }
                    }
                }
            }

            // ---- Phase 3: mutations. By the time we reach here,
            // patch_failed is None and any planned mutation is known
            // feasible against the snapshot we read in Phase 2.
            // Residual non-atomicity (race between the two writes
            // below, or between Phase 2 and Phase 3) is acknowledged
            // as an option 1b limitation — see `MailStore::patch_email`
            // tracking in Task 18c.
            if patch_failed.is_none()
                && let Some((flags, tags)) = new_keywords.clone()
            {
                // Top-level `keywords` replacement: JMAP only sees the
                // four mapped system keywords ($seen/$flagged/$answered/
                // $draft), so a "full replace" of the keywords object
                // must NOT clobber substrate bits that have no JMAP
                // counterpart (today: `\Deleted`). Read current state
                // and OR-in the preserved bits before the write. The
                // pointer-delta path already does this implicitly
                // (deltas start from `current.keywords.0`), so it's
                // skipped here.
                let merged_flags = if keywords_from_replace {
                    let ms = mailstore.clone();
                    let cur =
                        tokio::task::spawn_blocking(move || ms.get_email(account_id, item_id))
                            .await?;
                    match cur {
                        Ok(current) => {
                            // Same fan-out leak as the delta path:
                            // `set_keywords` writes union state to
                            // every membership, so a multi-mailbox
                            // replacement would clobber per-mailbox
                            // IMAP-only bits. Reject rather than
                            // corrupt state silently. See the delta-
                            // path comment above for the underlying
                            // primitive gap.
                            if current.mailbox_ids.len() > 1 {
                                patch_failed = Some(SetError {
                                    error_type: "invalidPatch".into(),
                                    description: Some(
                                        "keyword updates on multi-mailbox emails are not supported until per-membership JMAP keyword writes preserve IMAP-only flags"
                                            .into(),
                                    ),
                                });
                                None
                            } else {
                                const JMAP_BITS: u32 =
                                    Flags::SEEN | Flags::FLAGGED | Flags::ANSWERED | Flags::DRAFT;
                                Some(Flags(
                                    (current.keywords.0 & !JMAP_BITS) | (flags.0 & JMAP_BITS),
                                ))
                            }
                        }
                        Err(e) => {
                            patch_failed = Some(classify_email_not_found(&e));
                            None
                        }
                    }
                } else {
                    Some(flags)
                };

                if let Some(flags) = merged_flags {
                    let ms = mailstore.clone();
                    // `tags` is the complete new user-keyword set (delta-folded
                    // from current, or the whole-object replacement). For a
                    // replace, `merged_flags` preserves IMAP-only `\Deleted`;
                    // user keywords have no such hidden bits, so `tags` is the
                    // full intended set. `set_keywords` replaces both columns.
                    match tokio::task::spawn_blocking(move || {
                        ms.set_keywords(account_id, item_id, flags, tags)
                    })
                    .await?
                    {
                        Ok(()) => {
                            changed = true;
                        }
                        Err(e) => {
                            patch_failed = Some(classify_email_not_found(&e));
                        }
                    }
                }
            }

            if patch_failed.is_none()
                && let Some((dest, blob_hash, retrain_label)) = planned_move
            {
                let ms = mailstore.clone();
                match tokio::task::spawn_blocking(move || ms.move_email(account_id, item_id, dest))
                    .await?
                {
                    Ok(()) => {
                        changed = true;
                        // Junk-boundary retrain runs only after the
                        // move has committed, so a retrain stamp is
                        // never written for a move that didn't
                        // happen. The retrain itself is best-effort
                        // and never blocks the patch's success
                        // (matching legacy semantics).
                        if let Some(label) = retrain_label
                            && let Err(e) = retrain_for_move(
                                mailstore, classifier, account_id, item_id, blob_hash, label,
                            )
                            .await
                        {
                            tracing::warn!(
                                error = %e,
                                label = ?label,
                                "bayesian retrain on folder move failed"
                            );
                        }
                    }
                    Err(e) => {
                        patch_failed = Some(classify_move_error(&e));
                    }
                }
            }

            match patch_failed {
                Some(err) => {
                    not_updated.insert(id_str.clone(), err);
                }
                None if changed => {
                    updated_map.insert(id_str.clone(), serde_json::Value::Null);
                }
                None => {
                    // No-op patch (e.g. mailboxIds == current). RFC
                    // 8620 §5.3 says a no-op update may either be
                    // listed in `updated` with `null` or omitted; the
                    // legacy handler omitted it, so preserve that
                    // behavior.
                }
            }
        }
    }

    // ---- destroy ----
    if let Some(destroy) = args.get("destroy").and_then(|v| v.as_array()) {
        for id_val in destroy {
            let Some(id_str) = id_val.as_str() else {
                continue;
            };
            let Ok(item_uuid) = id_str.parse::<Uuid>() else {
                not_destroyed.insert(
                    id_str.to_string(),
                    SetError {
                        error_type: "notFound".into(),
                        description: None,
                    },
                );
                continue;
            };
            let item_id = ItemId(item_uuid);
            let ms = mailstore.clone();
            match tokio::task::spawn_blocking(move || ms.delete_email(account_id, item_id)).await? {
                Ok(()) => {
                    destroyed_list.push(id_str.to_string());
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.starts_with("email not found") || msg.starts_with("mailbox not found") {
                        not_destroyed.insert(
                            id_str.to_string(),
                            SetError {
                                error_type: "notFound".into(),
                                description: None,
                            },
                        );
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    let new_state_seq = {
        let ms = mailstore.clone();
        tokio::task::spawn_blocking(move || ms.email_state(account_id)).await??
    };

    let resp = SetResponse {
        account_id: acct,
        old_state: old_state_seq.to_string(),
        new_state: new_state_seq.to_string(),
        created: if created_map.is_empty() {
            None
        } else {
            Some(created_map)
        },
        updated: if updated_map.is_empty() {
            None
        } else {
            Some(updated_map.into_iter().collect())
        },
        destroyed: if destroyed_list.is_empty() {
            None
        } else {
            Some(destroyed_list)
        },
        not_created: if not_created.is_empty() {
            None
        } else {
            Some(not_created)
        },
        not_updated: if not_updated.is_empty() {
            None
        } else {
            Some(not_updated)
        },
        not_destroyed: if not_destroyed.is_empty() {
            None
        } else {
            Some(not_destroyed)
        },
    };

    Ok(serde_json::to_value(resp)?)
}

/// Map a `create_from_blob` failure to a JMAP [`SetError`]. The
/// helper threads typed prefixes (`invalidProperties:`,
/// `blobNotFound:`, `mailbox not found:`) through `anyhow` errors so
/// the JMAP layer can re-shape them into the corresponding
/// `SetError.type` without re-parsing the underlying source.
fn classify_create_error(e: &anyhow::Error) -> SetError {
    let msg = e.to_string();
    if let Some(rest) = msg.strip_prefix("invalidProperties: ") {
        SetError {
            error_type: "invalidProperties".into(),
            description: Some(rest.to_string()),
        }
    } else if let Some(rest) = msg.strip_prefix("blobNotFound: ") {
        SetError {
            error_type: "blobNotFound".into(),
            description: Some(rest.to_string()),
        }
    } else if let Some(rest) = msg.strip_prefix("serverFail: ") {
        // Internal failures (e.g. SQLite / hex-parse errors during
        // alias resolution) MUST surface as JMAP `serverFail`, not
        // `invalidArguments`. The client did nothing wrong; the
        // server hit an internal error and the wire shape needs to
        // say so.
        SetError {
            error_type: "serverFail".into(),
            description: Some(rest.to_string()),
        }
    } else {
        // Includes the substrate's `"mailbox not found"` /
        // `"blob not found"` (no typed prefix) plus any uncategorised
        // failures — JMAP's invalidArguments is the conservative bucket
        // for caller-shape problems we couldn't pin to a typed error.
        SetError {
            error_type: "invalidArguments".into(),
            description: Some(msg),
        }
    }
}

fn classify_email_not_found(e: &anyhow::Error) -> SetError {
    let msg = e.to_string();
    if msg.starts_with("email not found") || msg.starts_with("mailbox not found") {
        SetError {
            error_type: "notFound".into(),
            description: None,
        }
    } else {
        SetError {
            error_type: "serverFail".into(),
            description: Some(msg),
        }
    }
}

fn classify_move_error(e: &anyhow::Error) -> SetError {
    let msg = e.to_string();
    if msg.starts_with("email not found") || msg.starts_with("mailbox not found") {
        SetError {
            error_type: "notFound".into(),
            description: None,
        }
    } else if msg.starts_with("ambiguous source") {
        // Defensive: `move_email` rejects multi-membership sources,
        // but we already prevalidated above. If it surfaces anyway
        // (e.g. concurrent fan-out raced past our `get_email`), map
        // to invalidArguments.
        SetError {
            error_type: "invalidArguments".into(),
            description: Some("multi-mailbox replacement not yet supported".into()),
        }
    } else {
        SetError {
            error_type: "serverFail".into(),
            description: Some(msg),
        }
    }
}

/// Read the email's blob bytes through the MailStore's CAS and call
/// the bayesian classifier's retrain for a Junk-boundary move. The
/// blob is fetched via [`Mds::get_blob`] (CAS-keyed; no per-account
/// gate needed because the caller has already authenticated via
/// `account_id`'s set membership). Returns the classifier error
/// verbatim — the caller logs and proceeds.
async fn retrain_for_move(
    mailstore: &Arc<SqliteMailStore>,
    classifier: &Arc<DefaultClassifier>,
    account_id: i32,
    item_id: ItemId,
    blob_hash: cosmix_mds::BlobHash,
    label: Label,
) -> Result<()> {
    let ms = mailstore.clone();
    let blob_data = tokio::task::spawn_blocking(move || ms.mds().get_blob(&blob_hash)).await??;
    let acct = AccountId::new(account_id.to_string());
    let stamp_id = canonical_retrain_stamp(item_id);
    let req = RetrainRequest {
        stamp_id: &stamp_id,
        account: &acct,
        message: &blob_data,
        label,
    };
    classifier.retrain(&req).await?;
    Ok(())
}

fn canonical_retrain_stamp(item_id: ItemId) -> String {
    item_id.0.to_string()
}

/// Create an email item via the cosmix-mds-backed MailStore. Used by
/// `Email/set { create }` and `Email/import`.
///
/// Expected JMAP shape (RFC 8621 §4.5 / §4.6):
///
/// - `blobId` (required) — a UUID returned by `/jmap/upload`. Post
///   Task 3.3a, the upload mints a `MailStore::BlobId` alias (a
///   per-account `blob_refs` row tied to a temp item in
///   `__upload_staging__`), and this lookup resolves it via
///   `MailStore::resolve_blob_ref` → `BlobHash`. Pre-migration
///   `db::blob` UUIDs continue to resolve through the legacy
///   `db::blob::load_for_account` fallback for the duration of the
///   migration window. The CAS hex form (64 lowercase hex chars)
///   that `record_to_jmap` emits is still rejected here — the
///   round-trip "Email/get blobId → Email/set create blobId" is the
///   client's responsibility for that case.
/// - `mailboxIds` (required) — `Id[Boolean]` map of target containers.
///   Empty surfaces `invalidProperties: mailboxIds` per RFC 8621 §4.6.
/// - `keywords` (optional) — `Id[Boolean]` map. The four system
///   `$`-keywords route to flag bits; every other name (incl. registry
///   `$`-keywords) is normalised into `Tags`; a malformed name is rejected — see
///   [`keywords_json_to_flags_and_tags`].
/// - `receivedAt` (optional, Email/import only) — JMAP UTCDate string.
///   When absent, `chrono::Utc::now().timestamp_millis()` is used.
///
/// Returns the freshly-allocated [`ItemId`]. Errors carry one of the
/// classified prefixes (`invalidProperties:`, `blobNotFound:`,
/// `mailbox not found`) that [`classify_create_error`] re-shapes into
/// JMAP `SetError.type`.
async fn create_from_blob(
    db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    entry: &serde_json::Value,
    idempotent: bool,
) -> Result<ItemId> {
    use mail_parser::MessageParser;

    let blob_id_str = entry
        .get("blobId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("invalidProperties: blobId is required"))?;
    let blob_id: Uuid = blob_id_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalidProperties: Invalid blobId"))?;

    let mailbox_ids_obj = entry
        .get("mailboxIds")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("invalidProperties: mailboxIds is required"))?;
    // Strict: every key must parse as a UUID and every value must be
    // `true` (RFC 8621 §4.6 — Set-style id-keyed maps). A malformed
    // entry rejects the whole create rather than silently dropping
    // the bad key (which would create the email in a subset of the
    // requested mailboxes — confusing and surprising).
    let mut mailbox_ids: Vec<ContainerId> = Vec::with_capacity(mailbox_ids_obj.len());
    for (k, val) in mailbox_ids_obj {
        if val.as_bool() != Some(true) {
            return Err(anyhow::anyhow!(
                "invalidProperties: mailboxIds entry {k} must map to true"
            ));
        }
        let uuid = k
            .parse::<Uuid>()
            .map_err(|_| anyhow::anyhow!("invalidProperties: Invalid mailboxIds entry: {k}"))?;
        mailbox_ids.push(ContainerId(uuid));
    }
    if mailbox_ids.is_empty() {
        return Err(anyhow::anyhow!(
            "invalidProperties: mailboxIds must contain at least one valid mailbox ID"
        ));
    }

    // Resolve the `blobId` to a `BlobHash` and load bytes from the
    // MDS CAS. Two-tier lookup during the Task-3.3a migration window:
    //
    //   1. `MailStore::resolve_blob_ref(account, BlobId(uuid))` —
    //      the post-3.3a path. The alias is account-scoped at the
    //      `blob_refs` row level (`WHERE account_id = ?`), so a
    //      caller cannot reference another account's `BlobId` (the
    //      lookup returns "not found" rather than the foreign hash).
    //   2. Legacy fallback to `db::blob::load_for_account` for
    //      pre-migration `db::blob` UUIDs that still appear in
    //      client state. Same `(blob_id, account_id)` gating; the
    //      fallback collapses "foreign id" / "no such id" into a
    //      uniform `blobNotFound` so we never confirm whether
    //      someone else's blob exists.
    //
    // The fallback retires when (a) the `/download` gate moves to
    // an mds-side ownership oracle (option (b) in the migration
    // plan §3.3a) and (b) `db::blob` is fully retired in Phase 6.
    let blob_id_alias = crate::mailstore::BlobId(blob_id);
    let ms_for_resolve = mailstore.clone();
    let resolved_hash = tokio::task::spawn_blocking(move || {
        ms_for_resolve.resolve_blob_ref(account_id, blob_id_alias)
    })
    .await?;

    use crate::mailstore::BlobRefLookup;
    let resolved_lookup = resolved_hash
        .map_err(|e| anyhow::anyhow!("serverFail: alias resolve internal error: {e}"))?;
    let (blob_data, alias_hash) = match resolved_lookup {
        BlobRefLookup::Found(hash) => {
            let mds = mailstore.mds().clone();
            let bytes = tokio::task::spawn_blocking(move || mds.get_blob(&hash))
                .await?
                .map_err(|e| anyhow::anyhow!("blobNotFound: {blob_id}: {e}"))?;
            (bytes, Some(hash))
        }
        BlobRefLookup::NotFound => {
            let bytes = db::blob::load_for_account(&db.conn, &db.blob_dir, account_id, blob_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("blobNotFound: {blob_id}"))?;
            (bytes, None)
        }
        BlobRefLookup::Dangling => {
            // Alias minted but its CAS bytes are gone. Real
            // blobNotFound — MUST NOT fall back to legacy db::blob
            // (the client got this BlobId from us, so the alias is
            // authoritative for this account).
            return Err(anyhow::anyhow!("blobNotFound: {blob_id}"));
        }
    };

    let parser = MessageParser::default();
    let message = parser
        .parse(&blob_data)
        .ok_or_else(|| anyhow::anyhow!("invalidProperties: Failed to parse MIME message"))?;
    let envelope = build_envelope(&message);
    let in_reply_to = extract_in_reply_to(&message);

    // Strict: if `keywords` is present it must be an object whose
    // every value is `true` (RFC 8621 §4.6). Silent degradation to
    // an empty keyword set would let a malformed payload create the
    // email with no keywords instead of rejecting the request. The
    // split also rejects an unknown `$`-reserved keyword or a malformed
    // user keyword (mapped to `invalidProperties`).
    let (initial_keywords, initial_tags) = match entry.get("keywords") {
        None => (Flags(0), Tags::new()),
        Some(v) => {
            let obj = v
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("invalidProperties: keywords must be an object"))?;
            for (k, val) in obj {
                if val.as_bool() != Some(true) {
                    return Err(anyhow::anyhow!(
                        "invalidProperties: keywords entry {k} must map to true"
                    ));
                }
            }
            keywords_json_to_flags_and_tags(v)
                .map_err(|e| anyhow::anyhow!("invalidProperties: {e}"))?
        }
    };

    let received_at: i64 = match entry.get("receivedAt") {
        Some(v) if !v.is_null() => parse_utcdate_ms(v, "receivedAt")?,
        _ => chrono::Utc::now().timestamp_millis(),
    };

    let ms = mailstore.clone();
    let mailboxes = mailbox_ids;
    let envelope_owned = envelope;
    let in_reply_to_owned = in_reply_to;
    // Pre-resolve the hash. The post-3.3a alias path already gave us
    // a `BlobHash` (and `resolve_blob_ref` verified the underlying
    // CAS row exists), so we skip the redundant re-hash. The legacy
    // fallback path went through `db::blob::load_for_account` which
    // doesn't return a hash, so we put_blob the bytes (idempotent on
    // hash) to land them in the MDS CAS.
    let pre_hash: Option<BlobHash> = alias_hash;
    let item_id = tokio::task::spawn_blocking(move || -> Result<ItemId> {
        // Step 1: ensure the bytes are present in MDS CAS. For the
        // alias path, `resolve_blob_ref` already proved the CAS row
        // exists. For the legacy fallback path, the bytes came from
        // `db::blob`'s filesystem store (not the MDS CAS), so we
        // put_blob to land them there.
        let blob_hash = match pre_hash {
            Some(h) => h,
            None => ms.mds().put_blob(&blob_data)?,
        };

        // Step 2 (idempotent path only): if an existing email in the
        // account already has this blob_hash AND the same set of
        // mailbox memberships as requested, return its id rather
        // than allocating a new one. Backs the JMAP `Email/import`
        // idempotency contract — see
        // `_doc/planned/jmap-mds-migration.md` §3.5b. The check is
        // gated on the `idempotent` flag so regular `Email/set
        // create` keeps RFC-/legacy-default semantics where two
        // `create` entries for the same blob produce two distinct
        // emails.
        //
        // Atomicity scope: the find + create are split across two
        // operations, so two concurrent `Email/import` calls with
        // identical (blob_hash, mailbox-set) can both observe no
        // existing item and both create. The pre-staged contract
        // test `email_set_contract_import_is_idempotent_on_blob_hash`
        // exercises sequential repeat-import (which this check
        // covers); concurrent-import atomicity needs an
        // `Mds::create_email_if_not_exists_by_blob_hash` substrate
        // primitive that does the find + create inside one set
        // transaction, and is tracked as a follow-up under Task
        // 18d.
        if idempotent {
            let existing = ms.find_emails_by_blob_hash(account_id, &blob_hash)?;
            let want: std::collections::HashSet<ContainerId> = mailboxes.iter().copied().collect();
            for rec in existing {
                let have: std::collections::HashSet<ContainerId> =
                    rec.mailbox_ids.iter().copied().collect();
                if have == want {
                    // Match. Return the existing id; do not touch
                    // its keywords or received_at — the contract is
                    // "same blob into same mailbox returns same id",
                    // which means a re-import is a no-op rather than
                    // a re-write of the existing row's metadata.
                    return Ok(rec.id);
                }
            }
        }

        // Step 3: atomic create with N memberships in one
        // `with_set_tx`. The MembershipUid pairs are not surfaced to
        // JMAP today (RFC 8621 has no per-membership uid concept);
        // they're computed for IMAP / future use.
        let (id, _uids) = ms.create_email_with_memberships(
            account_id,
            &mailboxes,
            blob_hash,
            envelope_owned,
            &in_reply_to_owned,
            initial_keywords,
            // User keywords from the JMAP `keywords` object (the non-`$`
            // names), applied uniformly to every membership of the fresh
            // item in the same create transaction.
            initial_tags,
            received_at,
        )?;
        Ok(id)
    })
    .await??;

    Ok(item_id)
}

/// Email/import (RFC 8621 §4.6) — import raw RFC 5322 messages.
///
/// Migrated to the MailStore in Task 3.5b. State tokens derive from
/// [`MailStore::email_state`]; the legacy SQLite `changelog` table is
/// no longer consulted for the `Email` type. Per-entry success
/// tracking (`created` vs `notCreated`) honours JMAP §5.5 partial
/// success — one bad entry never aborts the batch.
pub async fn import(
    _db: &Db,
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let old_state_seq = {
        let ms = mailstore.clone();
        tokio::task::spawn_blocking(move || ms.email_state(account_id)).await??
    };

    let mut created_map = std::collections::HashMap::new();
    let mut not_created = std::collections::HashMap::new();

    if let Some(emails) = args.get("emails").and_then(|v| v.as_object()) {
        for (client_id, entry) in emails {
            // `Email/import` IS idempotent on (account, blob_hash,
            // mailbox-set): re-importing the same blob into the same
            // mailbox set returns the existing email id rather than
            // allocating a new one. See
            // `_doc/planned/jmap-mds-migration.md` §3.5b — the
            // migration plan promotes RFC 8621 §4.6's SHOULD to MUST
            // for the same-blob-same-mailbox case.
            match create_from_blob(_db, mailstore, account_id, entry, true).await {
                Ok(item_id) => {
                    // RFC 8621 §4.6: the import response carries the
                    // imported email's id, blobId, threadId, and size.
                    // `threadId` is substrate-pending (C6) and projects
                    // as null; `size` is read back via MailStore for an
                    // accurate post-write value.
                    let ms = mailstore.clone();
                    let size =
                        tokio::task::spawn_blocking(move || ms.get_email(account_id, item_id))
                            .await?
                            .map(|r| r.size_bytes)
                            .unwrap_or(0);
                    created_map.insert(
                        client_id.clone(),
                        serde_json::json!({
                            "id": item_id.0.to_string(),
                            "blobId": entry.get("blobId").and_then(|v| v.as_str()).unwrap_or(""),
                            "threadId": serde_json::Value::Null,
                            "size": size,
                        }),
                    );
                }
                Err(e) => {
                    not_created.insert(client_id.clone(), classify_create_error(&e));
                }
            }
        }
    }

    let new_state_seq = {
        let ms = mailstore.clone();
        tokio::task::spawn_blocking(move || ms.email_state(account_id)).await??
    };

    let resp = serde_json::json!({
        "accountId": acct,
        "oldState": old_state_seq.to_string(),
        "newState": new_state_seq.to_string(),
        "created": created_map,
        "notCreated": not_created.into_iter().map(|(k, v)| (k, serde_json::to_value(v).unwrap())).collect::<std::collections::HashMap<_, _>>(),
    });

    Ok(resp)
}

/// Wire-form name for a `serde_json::Value`, used in
/// `invalidArguments` descriptions so clients see *what* they sent
/// rather than a generic type-mismatch.
pub(super) fn type_name_of_json(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "Null",
        serde_json::Value::Bool(_) => "Boolean",
        serde_json::Value::Number(_) => "Number",
        serde_json::Value::String(_) => "String",
        serde_json::Value::Array(_) => "Array",
        serde_json::Value::Object(_) => "Object",
    }
}

/// Email/changes — RFC 8621 §4.1.5.
///
/// Sources deltas from the cosmix-mds `set_change` stream via
/// [`MailStore::email_changes`] rather than the legacy SQLite
/// `changelog` table. The MailStore primitive returns the *full*
/// delta since `sinceState`; this handler is the protocol-layer
/// gate that enforces JMAP `maxChanges` semantics — see RFC 8620
/// §5.2: when the server cannot return a delta within the cap, it
/// MUST respond with `cannotCalculateChanges`.
///
/// **Behaviour change vs. legacy.** The pre-migration handler
/// emitted partial responses with `hasMoreChanges: true` whenever
/// the row count exceeded `maxChanges`. The MailStore primitive
/// has no notion of a per-event seq cutoff inside the delta
/// window, so the handler converts overflow into the JMAP error
/// instead. JMAP clients honour `cannotCalculateChanges` by
/// re-syncing from a fresh `Email/get`, which is the conservative
/// outcome for an account that has accumulated more changes than
/// the client's chosen cap. Servers that want the partial-window
/// behaviour back can extend the MailStore primitive with a
/// `since_state, until_seq` bound and slice in the handler.
///
/// `sinceState` is the client's previously observed `set_change_seq`
/// (as a stringified `u64` per RFC 8620 §1.3 — the spec mandates the
/// String type for state tokens; bare-integer `sinceState` is
/// rejected with `invalidArguments` rather than silently coerced to
/// 0, which would force a full account walk). `newState` is the
/// highest seq in the returned window — including any
/// staging-container events that the primitive filtered out of the
/// touched set, so a client re-issuing
/// `Email/changes(sinceState = newState)` immediately after sees an
/// empty delta.
///
/// `maxChanges == 0` is treated as a 0-cap — any non-empty delta
/// produces `cannotCalculateChanges`. RFC 8620 §5.2 specifies
/// `maxChanges` as a `PositiveInt`, so 0 is technically out-of-spec
/// input; the conservative interpretation is "client asked for at
/// most 0 changes, anything else is overflow." Clients wanting "no
/// cap" must omit the field (the handler defaults to 500) or send
/// a large explicit value.
pub async fn changes(
    mailstore: &Arc<SqliteMailStore>,
    account_id: i32,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    // RFC 8620 §1.3 mandates state tokens be Strings. A bare integer
    // `sinceState: 42` would silently fail `as_str()` and fall back
    // to seq 0, forcing a full account walk — fail fast instead.
    // `null` is treated as invalid: per the spec, an absent
    // sinceState is conveyed by omission, not by an explicit JSON
    // null, and conflating the two would let a buggy client get a
    // full-sync where it intended a no-op.
    let since_state: u64 = match args.get("sinceState") {
        None => 0,
        Some(serde_json::Value::String(s)) => s.parse::<u64>().map_err(|_| JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some(format!("sinceState not a u64 string: {s:?}")),
        })?,
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "sinceState must be a String per RFC 8620 §1.3; got {}",
                    type_name_of_json(other)
                )),
            }
            .into());
        }
    };

    // RFC 8620 §5.2 specifies `maxChanges` as `PositiveInt` (a
    // u32). A negative or non-integer JSON value would silently
    // fall through `as_u64()` to the 500 default and produce a
    // request that decoded as "no cap" instead of erroring — the
    // same silent-coercion class as the prior `sinceState` bug.
    let max_changes: u64 = match args.get("maxChanges") {
        None | Some(serde_json::Value::Null) => 500,
        Some(serde_json::Value::Number(n)) => n.as_u64().ok_or_else(|| JmapMethodError {
            error_type: "invalidArguments".into(),
            description: Some(format!(
                "maxChanges must fit a u64 (RFC 8620 §5.2 specifies PositiveInt); got {n}"
            )),
        })?,
        Some(other) => {
            return Err(JmapMethodError {
                error_type: "invalidArguments".into(),
                description: Some(format!(
                    "maxChanges must be a Number per RFC 8620 §5.2; got {}",
                    type_name_of_json(other)
                )),
            }
            .into());
        }
    };

    let ms = mailstore.clone();
    let result =
        match tokio::task::spawn_blocking(move || ms.email_changes(account_id, since_state)).await?
        {
            Ok(r) => r,
            Err(e) => {
                // Per RFC 8620 §5.2: a `sinceState` token that is not
                // a valid prior state MUST surface as
                // `cannotCalculateChanges`. The mailstore primitive
                // returns `Err` with the `"future since_state"` prefix
                // when `since_state` is above the per-set head, and
                // `"stale since_state"` when it is below the retention
                // floor (MCS Phase 3-B — a prior prune has discarded
                // the rows needed to reconstruct the delta). Both map
                // to the same JMAP error. The other primitive errors
                // (`mailbox not found`, `since_state out of range`)
                // stay on their existing pathways.
                let s = e.to_string();
                if s.starts_with("future since_state") || s.starts_with("stale since_state") {
                    return Err(JmapMethodError {
                        error_type: "cannotCalculateChanges".into(),
                        description: Some(s),
                    }
                    .into());
                }
                return Err(e);
            }
        };

    // Apply the JMAP `maxChanges` cap at the protocol layer per
    // `MailStore::email_changes` doc. Total is the sum of the three
    // partition arrays — items only ever appear in one of them, so
    // a simple sum is the correct count of distinct events the
    // client would observe.
    let total = result.created.len() + result.updated.len() + result.destroyed.len();
    if (total as u64) > max_changes {
        return Err(JmapMethodError {
            error_type: "cannotCalculateChanges".into(),
            description: Some(format!(
                "{total} changes since state {since_state}; exceeds maxChanges {max_changes}"
            )),
        }
        .into());
    }

    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state.to_string(),
        // The MailStore primitive returns the full delta, so the
        // client has nothing more to fetch in this window.
        has_more_changes: false,
        created: result
            .created
            .into_iter()
            .map(|i| i.0.to_string())
            .collect(),
        updated: result
            .updated
            .into_iter()
            .map(|i| i.0.to_string())
            .collect(),
        destroyed: result
            .destroyed
            .into_iter()
            .map(|i| i.0.to_string())
            .collect(),
        // RFC 8621 §4.1.5: Email/changes does not carry
        // updatedProperties. ChangesResponse skip-if-none keeps the
        // field off the wire when left at None.
        updated_properties: None,
    };

    Ok(serde_json::to_value(resp)?)
}

#[cfg(test)]
mod tests {
    //! Projection tests for `record_to_jmap` and `flags_to_keywords`.
    //! The async `get` handler exercises full MailStore plumbing in
    //! `mailstore::tests`; these tests cover the JSON-shape mapping
    //! that the handler layers on top.

    use super::*;
    use cosmix_mds::{BlobHash, ContainerId, Flags, ItemId, Tags};

    use crate::mailstore::{EmailEnvelope, EmailRecord};

    fn sample_record() -> EmailRecord {
        EmailRecord {
            id: ItemId(Uuid::nil()),
            blob_hash: BlobHash([0u8; 32]),
            size_bytes: 1234,
            received_at: 1_700_000_000_000,
            envelope: EmailEnvelope {
                from: "alice@example.com".into(),
                to: vec!["bob@example.com".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                reply_to: Vec::new(),
                subject: "hello".into(),
                date: 1_699_999_900,
                message_id: Some("<m1@example.com>".into()),
            },
            keywords: Flags(Flags::SEEN),
            tags: Tags::new(),
            mailbox_ids: vec![ContainerId(Uuid::nil())],
        }
    }

    #[test]
    fn record_to_jmap_emits_rfc8621_field_names() {
        let v = record_to_jmap(&sample_record());
        let obj = v.as_object().unwrap();

        assert_eq!(obj["id"], serde_json::json!(Uuid::nil().to_string()));
        // BlobHash projects to lowercase hex of the underlying 32-byte
        // CAS hash; not the legacy per-account UUID alias. 32 bytes ⇒
        // 64 lowercase hexdigit chars.
        let blob_id = obj["blobId"].as_str().unwrap();
        assert_eq!(blob_id.len(), 64);
        assert!(
            blob_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "blobId must be lowercase ASCII hex, got {blob_id:?}"
        );
        assert_eq!(obj["size"], serde_json::json!(1234u64));
        // RFC 8620 §1.4 / RFC 3339 with literal `Z`. EmailRecord
        // received_at is unix-millis (1_700_000_000_000 → 2023-11-14
        // 22:13:20.000Z). Tests pin the exact shape — accidental
        // collation changes (offset suffix, ms precision drop) are
        // wire breaks for clients that parse the canonical form.
        assert_eq!(
            obj["receivedAt"],
            serde_json::json!("2023-11-14T22:13:20.000Z")
        );
        // RFC 8621 §4.1.1: `mailboxIds` is `Id[Boolean]` — object form,
        // each value `true`.
        assert_eq!(
            obj["mailboxIds"],
            serde_json::json!({ Uuid::nil().to_string(): true })
        );
        assert_eq!(obj["subject"], serde_json::json!("hello"));
        assert_eq!(obj["messageId"], serde_json::json!(["<m1@example.com>"]));
        assert_eq!(obj["from"], serde_json::json!("alice@example.com"));
        assert_eq!(obj["to"], serde_json::json!(["bob@example.com"]));
        // EmailEnvelope.date is unix-seconds (1_699_999_900 →
        // 2023-11-14 22:11:40Z); seconds form keeps the JMAP
        // canonical second precision.
        assert_eq!(obj["date"], serde_json::json!("2023-11-14T22:11:40Z"));
        assert_eq!(obj["keywords"], serde_json::json!({"$seen": true}));
    }

    #[test]
    fn record_to_jmap_emits_null_message_id_when_absent() {
        let mut r = sample_record();
        r.envelope.message_id = None;
        let v = record_to_jmap(&r);
        assert!(v["messageId"].is_null());
    }

    #[test]
    fn flags_to_keywords_empty_flags_is_empty_object() {
        let v = flags_to_keywords(Flags(0));
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn flags_to_keywords_seen_bit_only() {
        let v = flags_to_keywords(Flags(Flags::SEEN));
        assert_eq!(v, serde_json::json!({"$seen": true}));
    }

    #[test]
    fn flags_to_keywords_all_allocated_bits() {
        // $deleted has no JMAP counterpart (IMAP EXPUNGE-only marker)
        // so the bit IS set in Flags but is silently dropped from the
        // keywords projection, per RFC 8621 §4.1.4.
        let v = flags_to_keywords(Flags(Flags::ALL_ALLOCATED));
        assert_eq!(
            v,
            serde_json::json!({
                "$seen": true,
                "$flagged": true,
                "$answered": true,
                "$draft": true,
            })
        );
    }

    #[test]
    fn flags_to_keywords_flagged_answered_draft() {
        let v = flags_to_keywords(Flags(Flags::FLAGGED | Flags::ANSWERED | Flags::DRAFT));
        assert_eq!(
            v,
            serde_json::json!({
                "$flagged": true,
                "$answered": true,
                "$draft": true,
            })
        );
    }

    #[test]
    fn flags_to_keywords_deleted_bit_drops_silently() {
        // \Deleted has no JMAP counterpart per the Flags doc table —
        // ensure it does not surface as some opaque `$deleted` field.
        let v = flags_to_keywords(Flags(Flags::DELETED));
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn flags_to_keywords_unmapped_bits_drop_silently() {
        // Bits >= 5 are not allocated to any JMAP keyword in the
        // substrate. They must not project to opaque field names — the
        // JMAP keyword set becomes empty if only unallocated bits are
        // set, matching RFC 8621 §4.1.4 (unknown keywords are silently
        // ignored).
        let unmapped = Flags(!Flags::ALL_ALLOCATED);
        let v = flags_to_keywords(unmapped);
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn flags_and_tags_to_keywords_unions_system_and_user() {
        // GET projection: system flags + user keywords in one object.
        let tags = Tags::from(vec!["public".to_string(), "work".to_string()]);
        let v = flags_and_tags_to_keywords(Flags(Flags::SEEN | Flags::FLAGGED), &tags);
        assert_eq!(
            v,
            serde_json::json!({"$seen": true, "$flagged": true, "public": true, "work": true})
        );
        // Empty tags = the plain system-flag projection.
        assert_eq!(
            flags_and_tags_to_keywords(Flags(Flags::SEEN), &Tags::new()),
            serde_json::json!({"$seen": true})
        );
    }

    #[test]
    fn keywords_json_split_routes_system_and_user() {
        // SET parse: $-system → Flags; user → normalised Tags.
        let (flags, tags) = keywords_json_to_flags_and_tags(&serde_json::json!({
            "$seen": true,
            "$draft": true,
            "Public": true,
            "WORK": true,
        }))
        .unwrap();
        assert_eq!(flags, Flags(Flags::SEEN | Flags::DRAFT));
        assert_eq!(
            tags,
            Tags::from(vec!["public".to_string(), "work".to_string()])
        );
    }

    #[test]
    fn keywords_json_split_stores_registry_dollar_keyword() {
        // A registered (non-system) $-name is a valid user keyword (real clients
        // use $Forwarded/$Junk/$label2), stored normalised — NOT rejected.
        let (flags, tags) = keywords_json_to_flags_and_tags(
            &serde_json::json!({"$Forwarded": true, "$seen": true}),
        )
        .unwrap();
        assert_eq!(flags, Flags(Flags::SEEN));
        assert_eq!(tags, Tags::from(vec!["$forwarded".to_string()]));
    }

    #[test]
    fn keywords_json_split_enforces_count_cap() {
        let mut obj = serde_json::Map::new();
        for i in 0..(crate::keyword::MAX_KEYWORDS_PER_MESSAGE + 1) {
            obj.insert(format!("tag{i}"), serde_json::Value::Bool(true));
        }
        assert!(matches!(
            keywords_json_to_flags_and_tags(&serde_json::Value::Object(obj)),
            Err(crate::keyword::KeywordError::TooMany)
        ));
    }

    #[test]
    fn keywords_json_split_non_object_is_empty() {
        let (flags, tags) = keywords_json_to_flags_and_tags(&serde_json::Value::Null).unwrap();
        assert_eq!(flags, Flags(0));
        assert!(tags.is_empty());
    }

    #[test]
    fn record_to_jmap_empty_mailbox_ids_projects_empty_object() {
        // RFC 8621 §4.1.1: `mailboxIds` is `Id[Boolean]`. The empty
        // case is `{}`, not `[]` — emitting an array would break
        // strict JMAP clients that decode into `Map<Id, Boolean>`.
        let mut r = sample_record();
        r.mailbox_ids.clear();
        let v = record_to_jmap(&r);
        assert_eq!(v["mailboxIds"], serde_json::json!({}));
    }

    #[test]
    fn record_to_jmap_empty_to_projects_empty_array() {
        let mut r = sample_record();
        r.envelope.to.clear();
        let v = record_to_jmap(&r);
        assert_eq!(v["to"], serde_json::json!([]));
    }

    #[test]
    fn record_to_jmap_substrate_pending_fields_project_as_null_or_false() {
        // C5 contract (post-Task 5.4a): `cc`, `bcc`, `replyTo` are now
        // substrate-backed (cosmix-mds v1.3 schema) and project as
        // arrays — empty when the source had no Cc/Bcc/Reply-To
        // header. The keys that remain null/false are the ones still
        // pending C6 substrate work: threadId, inReplyTo, preview,
        // hasAttachment, spamScore, spamVerdict.
        let v = record_to_jmap(&sample_record());
        let obj = v.as_object().unwrap();

        assert!(obj["threadId"].is_null());
        assert!(obj["inReplyTo"].is_null());
        assert!(obj["preview"].is_null());
        assert!(obj["hasAttachment"].is_null());
        assert!(obj["spamScore"].is_null());
        assert!(obj["spamVerdict"].is_null());
    }

    #[test]
    fn record_to_jmap_from_to_project_as_bare_strings_pre_substrate() {
        // C5 contract: pre-C6, `from` / `to` / `cc` / `bcc` /
        // `replyTo` carry the bare-string / string-array shape from
        // `EmailEnvelope`, NOT the RFC 8621 §4.1.2 `[EmailAddress]`
        // object form. Documenting the currently-shipped shape so a
        // future C6 migration that changes it is a deliberate wire
        // break, not an accident.
        let v = record_to_jmap(&sample_record());
        let obj = v.as_object().unwrap();
        assert_eq!(obj["from"], serde_json::json!("alice@example.com"));
        assert_eq!(obj["to"], serde_json::json!(["bob@example.com"]));
        // sample_record's envelope has empty cc/bcc/reply_to.
        assert_eq!(obj["cc"], serde_json::json!([]));
        assert_eq!(obj["bcc"], serde_json::json!([]));
        assert_eq!(obj["replyTo"], serde_json::json!([]));
    }

    #[test]
    fn apply_email_properties_none_returns_full_projection() {
        // RFC 8620 §5.1: omitted `properties` argument means full
        // projection. The filter must be a no-op in that case.
        let v = record_to_jmap(&sample_record());
        let filtered = apply_email_properties(v.clone(), None);
        assert_eq!(v, filtered);
    }

    #[test]
    fn apply_email_properties_subset_keeps_id_and_named_fields() {
        // Client requests `["subject", "size"]`; response must
        // contain `id` (always returned per RFC) plus those two,
        // and nothing else.
        let v = record_to_jmap(&sample_record());
        let props = vec!["subject".to_string(), "size".to_string()];
        let filtered = apply_email_properties(v, Some(&props));
        let obj = filtered.as_object().unwrap();

        assert!(obj.contains_key("id"), "id must always be returned");
        assert!(obj.contains_key("subject"));
        assert!(obj.contains_key("size"));
        assert!(!obj.contains_key("blobId"));
        assert!(!obj.contains_key("from"));
        assert!(!obj.contains_key("keywords"));
        assert_eq!(obj.len(), 3);
    }

    #[test]
    fn apply_email_properties_empty_list_is_id_only() {
        // RFC 8620 §5.1 ambiguity: an empty list is "no fields
        // requested". The server is required to return `id`
        // always, so the projection becomes id-only — not the
        // full row, not an empty object.
        let v = record_to_jmap(&sample_record());
        let filtered = apply_email_properties(v, Some(&[]));
        let obj = filtered.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.contains_key("id"));
    }

    #[test]
    fn apply_email_properties_unknown_field_is_silently_dropped() {
        // RFC 8620 §5.1: server returns properties it knows about;
        // unknown names are dropped (not an error). Clients can
        // request optimistically without per-server feature
        // detection.
        let v = record_to_jmap(&sample_record());
        let props = vec!["subject".to_string(), "nosuchfield".to_string()];
        let filtered = apply_email_properties(v, Some(&props));
        let obj = filtered.as_object().unwrap();
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("subject"));
        assert!(!obj.contains_key("nosuchfield"));
        assert_eq!(obj.len(), 2);
    }

    // ---------- Email/query argument parsing ----------

    fn err_kind(e: &anyhow::Error) -> Option<(String, Option<String>)> {
        e.downcast_ref::<JmapMethodError>()
            .map(|m| (m.error_type.clone(), m.description.clone()))
    }

    #[test]
    fn parse_query_filter_none_or_null_yields_default() {
        assert!(parse_query_filter(None).unwrap().in_mailbox.is_none());
        let null = serde_json::Value::Null;
        assert!(
            parse_query_filter(Some(&null))
                .unwrap()
                .in_mailbox
                .is_none()
        );
    }

    #[test]
    fn parse_query_filter_full_supported_subset() {
        let mb = Uuid::new_v4();
        let other = Uuid::new_v4();
        let v = serde_json::json!({
            "inMailbox": mb.to_string(),
            "inMailboxOtherThan": [other.to_string()],
            "before": "2026-01-01T00:00:00Z",
            "after": "2025-01-01T00:00:00Z",
            "minSize": 1024u64,
            "maxSize": 1_000_000u64,
            "hasKeyword": "$seen",
            "notKeyword": "$seen",
            "text": "quarterly invoice",
        });
        let f = parse_query_filter(Some(&v)).unwrap();
        assert_eq!(f.in_mailbox, Some(ContainerId(mb)));
        assert_eq!(f.in_mailbox_other_than, Some(vec![ContainerId(other)]));
        assert!(f.before.is_some_and(|b| b > 1_700_000_000_000));
        assert!(
            f.after
                .is_some_and(|a| a > 1_700_000_000_000 - 365 * 86_400_000)
        );
        assert_eq!(f.min_size, Some(1024));
        assert_eq!(f.max_size, Some(1_000_000));
        assert_eq!(f.has_keyword, Some(Flags(Flags::SEEN)));
        assert_eq!(f.not_keyword, Some(Flags(Flags::SEEN)));
        assert_eq!(f.text.as_deref(), Some("quarterly invoice"));
    }

    #[test]
    fn parse_query_filter_empty_in_mailbox_other_than_drops_to_none() {
        // RFC 8621 §4.4.1 semantics for `inMailboxOtherThan: []` are a
        // no-op — every email "has a mailbox not in []" trivially. The
        // parser should drop the empty array to `None` so the storage
        // layer skips the per-survivor `item_memberships` lookup that
        // `Some(vec![])` would otherwise force.
        let v = serde_json::json!({ "inMailboxOtherThan": [] });
        let f = parse_query_filter(Some(&v)).unwrap();
        assert!(f.in_mailbox_other_than.is_none());
    }

    #[test]
    fn parse_query_filter_operator_surfaces_unsupported_filter() {
        let v = serde_json::json!({
            "operator": "AND",
            "conditions": [{ "minSize": 1u64 }],
        });
        let (kind, _) = err_kind(&parse_query_filter(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "unsupportedFilter");
    }

    #[test]
    fn parse_query_filter_envelope_property_surfaces_unsupported_filter() {
        // `text` is now SUPPORTED (whole-message FTS); the field-scoped
        // substring/full-text filters remain unsupported.
        for prop in [
            "from",
            "to",
            "cc",
            "subject",
            "body",
            "header",
            "hasAttachment",
        ] {
            let v = serde_json::json!({ prop: "x" });
            let (kind, _) = err_kind(&parse_query_filter(Some(&v)).unwrap_err()).unwrap();
            assert_eq!(kind, "unsupportedFilter", "property={prop}");
        }
    }

    #[test]
    fn parse_query_filter_text_is_supported() {
        let v = serde_json::json!({ "text": "quarterly invoice" });
        let f = parse_query_filter(Some(&v)).unwrap();
        assert_eq!(f.text.as_deref(), Some("quarterly invoice"));
        // A non-string text is a malformed value for a supported property.
        let v = serde_json::json!({ "text": 42 });
        let (kind, _) = err_kind(&parse_query_filter(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "invalidArguments");
    }

    #[test]
    fn parse_query_filter_unknown_property_surfaces_unsupported_filter() {
        let v = serde_json::json!({ "totallyMadeUp": 1 });
        let (kind, _) = err_kind(&parse_query_filter(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "unsupportedFilter");
    }

    #[test]
    fn parse_query_filter_malformed_in_mailbox_surfaces_invalid_arguments() {
        let v = serde_json::json!({ "inMailbox": "not-a-uuid" });
        let (kind, _) = err_kind(&parse_query_filter(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "invalidArguments");
    }

    #[test]
    fn parse_query_filter_malformed_keyword_surfaces_unsupported_filter() {
        // A malformed keyword name (atom-special / space) is the safe-refusal
        // case; registry $-keywords are now valid (routed to has_tag).
        for kw in ["a(b", "a b"] {
            let v = serde_json::json!({ "hasKeyword": kw });
            let (kind, _) = err_kind(&parse_query_filter(Some(&v)).unwrap_err()).unwrap();
            assert_eq!(kind, "unsupportedFilter", "keyword={kw}");
        }
        let v = serde_json::json!({ "hasKeyword": "$Forwarded" });
        let f = parse_query_filter(Some(&v)).unwrap();
        assert_eq!(f.has_tag.as_deref(), Some("$forwarded"));
    }

    #[test]
    fn parse_query_filter_user_keyword_routes_to_tag_predicate() {
        // A non-$ user keyword is now a supported filter: it populates the
        // normalised has_tag / not_tag predicate, not has_keyword.
        let v = serde_json::json!({ "hasKeyword": "Public" });
        let f = parse_query_filter(Some(&v)).unwrap();
        assert_eq!(f.has_tag.as_deref(), Some("public"));
        assert!(f.has_keyword.is_none());

        let v = serde_json::json!({ "notKeyword": "WeirdO" });
        let f = parse_query_filter(Some(&v)).unwrap();
        assert_eq!(f.not_tag.as_deref(), Some("weirdo"));
        assert!(f.not_keyword.is_none());
    }

    #[test]
    fn parse_query_filter_malformed_utcdate_surfaces_invalid_arguments() {
        let v = serde_json::json!({ "before": "not-a-date" });
        let (kind, _) = err_kind(&parse_query_filter(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "invalidArguments");
    }

    #[test]
    fn parse_query_sort_default_when_missing_or_empty() {
        assert_eq!(parse_query_sort(None).unwrap(), EmailSort::ReceivedAtDesc);
        let null = serde_json::Value::Null;
        assert_eq!(
            parse_query_sort(Some(&null)).unwrap(),
            EmailSort::ReceivedAtDesc
        );
        let empty = serde_json::json!([]);
        assert_eq!(
            parse_query_sort(Some(&empty)).unwrap(),
            EmailSort::ReceivedAtDesc
        );
    }

    #[test]
    fn parse_query_sort_received_at_is_ascending_round_trip() {
        let asc = serde_json::json!([{"property": "receivedAt", "isAscending": true}]);
        assert_eq!(
            parse_query_sort(Some(&asc)).unwrap(),
            EmailSort::ReceivedAtAsc
        );
        let desc = serde_json::json!([{"property": "receivedAt", "isAscending": false}]);
        assert_eq!(
            parse_query_sort(Some(&desc)).unwrap(),
            EmailSort::ReceivedAtDesc
        );
        // RFC 8620 §5.5 default for isAscending is false.
        let no_asc = serde_json::json!([{"property": "receivedAt"}]);
        assert_eq!(
            parse_query_sort(Some(&no_asc)).unwrap(),
            EmailSort::ReceivedAtDesc
        );
    }

    #[test]
    fn parse_query_sort_multi_comparator_surfaces_unsupported_sort() {
        let v = serde_json::json!([
            {"property": "receivedAt"},
            {"property": "receivedAt", "isAscending": true},
        ]);
        let (kind, _) = err_kind(&parse_query_sort(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "unsupportedSort");
    }

    #[test]
    fn parse_query_sort_other_property_surfaces_unsupported_sort() {
        // receivedAt/subject/from are supported; everything else still rejects.
        for prop in ["sentAt", "size", "to", "hasKeyword"] {
            let v = serde_json::json!([{ "property": prop }]);
            let (kind, _) = err_kind(&parse_query_sort(Some(&v)).unwrap_err()).unwrap();
            assert_eq!(kind, "unsupportedSort", "property={prop}");
        }
    }

    #[test]
    fn parse_query_sort_subject_and_from_round_trip() {
        let cases = [
            ("subject", true, EmailSort::SubjectAsc),
            ("subject", false, EmailSort::SubjectDesc),
            ("from", true, EmailSort::FromAsc),
            ("from", false, EmailSort::FromDesc),
        ];
        for (prop, asc, want) in cases {
            let v = serde_json::json!([{ "property": prop, "isAscending": asc }]);
            assert_eq!(
                parse_query_sort(Some(&v)).unwrap(),
                want,
                "property={prop} asc={asc}"
            );
        }
        // isAscending defaults to false (descending) per RFC 8620 §5.5.
        let no_asc = serde_json::json!([{ "property": "subject" }]);
        assert_eq!(
            parse_query_sort(Some(&no_asc)).unwrap(),
            EmailSort::SubjectDesc
        );
    }

    #[test]
    fn parse_query_sort_explicit_collation_surfaces_unsupported_sort() {
        let v = serde_json::json!([{
            "property": "receivedAt",
            "collation": "i;ascii-casemap",
        }]);
        let (kind, _) = err_kind(&parse_query_sort(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "unsupportedSort");
    }

    #[test]
    fn parse_query_sort_non_array_surfaces_invalid_arguments() {
        let v = serde_json::json!("receivedAt");
        let (kind, _) = err_kind(&parse_query_sort(Some(&v)).unwrap_err()).unwrap();
        assert_eq!(kind, "invalidArguments");
    }

    #[test]
    fn parse_keyword_filter_routes_system_user_and_rejects_reserved() {
        // System `$`-names route to a Flags predicate.
        let system = [
            ("$seen", Flags::SEEN),
            ("$flagged", Flags::FLAGGED),
            ("$answered", Flags::ANSWERED),
            ("$draft", Flags::DRAFT),
        ];
        for (kw, expected_bit) in system {
            assert_eq!(
                parse_keyword_filter(&serde_json::json!(kw), "hasKeyword").unwrap(),
                KeywordFilterTarget::System(Flags(expected_bit)),
                "keyword={kw}"
            );
        }
        // User keywords route to a normalised (NFC + lowercase) Tags predicate —
        // including registry `$`-names ($Forwarded etc.), stored as-is.
        assert_eq!(
            parse_keyword_filter(&serde_json::json!("UserDefined"), "hasKeyword").unwrap(),
            KeywordFilterTarget::User("userdefined".to_string())
        );
        assert_eq!(
            parse_keyword_filter(&serde_json::json!("$Forwarded"), "notKeyword").unwrap(),
            KeywordFilterTarget::User("$forwarded".to_string())
        );
        // Only a malformed name (atom-special) is refused as unsupportedFilter.
        for kw in ["a(b", "bad name"] {
            let (kind, _) =
                err_kind(&parse_keyword_filter(&serde_json::json!(kw), "hasKeyword").unwrap_err())
                    .unwrap();
            assert_eq!(kind, "unsupportedFilter", "keyword={kw}");
        }
    }

    #[test]
    fn parse_utcdate_ms_round_trip_and_rejects_garbage() {
        let ms = parse_utcdate_ms(&serde_json::json!("2024-01-01T00:00:00Z"), "before").unwrap();
        // 2024-01-01T00:00:00Z = 1704067200 unix seconds = 1_704_067_200_000 ms.
        assert_eq!(ms, 1_704_067_200_000);
        let (kind, _) =
            err_kind(&parse_utcdate_ms(&serde_json::json!("yesterday"), "before").unwrap_err())
                .unwrap();
        assert_eq!(kind, "invalidArguments");
    }

    // ---------------------------------------------------------------
    // Pre-staged contract tests for Tasks 3.3 / 3.4 / 3.5 / 3.5b.
    //
    // These are `#[ignore]`'d stubs that document the JMAP semantics
    // the MailStore-backed Email/set rewrite must honour. They exist
    // so the Codex-reviewed substrate work can convert each into a
    // real failing test before implementation, then green it.
    //
    // Run them explicitly with:
    //   cargo test -p cosmix-maild -- --ignored email_set_contract
    //
    // Until then they panic with the expected behaviour as the
    // panic message — surfacing the contract in test output, not
    // just in comments.
    // ---------------------------------------------------------------

    /// Task 3.3 — `Email/set { create }` must accept multiple mailbox
    /// memberships in a single call. JMAP `mailboxIds` is
    /// `Id[Boolean]` (RFC 8621 §4.1.1) — a set of containers the
    /// email belongs to, not a primary + extras. The current
    /// `MailStore::create_email` accepts a single `ContainerId`
    /// only; the migration must either extend the storage trait to
    /// take `&[ContainerId]` or add a follow-up `set_memberships`
    /// step that runs in the *same* `with_set_tx` so partial
    /// failure cannot leave an email in only some of its requested
    /// mailboxes.
    #[test]
    #[ignore = "contract-stub for Task 3.3 — see comment"]
    fn email_set_contract_create_honours_multi_mailbox_ids() {
        panic!(
            "Email/set create with `mailboxIds: {{ inboxId: true, archiveId: true }}` \
             must materialise the email in BOTH mailboxes atomically. Failure mode \
             to guard against: partial materialisation if the second membership write \
             fails after the first commits."
        );
    }

    /// Task 3.3 — empty `mailboxIds: {}` must surface
    /// `invalidProperties` per RFC 8621 §4.6 (an Email with zero
    /// mailbox memberships is ill-formed under the JMAP data
    /// model). The legacy SQL path silently accepted it.
    #[test]
    #[ignore = "contract-stub for Task 3.3 — see comment"]
    fn email_set_contract_create_rejects_empty_mailbox_ids() {
        panic!(
            "Email/set create with empty `mailboxIds` must enter `notCreated` with \
             `invalidProperties` and the offending property name. Silent acceptance \
             leaves orphaned emails in the substrate."
        );
    }

    /// Task 3.4 — partial success: one failing update among many
    /// must not abort the whole batch. RFC 8620 §5.3 mandates
    /// per-id success tracking via `updated` vs `notUpdated`. The
    /// current handler short-circuits on the first error, dropping
    /// later valid updates on the floor.
    #[test]
    #[ignore = "contract-stub for Task 3.4 — see comment"]
    fn email_set_contract_update_partial_success_per_id() {
        panic!(
            "Three updates: id_a (valid), id_b (unknown id), id_c (valid). The response \
             must include `updated: {{ id_a: null, id_c: null }}` and `notUpdated: {{ id_b: \
             {{ type: \"notFound\" }} }}` — id_c must NOT be skipped because id_b failed."
        );
    }

    /// Task 3.5 — destroy ordering. RFC 8620 §5.3 specifies that
    /// within a single `/set` call the server processes
    /// `create → update → destroy`, and that destroy uses the
    /// post-update mailbox memberships. A tx that destroys
    /// before update commits would race against concurrent
    /// readers and violate the changelog state-monotonicity
    /// invariant relied on by `Email/changes`.
    #[test]
    #[ignore = "contract-stub for Task 3.5 — see comment"]
    fn email_set_contract_destroy_runs_after_update_in_same_state_tx() {
        panic!(
            "An Email/set with both `update: {{ id: {{ keywords/$seen: true }} }}` and \
             `destroy: [id]` must apply the keyword change AND destroy in one set-tx, \
             with the destroy effective for any subsequent `Email/changes(sinceState)`. \
             Two separate transactions break state-monotonicity."
        );
    }

    /// Task 3.5b — `Email/import` is distinct from `Email/set
    /// create`: it takes a `blobId` + `receivedAt` + `mailboxIds` +
    /// `keywords` and ingests an RFC 5322 blob already in
    /// staging. Unlike create, import is the path mailing-list
    /// servers and migration tools use, and it MUST be
    /// idempotent on the same blob hash (re-import returns the
    /// existing email id, not a duplicate row). This is the
    /// substrate property that lets a mailbox replay against
    /// the same staging directory without exploding.
    #[test]
    #[ignore = "contract-stub for Task 3.5b — see comment"]
    fn email_set_contract_import_is_idempotent_on_blob_hash() {
        panic!(
            "Email/import called twice with the same `blobId` and the same target \
             mailbox must return the same email id and emit exactly one `created` \
             row in the changelog. Second invocation MUST NOT create a duplicate \
             item nor a second changelog entry."
        );
    }

    /// T10 — Multi-mailbox keyword updates must reject with
    /// `invalidPatch` rather than silently fan-out IMAP-only flags
    /// (e.g. `\Deleted`) across every membership. `MailStore::
    /// set_keywords` writes union state to every membership and
    /// `get_email` returns a union projection, so applying a JMAP
    /// keyword delta starting from the union would clobber per-mailbox
    /// IMAP-only bits. Until a per-membership JMAP keyword primitive
    /// lands, both top-level `keywords` replacement and
    /// `keywords/$xyz` pointer-deltas must refuse the patch when the
    /// email has more than one membership.
    #[tokio::test]
    async fn email_set_multi_mailbox_keyword_update_rejects_invalid_patch() {
        use crate::mailstore::SqliteMailStore;
        use cosmix_maild_bayesian::{
            ClassifierConfig,
            storage::{AccountConnection, SqliteAccountConnection, StorageBackend},
        };
        use cosmix_mds::{ContainerAttrs, SqliteCasMds};
        use rusqlite::Connection;
        use std::path::Path;
        use std::sync::Mutex;

        // ---- Db (in-memory, schema applied; only needed by `set()`'s
        // signature, not exercised by an update-only patch).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        let tmp_blob = tempfile::tempdir().unwrap();
        let db = Db {
            conn: Arc::new(Mutex::new(conn)),
            blob_dir: tmp_blob.path().to_path_buf(),
        };

        // ---- MDS + mailstore + provision account + 2 mailboxes.
        let mds_dir = tempfile::tempdir().unwrap();
        let mds = Arc::new(SqliteCasMds::open(mds_dir.path()).unwrap());
        let mailstore = Arc::new(SqliteMailStore::new(Arc::clone(&mds)));
        let account_id: i32 = 1;
        let acct_set = mailstore.ensure_account_set(account_id).unwrap();

        let inbox = mds
            .create_container(
                &acct_set,
                None,
                "Inbox",
                ContainerAttrs {
                    special_use: Some("\\Inbox".into()),
                    subscribed: false,
                    extra: serde_json::json!({}),
                },
            )
            .unwrap();
        let archive = mds
            .create_container(
                &acct_set,
                None,
                "Archive",
                ContainerAttrs {
                    special_use: Some("\\Archive".into()),
                    subscribed: false,
                    extra: serde_json::json!({}),
                },
            )
            .unwrap();

        // ---- Materialise an email with memberships in BOTH inbox and
        // archive (the multi-mailbox condition the fix guards). Use the
        // mailstore entry point so the `mail_envelopes` sidecar is
        // written; a bare `mds.add_item` would leave `get_email`
        // failing on the envelope lookup and the test would surface
        // `serverFail` from the patch's first read.
        let payload = b"multi-mailbox keyword guard test";
        let hash = mds.put_blob(payload).unwrap();
        let envelope = EmailEnvelope {
            from: "alice@example.com".into(),
            to: vec!["bob@example.com".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: "multi-mailbox keyword guard test".into(),
            date: 1_700_000_000,
            message_id: Some("<m1@example.com>".into()),
        };
        let (item_id, _uids) = mailstore
            .create_email_with_memberships(
                account_id,
                &[inbox, archive],
                hash,
                envelope,
                &[],
                Flags(0),
                Tags::new(),
                1_730_000_000_000,
            )
            .unwrap();

        // ---- Classifier (untouched by an update-only patch; hermetic).
        let bay_conn =
            Arc::new(SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap());
        struct SingleConn(Arc<SqliteAccountConnection>);
        #[async_trait::async_trait]
        impl StorageBackend for SingleConn {
            async fn open_account(
                &self,
                _account: &AccountId,
            ) -> cosmix_maild_bayesian::Result<Arc<dyn AccountConnection>> {
                Ok(self.0.clone() as Arc<dyn AccountConnection>)
            }
        }
        let backend: Arc<dyn StorageBackend> = Arc::new(SingleConn(bay_conn));
        let classifier = Arc::new(DefaultClassifier::new(ClassifierConfig::default(), backend));

        // ---- Case 1: top-level whole-object `keywords` replacement.
        let id_str = item_id.0.to_string();
        let args = serde_json::json!({
            "update": {
                id_str.clone(): { "keywords": { "$seen": true } }
            }
        });
        let resp = set(&db, &mailstore, account_id, args, &classifier)
            .await
            .unwrap();
        let not_updated = resp
            .get("notUpdated")
            .and_then(|v| v.as_object())
            .expect("notUpdated map present");
        let err = not_updated
            .get(&id_str)
            .expect("rejected id appears in notUpdated");
        assert_eq!(err["type"], "invalidPatch");
        assert_eq!(
            err["description"],
            "keyword updates on multi-mailbox emails are not supported until per-membership JMAP keyword writes preserve IMAP-only flags"
        );
        assert!(
            resp.get("updated")
                .and_then(|v| v.as_object())
                .map(|m| !m.contains_key(&id_str))
                .unwrap_or(true),
            "rejected id must not also appear in updated"
        );

        // ---- Case 2: pointer-path keyword delta (`keywords/$seen: true`).
        let args = serde_json::json!({
            "update": {
                id_str.clone(): { "keywords/$seen": true }
            }
        });
        let resp = set(&db, &mailstore, account_id, args, &classifier)
            .await
            .unwrap();
        let not_updated = resp
            .get("notUpdated")
            .and_then(|v| v.as_object())
            .expect("notUpdated map present");
        let err = not_updated
            .get(&id_str)
            .expect("rejected id appears in notUpdated (pointer-delta path)");
        assert_eq!(err["type"], "invalidPatch");
        assert_eq!(
            err["description"],
            "keyword updates on multi-mailbox emails are not supported until per-membership JMAP keyword writes preserve IMAP-only flags"
        );

        // ---- Verify the email's underlying flags were NOT mutated by
        // either rejected patch. Both memberships should still read
        // Flags(0) (neither $seen nor any other bit set).
        let rec = mailstore.get_email(account_id, item_id).unwrap();
        assert_eq!(
            rec.keywords,
            Flags(0),
            "rejected patches must not mutate flag state"
        );
    }

    #[test]
    fn retrain_stamp_canonicalises_caller_uuid_case() {
        let caller = "A0B1C2D3-E4F5-4678-9ABC-DEF012345678";
        let item = ItemId(uuid::Uuid::parse_str(caller).unwrap());
        assert_eq!(
            canonical_retrain_stamp(item),
            "a0b1c2d3-e4f5-4678-9abc-def012345678"
        );
    }
}
