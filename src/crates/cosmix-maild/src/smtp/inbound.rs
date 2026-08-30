//! Inbound mail delivery — verify, classify, parse, store in JMAP.

use std::net::IpAddr;

use anyhow::Result;
use cosmix_maild_auth::{
    ArcChainValidation, ArcResult, AuthResultsHeader, DkimAggregate, DkimOutcome, DmarcDisposition,
    DmarcOutcome, DmarcPolicy, DmarcReportRecord, DmarcResult, IprevOutcome, IprevResult, SpfCheck,
    SpfResult, Verifier, VerifyResult,
};
use cosmix_maild_bayesian::{classifier::Classifier, types::ClassifyContext};
use cosmix_maild_rules::{
    AccountId, AccountOverrides, RuleContext, RuleEngine, RuleId, RuleVerdict, VerdictShape,
};
use cosmix_mds::{Flags, Mds, Tags};
use mail_parser::{HeaderValue, MessageParser};

use super::SmtpState;
use crate::bus::verdict::{self, VerdictEvent};
use crate::db;
use crate::mailstore::{MailStore, MailboxRole};

/// How the SMTP session authenticated the message's sender.
///
/// Captured from the SMTP **session** (the authenticated submitter, the bind),
/// **never** from message headers (`Received` / `Authentication-Results` are
/// forgeable). Threaded into [`deliver`] so the vtoken sender-lock (a later
/// commit) can require `From == allowed_sender` at the correct strength:
/// `local-auth` proves the mailbox owner (the MSA authenticated them at login),
/// while an external message must fall back to DMARC alignment, which proves the
/// *domain*, not the mailbox.
#[derive(Debug, Clone)]
pub enum IngressAuth {
    /// A local SMTP-AUTH submission: the MSA authenticated this specific mailbox
    /// owner. The strongest sender signal, and for a same-host owner→token post
    /// the message never leaves the server.
    LocalAuth { account_id: i32, email: String },
    /// An unauthenticated message arriving via MX. Any sender identity must come
    /// from DMARC alignment on the `verify_result` (domain-level, not mailbox).
    ExternalUnverified,
}

impl IngressAuth {
    /// The authenticated account id, if this was a local SMTP-AUTH submission.
    /// The submission/queue path keys on this; the sender-lock keys on the email.
    /// Single source of the sender's account identity into [`deliver`].
    pub fn account_id(&self) -> Option<i32> {
        match self {
            IngressAuth::LocalAuth { account_id, .. } => Some(*account_id),
            IngressAuth::ExternalUnverified => None,
        }
    }

    /// True when the SMTP session authenticated the submitter.
    pub fn is_local_auth(&self) -> bool {
        match self {
            IngressAuth::LocalAuth { .. } => true,
            IngressAuth::ExternalUnverified => false,
        }
    }
}

/// Strip CR / LF / NUL from a value before it is interpolated into an
/// RFC 5322 header line, defeating header injection. The vacation
/// auto-reply embeds attacker-influenced values — the envelope sender
/// (`MAIL FROM`), the inbound `Message-ID` (echoed as `In-Reply-To`), and
/// the account owner's `vacation_subject` — into headers; a bare CR/LF in
/// any of them would otherwise inject extra headers (e.g. `Bcc:`) into the
/// reply, turning the auto-responder into a spoofed-mail / BCC-exfil
/// vector. Header values are single-line by RFC 5322, so dropping the
/// line breaks is the correct, lossless-for-legitimate-input fix.
///
/// Shared with `bounce::generate_ndr` (the NDR `To:` is built from the
/// same attacker-influenced envelope sender).
pub(crate) fn header_value_safe(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '\0'))
        .collect()
}

/// Per-message wall-clock budget for the inbound Mix routing filter.
/// Bounds a loop-based runaway (the default recursion cap already
/// prevents stack-overflow); a routing decision is near-instant.
const INBOUND_FILTER_TIME_LIMIT_SECS: u64 = 5;
/// Cap on any single list/map the filter builds.
const INBOUND_FILTER_MAX_COLLECTION_LEN: usize = 100_000;
/// Cap on any single string the filter builds (the returned mailbox name
/// is short; this bounds a pathological allocation).
const INBOUND_FILTER_MAX_STRING_BYTES: usize = 1024 * 1024;

/// Side-information from `classify_inbound` — every field needed to
/// build a `VerdictEvent` once `MailStore::create_email` returns success.
/// Returned alongside the legacy `(verdict_str, score)` so the routing
/// logic in `deliver` is unchanged. Bayesian fields are `None` on
/// hard-accept / hard-junk paths because those short-circuit before
/// the classifier runs.
struct ClassifyDetails {
    rules_verdict: VerdictShape,
    rules_score: f32,
    matched_rules: Vec<RuleId>,
    bayes_score: Option<f32>,
    cold_start: Option<bool>,
}

/// Deliver a received message to the appropriate mailboxes.
///
/// Recipients fall into two classes:
///
/// * **Local** — `db::account::get_by_email` resolves to a row. Inbound
///   path: classify, route, store in MDS via `MailStore::create_email`.
/// * **Remote** — no account row. Only valid on an authenticated
///   submission session (port 465). The bytes land once in the shared
///   CAS and a single `smtp_queue` row covers every remote recipient,
///   matching what JMAP `EmailSubmission/set` does. On port 25 (no
///   auth), RCPT TO already rejects non-local addresses, so an
///   unauthenticated remote recipient at this layer is a defensive
///   no-op — we log and skip rather than open an open relay.
pub async fn deliver(
    state: &SmtpState,
    ingress: &IngressAuth,
    mail_from: &str,
    rcpt_to: &[String],
    data: &[u8],
    remote_ip: IpAddr,
    ehlo_host: &str,
) -> Result<()> {
    // Ingress classification captured from the SMTP session (commit 1 of the
    // sender-lock work); consumed by the vtoken sender-lock in a later commit.
    match ingress {
        IngressAuth::LocalAuth { account_id, email } => {
            tracing::debug!(account_id, email = %email, "smtp deliver: ingress = local-auth submission");
        }
        IngressAuth::ExternalUnverified => {
            tracing::debug!("smtp deliver: ingress = external-unverified (MX)");
        }
    }
    // The authenticated account id (submission/queue path) now derives from the
    // ingress classification — one source of sender identity.
    let sender_account_id = ingress.account_id();
    let hostname = &state.config.hostname;

    // --- Authentication: SPF / DKIM / DMARC / ARC / iprev via cosmix-maild-auth ---
    // A DNS hiccup or per-check timeout must not bounce inbound mail. On any
    // verifier error we synthesize an all-temperror VerifyResult, log a
    // warning, and keep delivering — the rules engine treats temperror as
    // advisory the same way RFC 8601 prescribes.
    let verify_result = match state
        .mail_auth
        .verify(remote_ip, mail_from, ehlo_host, data)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "mail-auth verify failed; continuing with temperror");
            synthesize_temperror_verify(hostname, remote_ip, mail_from, ehlo_host)
        }
    };

    // Prepend the rendered Authentication-Results header to stored message.
    // The verifier already emits the full header line including trailing CRLF.
    let ar_rendered = &verify_result.authentication_results_header.rendered;
    let mut augmented_data = Vec::with_capacity(ar_rendered.len() + data.len());
    augmented_data.extend_from_slice(ar_rendered.as_bytes());
    augmented_data.extend_from_slice(data);

    // --- Parse the message ---
    let parser = MessageParser::default();
    let message = parser
        .parse(&augmented_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse message"))?;

    // Extract headers used by routing (subject feeds the Mix filter)
    // and the verdict event (message_id). The RFC 5322 origination
    // date, from/to/cc address lists, body preview, attachment flag,
    // and message size all flow through `MailStore::create_email` /
    // `mail_envelopes` and the CAS blob — no separate captures.
    let subject = message.subject().map(|s| s.to_string());
    let message_id = message.message_id().map(|s| s.to_string());
    // From: header (display-name + address for every From entry), rendered
    // once for the routing filter. Unlike the SMTP envelope sender, this
    // survives a null `MAIL FROM:<>` — the case for DSN/bounce mail, where
    // `MAILER-DAEMON@...` exists ONLY in the From: header. The filter sees
    // it as `$HEADER_FROM`; both name and address are included so a
    // "Mail Delivery System" display name and a "MAILER-DAEMON@" address
    // are each matchable.
    let header_from = render_header_from(&message);

    // F3 caps inputs (the 20-attachment lesson, email-gateway-vtoken.md §10):
    // surface attachment count + message size to the inbound filter so the
    // vtoken email-to-post branch can refuse an over-large / over-attached
    // submission *above* the publish route. Computed once per message (not per
    // recipient). `$MESSAGE_SIZE` is the received DATA size in bytes (excludes
    // the prepended Authentication-Results header, which is ours, not the
    // sender's). `$ATTACHMENT_COUNT` is `mail_parser`'s attachment count — every
    // non-body MIME part (file attachments AND inline images / unselected
    // alternatives / nested message parts), NOT strictly `Content-Disposition:
    // attachment`. That is deliberately conservative for an anti-bomb cap (it
    // errs toward trashing a pathological multipart), at the cost that a
    // legitimately image-heavy post (>10 inline parts) is trashed — acceptable
    // for the F3 cap; raise the filter's cap if a real post ever trips it.
    let attachment_count = message.attachment_count() as i64;
    let message_size = data.len() as i64;

    // Partition recipients: local accounts deliver into MDS below;
    // remote recipients on an authenticated session get a single
    // shared CAS blob + one `smtp_queue` row *before* local delivery
    // begins. Doing the lookup once per recipient up front avoids
    // re-querying inside the delivery loop. The original envelope
    // address is preserved alongside the resolved account so log
    // lines and Mix filter `$TO` keep matching what the sender
    // wrote (case/normalization unchanged from the pre-fix path).
    let mut local_recipients: Vec<(
        String,
        db::account::Account,
        Option<crate::vtoken::VtokenContext>,
    )> = Vec::new();
    let mut remote_addrs: Vec<String> = Vec::new();
    // vtoken-shaped recipients we deliberately ACCEPT-AND-DISCARD (bad PIN /
    // unknown user / unknown service / registry error / missing content
    // account). Folded into `accepted` below so a message to ONLY such a
    // recipient gets 250 then silently vanishes — never 451, which would
    // invite retries and leak a "not delivered" signal (the no-oracle rule).
    let mut vtoken_silent_drops: usize = 0;
    // The sender-lock proof (the PRIMARY vtoken auth) is per-MESSAGE — about the
    // sender, not the recipient. Local-auth uses the session-authenticated
    // submitter (strong, mailbox-level); an external (MX) message uses its
    // canonical From under DMARC. An ambiguous/absent From → unverified (fails
    // closed). Built ONCE, reused for every recipient.
    let sender_proof = match ingress {
        IngressAuth::LocalAuth { email, .. } => crate::vtoken::SenderProof::local(email.clone()),
        IngressAuth::ExternalUnverified => match canonical_single_from(&message) {
            Some(from) => {
                crate::vtoken::SenderProof::external(from, dmarc_aligned_pass(&verify_result))
            }
            None => crate::vtoken::SenderProof::unverified(),
        },
    };
    for rcpt in rcpt_to {
        // C7: validity-blind rate cap on the token-shaped namespace — shed
        // (silent-drop) BEFORE any resolve/HMAC work so an attacker can't force
        // unbounded per-message work, and WITHOUT leaking validity (the same
        // per-source + global limit applies to valid + invalid token-shaped
        // recipients identically).
        if crate::vtoken::is_active_token_shaped(rcpt, state.config.opaque_rcpt_enabled)
            && !state.token_rate.allow(remote_ip)
        {
            tracing::warn!(
                rcpt = %crate::maillog::sanitize(rcpt),
                "token-shaped recipient over rate cap; accept-and-drop (silent)"
            );
            vtoken_silent_drops += 1;
            continue;
        }
        // vtoken resolution FIRST, and it OWNS a vtoken-shaped local-part:
        // once `resolve` says vtoken-shaped, the outcome never falls through
        // to the alias path (a vtoken-shaped address must never collide with
        // an operator alias). `Valid` → deliver into the content account +
        // carry the service to the filter; `Invalid` → silent accept-drop
        // (no bounce oracle); `Err` (only reachable after a successful
        // shape-parse → the registry read failed) → fail CLOSED, accept-drop;
        // `NotAVtoken` → normal alias/account routing below.
        match state
            .vtoken_store
            .resolve_gated(rcpt, &sender_proof, state.config.opaque_rcpt_enabled)
            .await
        {
            Ok(crate::vtoken::VtokenOutcome::Valid(ctx)) => {
                match db::account::get_by_email(&state.db.conn, &ctx.account).await? {
                    Some(account) => {
                        local_recipients.push((rcpt.clone(), account, Some(ctx)));
                    }
                    None => {
                        // A valid token whose content account doesn't exist is
                        // a server misconfiguration — fail closed (drop) + log
                        // loudly; do NOT alias-route or bounce.
                        tracing::error!(
                            account = %ctx.account,
                            "vtoken resolves to a non-existent content account; accept-and-drop"
                        );
                        vtoken_silent_drops += 1;
                    }
                }
                continue;
            }
            Ok(crate::vtoken::VtokenOutcome::Invalid) => {
                tracing::info!(
                    rcpt = %crate::maillog::sanitize(rcpt),
                    "vtoken auth failed; accept-and-drop (silent, no oracle)"
                );
                vtoken_silent_drops += 1;
                continue;
            }
            Ok(crate::vtoken::VtokenOutcome::NotAVtoken) => { /* normal routing below */ }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    rcpt = %crate::maillog::sanitize(rcpt),
                    "vtoken registry error on a vtoken-shaped address; failing closed (accept-and-drop)"
                );
                vtoken_silent_drops += 1;
                continue;
            }
        }

        // Phase 1 aliases: resolve the recipient through `maild.aliases`
        // before the account lookup so mail to an alias lands in the
        // target's mailbox. The ORIGINAL `rcpt` is preserved in the
        // `local_recipients` tuple (log / Mix-filter `$TO` keep matching
        // what the sender wrote — only the account follows the alias).
        // `?` propagates a transient alias-store error so delivery is
        // retried rather than silently treating the alias as non-local.
        let resolved =
            crate::props::aliases::resolve_recipient(&state.aliases_runtime, rcpt).await?;
        match db::account::get_by_email(&state.db.conn, &resolved).await? {
            Some(account) => local_recipients.push((rcpt.clone(), account, None)),
            None => remote_addrs.push(rcpt.clone()),
        }
    }

    // Envelope-sender authorization for *every* authenticated
    // submission, regardless of recipient locality. RFC 6409 §6.1
    // requires the submission server to verify the submitter is
    // authorized to use the supplied envelope address. Doing this
    // only on the outbound-relay path would let an authenticated
    // user spoof `MAIL FROM:<other-user@local>` for *internal*
    // delivery and land bytes in another local mailbox attributed
    // to a sender they don't own — an internal phishing vector.
    // ASCII-case-insensitive equality on the bare address (display
    // name already stripped by `parse_mail_from`), OR a Phase 1 alias
    // whose target is the authenticated account (`mc@ → admin@` lets
    // `admin@` submit `From: mc@`). This is the §6.1 defense-in-depth
    // layer behind the MAIL FROM check in `session.rs` and MUST get the
    // identical relaxation or it re-rejects an alias sender that already
    // passed there.
    if let Some(sender_id) = sender_account_id {
        let sender_account = db::account::get_by_id(&state.db.conn, sender_id).await?;
        let sender_email = sender_account.as_ref().map(|a| a.email.as_str());
        let authorized = match sender_email {
            Some(e) => {
                crate::props::aliases::sender_authorized(&state.aliases_runtime, mail_from, e).await
            }
            None => false,
        };
        if !authorized {
            tracing::warn!(
                authenticated_as = ?sender_email,
                mail_from = %mail_from,
                "Refusing submission: MAIL FROM does not match authenticated account"
            );
            return Err(anyhow::anyhow!(
                "envelope sender {mail_from:?} not authorized for authenticated account"
            ));
        }
    }

    // Counts a recipient as "accepted" when one of the following
    // durably commits: (a) the outbound queue row covering the
    // remote recipients, or (b) a local `create_email` call. If
    // every path skipped (all locals failed, no remote queue write,
    // unauthenticated session whose remote addresses were dropped),
    // we MUST NOT reply 250 — that would tell the client the
    // message was delivered when nothing was. The caller in
    // `session.rs` maps `Err(_)` to a 451, which is the correct
    // retryable response for "no recipient could be accepted right
    // now" (RFC 5321 §3.6.1).
    // Seed with the vtoken accept-and-discard count: those recipients were
    // intentionally handled (delivered to /dev/null by design), so they
    // count toward "something was accepted" and a sole-invalid-token message
    // returns 250, not a retry-inviting 451.
    let mut accepted: usize = vtoken_silent_drops;

    // --- Outbound: queue remote recipients on authenticated submission ---
    //
    // Envelope-sender authorization happened above (applies to both
    // local and remote paths). What's left here is the relay-policy
    // gate: unauthenticated sessions cannot reach remote recipients
    // (port 25 RCPT TO already rejects non-local), so the `None` arm
    // is a defensive log-and-drop.
    //
    // Bytes queued are the original `data` slice — `augmented_data`
    // carries our inbound `Authentication-Results:` header generated
    // by the *receiving* verifier, which is wrong to ship onward to
    // a downstream MTA (RFC 8601 §5: trace headers reflect the
    // receiving server). `jmap/submission.rs` queues the unmodified
    // submitted blob; we match that shape here.
    //
    // Ordering is load-bearing: this enqueue runs *before* the
    // local-recipient loop so a queue-write failure 451s before any
    // local commit, preventing retry-double-delivery to locals.
    if !remote_addrs.is_empty() {
        match sender_account_id {
            Some(_) => {
                let queue_ms = state.mailstore.clone();
                let queue_data = data.to_vec();
                let put_outcome =
                    tokio::task::spawn_blocking(move || queue_ms.mds().put_blob(&queue_data)).await;
                let blob_hash = match put_outcome {
                    Ok(Ok(h)) => h,
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "Outbound put_blob failed");
                        return Err(e.into());
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Outbound spawn_blocking failed");
                        return Err(anyhow::anyhow!("outbound spawn_blocking: {e}"));
                    }
                };
                let queue_id = crate::smtp::queue::enqueue(
                    &state.db.conn,
                    mail_from,
                    &remote_addrs,
                    blob_hash,
                )
                .await?;
                // Key values inline in the message: the journald sink
                // (`tracing_journald`) emits structured fields as journal
                // FIELDS, invisible in default `journalctl` output.
                tracing::info!(
                    queue_id,
                    from = %mail_from,
                    to = ?remote_addrs,
                    "Submission queued for remote delivery: queue_id={queue_id} from=<{}> nrcpt={}",
                    crate::maillog::sanitize(mail_from),
                    remote_addrs.len(),
                );
                accepted += remote_addrs.len();
            }
            None => {
                // Port 25 reached deliver() with non-local recipients —
                // RCPT TO should have rejected these. Log and drop;
                // local-only delivery still proceeds below.
                tracing::warn!(
                    from = %mail_from,
                    to = ?remote_addrs,
                    "Dropping {} remote recipient(s) on unauthenticated session (would be relay): from=<{}>",
                    remote_addrs.len(),
                    crate::maillog::sanitize(mail_from),
                );
            }
        }
    }

    // --- Deliver to each local recipient ---
    //
    // **Failure semantics: per-recipient best-effort.** A single local
    // recipient's target-resolution or create_email failure logs and
    // `continue`s; the loop tallies successes in `accepted` and only
    // 451s the whole DATA if *no* recipient (local or remote) was
    // accepted. The alternatives are worse:
    //
    //   - Fail-fast (return Err on first local failure) reopens the
    //     round-2 double-delivery MAJOR — the remote queue write at
    //     line ~225 has already committed, so a 451 would prompt the
    //     sender to retry and we'd re-enqueue the remote rows and
    //     re-commit already-landed peer locals.
    //   - Synthesising a DSN bounce on partial failure is a real
    //     feature (substantial work, new bounce-loop surface) and
    //     is out of scope for the Phase 7 silent-drop fix.
    //
    // Logging-and-continue matches the local-delivery branch in
    // `jmap/submission.rs` (Phase 4 Task 4.4), which made the same
    // tradeoff for the same reason. Disposition recorded in the
    // Phase 7 commit and Codex round-5 review.
    for (rcpt, account, vtoken_ctx) in &local_recipients {
        let rcpt = rcpt.as_str();

        // Spam classification: rules → bayesian.
        let (spam_verdict, spam_score, classify_details) = if account.spam_enabled {
            match classify_inbound(
                state,
                account,
                mail_from,
                rcpt_to,
                remote_ip,
                data,
                &verify_result,
                ingress.is_local_auth(),
            )
            .await
            {
                Ok((v, s, details)) => {
                    tracing::info!(
                        to = %rcpt,
                        verdict = %v,
                        score = s,
                        "Spam classification: {v} (score {s:.2}) for <{rcpt}>"
                    );
                    (Some(v), Some(s), Some(details))
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        to = %rcpt,
                        "Spam classification failed for <{rcpt}>: {e}"
                    );
                    (None, None, None)
                }
            }
        } else {
            (None, None, None)
        };

        // Route: try Mix filter script first, then fall back to spam
        // verdict. Resolution goes through the MailStore primitives
        // (`mailbox_by_name` for filter-named targets, `mailbox_by_role`
        // for Inbox/Junk). Per the substrate contract both return
        // `Ok(None)` for unprovisioned accounts and unknown roles —
        // every fallback path here ends at `MailboxRole::Inbox` so an
        // account with no Inbox cannot accept inbound mail (matches
        // legacy `db::mailbox::get_inbox` which errored on the same
        // condition).
        // vtoken context for the filter: a valid token surfaces
        // `$VTOKEN_VALID=true` + the resolved `$VTOKEN_USER`/`$VTOKEN_SERVICE`
        // so the Mix filter can branch (`service=="posts"` → `return "Posts"`).
        let (vt_valid, vt_user, vt_service) = match vtoken_ctx {
            Some(ctx) => (
                true,
                ctx.user_id.as_str(),
                ctx.service.as_deref().unwrap_or(""),
            ),
            None => (false, "", ""),
        };
        let filter_target = run_inbound_filter(
            &state.config,
            mail_from,
            rcpt,
            subject.as_deref().unwrap_or(""),
            &header_from,
            spam_verdict.as_deref().unwrap_or("HAM"),
            spam_score.unwrap_or(0.0),
            vt_valid,
            vt_user,
            vt_service,
            attachment_count,
            message_size,
        )
        .await;

        let inbound_ms = state.mailstore.clone();
        let inbound_account_id = account.id;
        let inbound_filter_target = filter_target.clone();
        let inbound_spam_is_spam = spam_verdict.as_deref() == Some("SPAM");
        // Returns `(mailbox_id, landed_in_named_target)` — the bool is true ONLY
        // when the message landed in exactly the mailbox the filter named (not a
        // fallback). The content marker (below) requires it, so a filter that
        // named `Posts` but fell back to Inbox (Posts missing) never marks the
        // message — closing the "marked-but-misrouted, then moved into Posts"
        // edge.
        let target_outcome = tokio::task::spawn_blocking(move || {
            if let Some(name) = inbound_filter_target {
                if let Some(id) = inbound_ms.mailbox_by_name(inbound_account_id, &name)? {
                    return Ok::<_, anyhow::Error>((id, true));
                }
                tracing::warn!(
                    mailbox = %name,
                    "Inbound filter returned unknown mailbox, falling back to Inbox"
                );
            } else if inbound_spam_is_spam
                && let Some(id) =
                    inbound_ms.mailbox_by_role(inbound_account_id, MailboxRole::Junk)?
            {
                return Ok::<_, anyhow::Error>((id, false));
            }
            inbound_ms
                .mailbox_by_role(inbound_account_id, MailboxRole::Inbox)?
                .map(|id| (id, false))
                .ok_or_else(|| anyhow::anyhow!("recipient {inbound_account_id} has no Inbox"))
        })
        .await;
        // Per-recipient best-effort: a target-resolution failure on
        // one local recipient must not 451 the whole DATA — that
        // would force the remote-queue write to be re-enqueued on
        // retry (and other local recipients that already committed
        // re-delivered). Matches the failure stance in
        // `jmap/submission.rs` local-delivery branch.
        let (target_mailbox, landed_in_named_target) = match target_outcome {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, to = %rcpt, "Local target resolution failed; skipping recipient <{rcpt}>: {e}");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, to = %rcpt, "Local target resolution spawn_blocking failed; skipping recipient <{rcpt}>: {e}");
                continue;
            }
        };

        // Build envelope from the parsed message — same shape Email/set
        // create writes (Phase 3) and the local-recipient delivery
        // branch in submission.rs writes (Phase 4 Task 4.4). `cc` is
        // not carried in `EmailEnvelope` (substrate gap deferred); the
        // raw RFC 5322 Cc: header is preserved in the blob bytes and
        // re-parsed by clients on read.
        let envelope = crate::jmap::email::build_envelope(&message);
        let in_reply_to_vec = crate::jmap::email::extract_in_reply_to(&message);
        // SPAM verdict already routes to the Junk container above —
        // not also expressed as a `$junk` keyword in flags. Per RFC
        // 8621 §4.1.1, JMAP keywords are independent of the mailbox
        // membership; clients display "in Junk" via the role of the
        // containing mailbox, not via a per-message tag. Keeping
        // `initial_keywords` empty matches what `db::email::create`
        // wrote in the legacy path (no keyword column was set on
        // SPAM-routed inbound mail).
        let initial_keywords = Flags(0);
        // Content marker (§0.2 #5): stamped at ingest so the public renderer's
        // live predicate (`member of Posts AND has-marker AND NOT suppressed`)
        // gates on the daemon's own determination, not folder membership alone.
        // Gated on BOTH (a) the vtoken resolving to a content service AND (b) the
        // inbound filter actually routing this message into a content folder
        // (`Posts`/`Pages`). The destination gate is load-bearing: a valid
        // `posts` token whose message the filter sent to `Trash` (spam / null
        // sender / over the F3 attachment/size caps) must NOT be marked — else a
        // later move from Trash into Posts would publish a message the filter
        // explicitly rejected from the publish route. Normal mail and
        // action-service tokens get no tag. (Edge: if the filter names a content
        // folder that fails to resolve, `landed_in_named_target` is false, so the
        // tag is NOT stamped — the message sits in Inbox unmarked and a later
        // move into Posts cannot publish it.)
        let routed_to_content = landed_in_named_target
            && filter_target
                .as_deref()
                .is_some_and(crate::vtoken::is_content_folder_name);
        let initial_tags = match vtoken_ctx {
            Some(ctx)
                if routed_to_content
                    && ctx
                        .service
                        .as_deref()
                        .is_some_and(crate::vtoken::is_content_service) =>
            {
                Tags::from(vec![crate::vtoken::CONTENT_MARKER_TAG.to_string()])
            }
            _ => Tags::new(),
        };
        let received_at_ms = chrono::Utc::now().timestamp_millis();

        // Land bytes in CAS + create email row in one spawn_blocking,
        // matching the local-delivery shape in submission.rs. The
        // returned ItemId is the per-delivery stamp_id for the verdict
        // event below — built only after `create_email` returns Ok so
        // the topic can never publish a row that didn't durably land.
        let create_ms = state.mailstore.clone();
        let create_data = augmented_data.clone();
        let create_account_id = account.id;
        let create_target = target_mailbox;
        let create_outcome = tokio::task::spawn_blocking(move || {
            let blob_hash = create_ms.mds().put_blob(&create_data)?;
            create_ms.create_email(
                create_account_id,
                create_target,
                blob_hash,
                envelope,
                &in_reply_to_vec,
                initial_keywords,
                initial_tags,
                received_at_ms,
            )
        })
        .await;
        // Same best-effort rationale as `target_outcome` above:
        // a single recipient's create_email failure no longer
        // poisons the whole DATA, so already-queued remote rows
        // and already-committed peer locals don't get duplicated
        // on retry.
        let email_uuid = match create_outcome {
            Ok(Ok((item_id, _uid))) => item_id.0,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, to = %rcpt, "Local create_email failed; skipping recipient <{rcpt}>: {e}");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, to = %rcpt, "Local create_email spawn_blocking failed; skipping recipient <{rcpt}>: {e}");
                continue;
            }
        };
        accepted += 1;

        // Destination label for the log. A Mix-filter override names the
        // real destination and takes precedence; only when the filter
        // declined (`None`) does the spam verdict decide Junk vs Inbox.
        // (Previously this ignored `filter_target` and always reported the
        // spam-derived label, so a filtered-to-Trash message logged as
        // "(Junk)" — a genuinely misleading line. A filter naming a
        // mailbox that did NOT resolve falls back to Inbox and is warned
        // separately above, so this is accurate in the common case.)
        let target = match filter_target.as_deref() {
            Some(name) => name,
            None if spam_verdict.as_deref() == Some("SPAM") => "Junk",
            None => "Inbox",
        };
        // Subject is attacker-controlled wire bytes — sanitized before
        // it lands inline in the journal MESSAGE (the structured field
        // copy is journald-encoded and needs no escaping).
        tracing::info!(
            to = %rcpt,
            subject = subject.as_deref().unwrap_or("(none)"),
            target,
            "Delivered inbound message to <{rcpt}> ({target}): subject={}",
            crate::maillog::sanitize(subject.as_deref().unwrap_or("(none)")),
        );

        // Post-commit: publish the verdict event for every durably
        // delivered recipient. Lock-free fan-out; a slow subscriber
        // lags but never holds the DATA path.
        //
        // Unclassified deliveries (`spam_enabled = false`, or a
        // classification error that we logged and continued past)
        // still emit an event with `rules_verdict`/`rules_score`/
        // `bayes_score`/`cold_start` set to JSON null — the topic
        // contract is "one event per durably delivered recipient",
        // and dropping events for those rows would make the stream
        // diverge from what actually landed.
        let (
            rules_verdict_field,
            rules_score_field,
            matched_rules_field,
            bayes_score_field,
            cold_start_field,
        ) = match classify_details {
            Some(d) => (
                Some(d.rules_verdict),
                Some(d.rules_score),
                d.matched_rules,
                d.bayes_score,
                d.cold_start,
            ),
            None => (None, None, Vec::new(), None, None),
        };
        let event = VerdictEvent {
            ts: verdict::now_iso8601(),
            account_id: account.id,
            message_id: message_id.clone(),
            stamp_id: email_uuid.to_string(),
            envelope_from: mail_from.to_string(),
            envelope_to: rcpt_to.to_vec(),
            peer_ip: verdict::render_peer_ip(remote_ip),
            verdict: spam_verdict.clone().unwrap_or_else(|| "HAM".to_string()),
            score: spam_score.unwrap_or(0.0),
            rules_verdict: rules_verdict_field,
            rules_score: rules_score_field,
            matched_rules: matched_rules_field,
            bayes_score: bayes_score_field,
            cold_start: cold_start_field,
            auth_summary: verdict::auth_summary(&verify_result),
        };
        let _ = state.verdict_tx.send(event);

        // Vacation auto-reply (skip for spam, bounces, and Auto-Submitted
        // messages). Anti-backscatter (2026-07 audit): the envelope sender
        // must be SPF/DMARC-verified (`vacation_sender_verified`) — a forged
        // MAIL FROM gets no reply — and each (account, sender) pair is
        // rate-limited to one reply per RFC 3834 window via
        // `mark_reply_if_due` (stamped BEFORE queueing, so a failure path
        // under-sends rather than double-sends).
        if spam_verdict.as_deref() != Some("SPAM")
            && !mail_from.is_empty()
            && !is_auto_submitted(&message)
            && vacation_sender_verified(&verify_result, mail_from)
            && let Ok(Some(vr)) = db::vacation::is_active(&state.db.conn, account.id).await
            && matches!(
                db::vacation::mark_reply_if_due(&state.db.conn, account.id, mail_from).await,
                Ok(true)
            )
        {
            // Fixed fallback subject — the inbound Subject is attacker-
            // controlled and is NOT reflected (the pre-audit `Auto: {subject}`
            // made the responder a content-reflection vector). The account
            // owner's own `vacation_subject` (when set) is their content.
            let vacation_subject = vr
                .subject
                .unwrap_or_else(|| "Auto: Out of Office".to_string());
            let vacation_body = vr.text_body.unwrap_or_else(|| {
                "I am currently out of the office and will respond when I return.".to_string()
            });

            // Vacation reply is *from* the recipient (`rcpt`), so
            // the Message-ID host identifies the recipient's domain
            // as the sender sees us. Phase 3: look up the
            // recipient's `maild.domains` row.
            let recipient_domain =
                crate::smtp::delivery::sender_domain_of(rcpt, &state.config.hostname);
            let message_id_host = crate::smtp::delivery::sender_effective_host(
                &state.domains_runtime,
                &state.config.hostname,
                &recipient_domain,
                crate::smtp::delivery::HostKind::MessageId,
            )
            .await;

            // Sanitise every attacker-influenced value interpolated into a
            // header line (CR/LF stripped) — see `header_value_safe`. The
            // body (`vacation_body`) is legitimately multi-line and sits
            // after the header/body separator, so it is left intact.
            let rcpt_h = header_value_safe(rcpt);
            let mail_from_h = header_value_safe(mail_from);
            let vacation_subject_h = header_value_safe(&vacation_subject);
            let in_reply_to_h = header_value_safe(message_id.as_deref().unwrap_or(""));
            let reply_msg = format!(
                "From: <{rcpt_h}>\r\n\
                     To: <{mail_from_h}>\r\n\
                     Subject: {vacation_subject_h}\r\n\
                     Date: {}\r\n\
                     Message-ID: <vacation-{}@{}>\r\n\
                     In-Reply-To: {in_reply_to_h}\r\n\
                     Auto-Submitted: auto-replied\r\n\
                     MIME-Version: 1.0\r\n\
                     Content-Type: text/plain; charset=utf-8\r\n\
                     \r\n\
                     {vacation_body}",
                chrono::Utc::now().to_rfc2822(),
                uuid::Uuid::new_v4(),
                message_id_host,
            );

            // Queue the auto-reply for outbound delivery. Task 5.4b:
            // bytes go straight into the shared CAS via
            // `mds.put_blob` (idempotent on hash); the legacy
            // `db::blob::store` write is replaced and the resulting
            // `BlobHash` is what the queue addresses by.
            let reply_data = reply_msg.into_bytes();
            let mds = state.mailstore.mds().clone();
            let put_outcome = tokio::task::spawn_blocking(move || mds.put_blob(&reply_data)).await;
            match put_outcome {
                Ok(Ok(blob_hash)) => {
                    let _ = crate::smtp::queue::enqueue(
                        &state.db.conn,
                        rcpt,
                        &[mail_from.to_string()],
                        blob_hash,
                    )
                    .await;
                    tracing::info!(
                        to = %mail_from,
                        "Vacation auto-reply queued to <{}>",
                        crate::maillog::sanitize(mail_from),
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "Vacation auto-reply put_blob failed");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Vacation auto-reply spawn_blocking failed");
                }
            }
        }
    }

    if accepted == 0 {
        // Every recipient path was skipped: unauthenticated remote
        // drops, all-local create_email failures, or a recipient
        // that vanished between RCPT and DATA. Surface this as 451
        // so the client retries, not 250 which would claim
        // durability we never achieved.
        return Err(anyhow::anyhow!(
            "no recipient accepted (from {mail_from:?}, to {rcpt_to:?})"
        ));
    }

    Ok(())
}

/// Run the inbound Mix filter script if configured.
/// Returns Some(mailbox_name) if the filter wants to override routing,
/// or None to use the default spam-based routing.
/// Render the From: header (display-name + address for each entry) into a
/// single space-joined string for the routing filter's `$HEADER_FROM`.
/// Returns an empty string when there is no parseable From header.
fn render_header_from(message: &mail_parser::Message<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push_addr = |a: &mail_parser::Addr<'_>| {
        if let Some(n) = a.name.as_ref() {
            parts.push(n.to_string());
        }
        if let Some(s) = a.address.as_ref() {
            parts.push(s.to_string());
        }
    };
    if let Some(addr) = message.from() {
        match addr {
            mail_parser::Address::List(list) => list.iter().for_each(&mut push_addr),
            mail_parser::Address::Group(groups) => groups
                .iter()
                .flat_map(|g| g.addresses.iter())
                .for_each(&mut push_addr),
        }
    }
    parts.join(" ")
}

/// Reduce a parsed message's `From` to the single canonical mailbox the vtoken
/// sender-lock compares against, or `None` if the header carries zero or MORE
/// THAN ONE mailbox (ambiguous → the caller silent-drops, closing the
/// multi-`From` spoofing gap). Display names, comments, and groups are unwrapped
/// by the parser; case / IDN folding by [`crate::vtoken::canonical_addr`].
///
/// Used by the vtoken sender-lock (C4) to derive the external-path sender
/// identity (the local-auth path uses the session-authenticated submitter).
fn canonical_single_from(message: &mail_parser::Message<'_>) -> Option<String> {
    let mut addrs: Vec<String> = Vec::new();
    let mut push = |a: &mail_parser::Addr<'_>| {
        if let Some(s) = a.address.as_ref() {
            addrs.push(s.to_string());
        }
    };
    if let Some(from) = message.from() {
        match from {
            mail_parser::Address::List(list) => list.iter().for_each(&mut push),
            mail_parser::Address::Group(groups) => groups
                .iter()
                .flat_map(|g| g.addresses.iter())
                .for_each(&mut push),
        }
    }
    // Exactly one mailbox, or it is ambiguous (zero / many).
    if addrs.len() != 1 {
        return None;
    }
    crate::vtoken::canonical_addr(&addrs[0])
}

/// Whether DMARC PASSED for this message — an SPF-or-DKIM pass that is also
/// ALIGNED with the RFC5322.From domain (mail-auth's `DmarcOutcome::Pass`). NOT a
/// bare SPF or DKIM pass: the vtoken sender-lock's external path requires the
/// `From` domain itself be authenticated, which only DMARC alignment provides.
fn dmarc_aligned_pass(verify_result: &VerifyResult) -> bool {
    matches!(verify_result.dmarc.outcome, DmarcOutcome::Pass)
}

/// Whether the ENVELOPE sender is authenticated enough to auto-reply to
/// (the vacation backscatter gate — 2026-07 audit). The reply is
/// addressed to MAIL FROM, so it is the ENVELOPE sender that must be
/// authenticated:
/// - SPF pass on the MAIL FROM identity — the reply target's own domain
///   authorized the sending host — is directly sufficient.
/// - A DMARC-aligned pass authenticates only the header-From org domain,
///   so it counts ONLY when the envelope sender's domain is itself
///   aligned (relaxed: equal to or a subdomain of that org domain).
///   Without that check an attacker could DMARC-pass their own From
///   domain while forging a victim MAIL FROM and still farm backscatter
///   (Codex review catch).
///
/// A forged MAIL FROM fails both arms. Senders with no SPF and no DMARC
/// get no auto-reply — the RFC 3834 trade: silence over backscatter.
fn vacation_sender_verified(verify_result: &VerifyResult, mail_from: &str) -> bool {
    if matches!(
        &verify_result.spf,
        SpfCheck::MailFrom {
            result: SpfResult::Pass,
            ..
        }
    ) {
        return true;
    }
    if dmarc_aligned_pass(verify_result) {
        let env_domain = mail_from
            .rsplit('@')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let org = verify_result
            .dmarc
            .report_record
            .org_domain
            .to_ascii_lowercase();
        return !org.is_empty() && (env_domain == org || env_domain.ends_with(&format!(".{org}")));
    }
    false
}

#[allow(clippy::too_many_arguments)]
async fn run_inbound_filter(
    config: &super::SmtpConfig,
    from: &str,
    to: &str,
    subject: &str,
    header_from: &str,
    spam_verdict: &str,
    spam_score: f64,
    vtoken_valid: bool,
    vtoken_user: &str,
    vtoken_service: &str,
    attachment_count: i64,
    message_size: i64,
) -> Option<String> {
    let filter_path = config.inbound_filter.as_deref()?;

    let source = match std::fs::read_to_string(filter_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %filter_path, error = %e, "Failed to read inbound filter script");
            return None;
        }
    };

    // Parse + execute on blocking thread (Mix evaluator is !Send)
    let from = from.to_string();
    let to = to.to_string();
    let subject = subject.to_string();
    let header_from = header_from.to_string();
    let spam_verdict = spam_verdict.to_string();
    let vtoken_user = vtoken_user.to_string();
    let vtoken_service = vtoken_service.to_string();

    match tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut lexer = cosmix_mix::lexer::Lexer::new(&source);
            let tokens = lexer.tokenize().map_err(|e| format!("{e}"))?;
            let mut parser = cosmix_mix::parser::Parser::new(tokens, &source);
            let stmts = parser.parse_program().map_err(|e| format!("{e}"))?;

            let stdout = cosmix_mix::evaluator::SharedBuf::new();
            let stderr = cosmix_mix::evaluator::SharedBuf::new();
            let mut eval = cosmix_mix::evaluator::Evaluator::with_output(
                Box::new(stdout.clone()),
                Box::new(stderr.clone()),
            );

            // Self-protection knobs (mirrors the webd embed in
            // cosmix-webd/src/mix_handler.rs). The filter is operator-
            // authored/trusted, but it runs on the mail-delivery hot path
            // with attacker-influenced inputs (FROM/SUBJECT), so a buggy
            // or pathological filter must not be able to wedge a blocking
            // thread, balloon memory, or (without the gate) shell out.
            // The default recursion cap (16) already prevents a native-
            // stack-overflow SIGSEGV; add a wall-clock deadline +
            // collection caps to bound loops/allocations.
            eval.set_limits(cosmix_mix::EvalLimits {
                recursion_limit: cosmix_mix::DEFAULT_RECURSION_LIMIT,
                time_limit: Some(std::time::Duration::from_secs(
                    INBOUND_FILTER_TIME_LIMIT_SECS,
                )),
                max_list_len: Some(INBOUND_FILTER_MAX_COLLECTION_LEN),
                max_map_len: Some(INBOUND_FILTER_MAX_COLLECTION_LEN),
                max_string_len: Some(INBOUND_FILTER_MAX_STRING_BYTES),
            });
            // A routing filter inspects metadata and returns a mailbox
            // name; it legitimately may read a file (e.g. a denylist), so
            // allow Pure + FsRead and deny FsWrite/Network/Process/Env
            // (and the shell syntax they gate). No Bus handler installed →
            // send/emit inert.
            eval.set_capability_policy(std::rc::Rc::new(cosmix_mix::CategoryAllowList::new(&[
                cosmix_mix::CapabilityClass::FsRead,
            ])));

            // Inject email metadata as Mix globals
            eval.set_global("FROM", cosmix_mix::value::Value::String(from));
            eval.set_global("TO", cosmix_mix::value::Value::String(to));
            eval.set_global("SUBJECT", cosmix_mix::value::Value::String(subject));
            eval.set_global("HEADER_FROM", cosmix_mix::value::Value::String(header_from));
            eval.set_global(
                "SPAM_VERDICT",
                cosmix_mix::value::Value::String(spam_verdict),
            );
            eval.set_global("SPAM_SCORE", cosmix_mix::value::Value::Number(spam_score));

            // vtoken context (Rust did the parse/lookup/PIN-validation; the
            // filter only reads the verdict — it never touches the registry).
            eval.set_global("VTOKEN_VALID", cosmix_mix::value::Value::Bool(vtoken_valid));
            eval.set_global("VTOKEN_USER", cosmix_mix::value::Value::String(vtoken_user));
            eval.set_global(
                "VTOKEN_SERVICE",
                cosmix_mix::value::Value::String(vtoken_service),
            );

            // F3 caps inputs: attachment count + received message size (bytes).
            // Whole-number i64 → an exact JSON-integer-clean Mix Number.
            eval.set_global(
                "ATTACHMENT_COUNT",
                cosmix_mix::value::Value::Number(attachment_count as f64),
            );
            eval.set_global(
                "MESSAGE_SIZE",
                cosmix_mix::value::Value::Number(message_size as f64),
            );

            match eval.execute(&stmts).await {
                Ok(mut val) => {
                    // Check return value first, then stdout. `Value`
                    // implements `Drop` (mix ≥ 0.16.3, stack-safe nested
                    // drop), so the inner String can't be moved out by
                    // value — take it from a `&mut` binding instead.
                    match &mut val {
                        cosmix_mix::value::Value::String(s) if !s.is_empty() => {
                            Ok(Some(std::mem::take(s)))
                        }
                        _ => {
                            let out = stdout.to_string_lossy().trim().to_string();
                            if out.is_empty() {
                                Ok(None)
                            } else {
                                Ok(Some(out))
                            }
                        }
                    }
                }
                Err(e) => Err(format!("{e}")),
            }
        })
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            tracing::warn!(path = %filter_path, error = %e, "Inbound filter script failed");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "Inbound filter task panicked");
            None
        }
    }
}

/// Run rules → bayesian against one recipient. Returns
/// `(verdict_str, score)` where `verdict_str` is `"SPAM"` or `"HAM"` —
/// the values the existing JMAP schema and Mix filter pipeline expect.
#[allow(clippy::too_many_arguments)]
async fn classify_inbound(
    state: &SmtpState,
    account: &db::account::Account,
    mail_from: &str,
    rcpt_to: &[String],
    peer_ip: IpAddr,
    data: &[u8],
    verify_result: &cosmix_maild_auth::VerifyResult,
    sender_authenticated: bool,
) -> anyhow::Result<(String, f64, ClassifyDetails)> {
    if sender_authenticated {
        tracing::debug!(
            "authenticated submission: mail-auth hard-fail rules disabled for this message (other rules and bayes still run)"
        );
    }

    // Stringify the existing i32 account id to match the bayesian
    // backend's per-account on-disk layout (`<base>/<id>/...`).
    let account_id = AccountId::new(account.id.to_string());
    // SPEC 12 Phase 2 — resolve per-account overrides from the
    // property substrate. Absence / tombstone returns
    // `AccountOverrides::default()` inside the helper (preserves the
    // pre-C2 behaviour); any other storage error is logged and the
    // delivery path falls back to defaults so a substrate hiccup
    // never blocks inbound mail.
    let overrides = crate::props::account_overrides::read_account_overrides_by_email(
        &state.overrides_runtime,
        &account.email,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(
            target: "cosmix_maild::smtp::inbound",
            error = %e, email = %account.email,
            "account_overrides read failed; defaulting to empty"
        );
        AccountOverrides::default()
    });
    let rule_ctx = RuleContext {
        peer_ip,
        envelope_from: mail_from,
        envelope_to: rcpt_to,
        message: data,
        mail_auth: verify_result,
        sender_authenticated,
        account: &account_id,
        overrides: &overrides,
    };

    let rule_verdict = state
        .rule_engine
        .classify(&rule_ctx)
        .await
        .map_err(|e| anyhow::anyhow!("rules classify: {e}"))?;
    state.rule_stats.record(&rule_verdict);

    let (rules_score, matched_rules) = match &rule_verdict {
        RuleVerdict::HardAccept { matched_rules, .. } => {
            let details = ClassifyDetails {
                rules_verdict: VerdictShape::HardAccept,
                rules_score: 0.0,
                matched_rules: matched_rules.clone(),
                bayes_score: None,
                cold_start: None,
            };
            return Ok(("HAM".to_string(), 0.0, details));
        }
        RuleVerdict::HardJunk {
            score,
            matched_rules,
            ..
        } => {
            let details = ClassifyDetails {
                rules_verdict: VerdictShape::HardJunk,
                rules_score: *score,
                matched_rules: matched_rules.clone(),
                bayes_score: None,
                cold_start: None,
            };
            return Ok(("SPAM".to_string(), *score as f64, details));
        }
        RuleVerdict::Continue {
            score,
            matched_rules,
            ..
        } => (*score, matched_rules.clone()),
    };

    let classify_ctx = ClassifyContext {
        message: data,
        account: &account_id,
        rules_score,
        matched_rules: &matched_rules,
        trusted: false,
    };
    let bayes_verdict = state
        .classifier
        .classify(&classify_ctx)
        .await
        .map_err(|e| anyhow::anyhow!("bayes classify: {e}"))?;

    // The per-account threshold is the authority for Bayesian-path labels —
    // EXCEPT while the account is still cold. The classifier's own label is
    // otherwise ignored: its threshold is the engine default (0.95) and would
    // override a stricter per-account setting.
    //
    // Cold-start is the exception, and it is not cosmetic. An untrained corpus
    // returns the UNINFORMATIVE score 0.5 — "I have no evidence". The default
    // account threshold is also 0.5, and the test is `>=`, so 0.5 >= 0.5 labels
    // it SPAM: a brand-new account junks 100% of its mail, including the very
    // messages it needs in order to train out of cold-start. Found by inter-node
    // smoke testing on the WG mesh (2026-07-14): three of four cold nodes junked
    // every message at exactly score 0.50, while gamma — the one node with a
    // corpus — scored a real 0.38 and delivered to Inbox.
    //
    // The classifier already computes the guard for this (`cold_start` plus a
    // lenient threshold, 0.85); this path simply discarded it. Honour it: while
    // cold, take the MORE LENIENT of the two thresholds, so no-evidence lands in
    // the Inbox. Once trained, the per-account threshold resumes sole authority,
    // so a user who deliberately sets an aggressive threshold still gets it.
    let score_f64 = bayes_verdict.score as f64;
    let effective_threshold = if bayes_verdict.cold_start {
        account.spam_threshold.max(bayes_verdict.threshold as f64)
    } else {
        account.spam_threshold
    };
    let label = if score_f64 >= effective_threshold {
        "SPAM"
    } else {
        "HAM"
    };
    let details = ClassifyDetails {
        rules_verdict: VerdictShape::Continue,
        rules_score,
        matched_rules,
        bayes_score: Some(bayes_verdict.score),
        cold_start: Some(bayes_verdict.cold_start),
    };
    Ok((label.to_string(), score_f64, details))
}

/// Build an all-temperror `VerifyResult` so a DNS hiccup or per-check
/// timeout in `MailAuthVerifier` does not bounce inbound mail. The
/// rendered header lists each check as `temperror` per RFC 8601.
///
/// Also used by the Bus `maild.rules.explain` action to synthesize the
/// `mail_auth: null` case — the spec says no DNS lookup is performed
/// for explain, so the synthesized all-temperror result stands in for
/// the normal verifier output.
pub(crate) fn synthesize_temperror_verify(
    host_identity: &str,
    peer_ip: IpAddr,
    mail_from: &str,
    helo: &str,
) -> VerifyResult {
    let identity_kind = if mail_from.is_empty() {
        ("smtp.helo", helo.to_string())
    } else {
        ("smtp.mailfrom", mail_from.to_string())
    };
    let from_domain = mail_from
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or(helo)
        .to_string();
    // `identity_kind.1` (helo / mail_from) and `from_domain` are
    // attacker-influenced; strip CR/LF before they land in this
    // multi-line header or they inject extra Authentication-Results
    // fields a downstream consumer would trust. (Source values feeding
    // the VerifyResult below are left intact — only the rendered header
    // is sanitised.)
    let rendered = format!(
        "Authentication-Results: {host_identity};\r\n\
         \tiprev=temperror policy.iprev={peer_ip};\r\n\
         \tspf=temperror {}={};\r\n\
         \tdkim=temperror;\r\n\
         \tdmarc=temperror header.from={};\r\n\
         \tarc=temperror smtp.remote-ip={peer_ip}\r\n",
        identity_kind.0,
        header_value_safe(&identity_kind.1),
        header_value_safe(&from_domain),
    );
    VerifyResult {
        spf: if mail_from.is_empty() {
            SpfCheck::Helo {
                result: SpfResult::TempError,
                domain: helo.to_string(),
            }
        } else {
            SpfCheck::MailFrom {
                result: SpfResult::TempError,
                domain: from_domain.clone(),
            }
        },
        iprev: IprevResult {
            result: IprevOutcome::TempError,
            ptr: None,
            matched_forward: None,
        },
        dkim: DkimAggregate {
            signatures: Vec::new(),
            overall: DkimOutcome::TempError,
            capped: false,
        },
        dmarc: DmarcResult {
            outcome: DmarcOutcome::None,
            report_record: DmarcReportRecord {
                org_domain: from_domain,
                source_ip: peer_ip,
                policy_published: DmarcPolicy::None,
                policy_evaluated: DmarcPolicy::None,
                spf_aligned: false,
                dkim_aligned: false,
                disposition: DmarcDisposition::None,
                count: 1,
            },
        },
        arc: ArcResult {
            chain_validation: ArcChainValidation::None,
            instance_count: 0,
            oldest_pass_chain: false,
        },
        authentication_results_header: AuthResultsHeader {
            host_identity: host_identity.to_string(),
            rendered,
            spf: None,
            iprev: None,
            dkim: Vec::new(),
            dmarc: None,
            arc: None,
        },
    }
}

/// Check if a message has Auto-Submitted header (prevents vacation reply loops).
fn is_auto_submitted(message: &mail_parser::Message<'_>) -> bool {
    // Check for Auto-Submitted header (RFC 3834)
    for header in message.headers() {
        if header.name() == "Auto-Submitted"
            && let HeaderValue::Text(val) = header.value()
            && val.as_ref() != "no"
        {
            return true;
        }
        // Also check Precedence: bulk/list/junk
        if header.name() == "Precedence"
            && let HeaderValue::Text(val) = header.value()
        {
            let v = val.to_lowercase();
            if v == "bulk" || v == "list" || v == "junk" {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{canonical_single_from, header_value_safe};

    fn from_header(raw: &str) -> Option<String> {
        let msg = format!("From: {raw}\r\nTo: x@y.com\r\nSubject: t\r\n\r\nbody\r\n");
        let parsed = mail_parser::MessageParser::default()
            .parse(msg.as_bytes())
            .expect("parse");
        canonical_single_from(&parsed)
    }

    #[test]
    fn canonical_single_from_extracts_one_mailbox() {
        assert_eq!(from_header("alice@d.com").as_deref(), Some("alice@d.com"));
        // Display name + angle brackets + mixed case → stripped + folded.
        assert_eq!(
            from_header("\"Alice Example\" <Alice@D.COM>").as_deref(),
            Some("alice@d.com")
        );
        // RFC 5322 comment unwrapped by the parser.
        assert_eq!(from_header("bob@d.com (Bob)").as_deref(), Some("bob@d.com"));
        // A single-member group resolves to its one mailbox.
        assert_eq!(
            from_header("Team: solo@d.com;").as_deref(),
            Some("solo@d.com")
        );
    }

    #[test]
    fn canonical_single_from_rejects_ambiguous() {
        // More than one mailbox → ambiguous → None (sender-lock fails closed).
        assert_eq!(from_header("a@d.com, b@d.com"), None);
        assert_eq!(from_header("Team: a@d.com, b@d.com;"), None);
        // Display-name-only / empty angle / garbage → None.
        assert_eq!(from_header("\"No Address\" <>"), None);
        assert_eq!(from_header("not-an-email"), None);
    }

    #[test]
    fn header_value_safe_strips_crlf_injection() {
        // A CRLF-injection payload in MAIL FROM must not be able to add a
        // header line to the vacation auto-reply.
        let evil = "attacker@evil.test>\r\nBcc: victim-list@evil.test";
        let safe = header_value_safe(evil);
        assert!(!safe.contains('\r') && !safe.contains('\n'));
        assert!(!safe.contains("Bcc:") || !safe.contains('\n'));
        // Folding/control chars gone; the visible text is preserved on one line.
        assert_eq!(safe, "attacker@evil.test>Bcc: victim-list@evil.test");
        // NUL is also dropped.
        assert_eq!(header_value_safe("a\0b"), "ab");
        // Legitimate single-line values pass through unchanged.
        assert_eq!(header_value_safe("Out of office"), "Out of office");
    }
}
