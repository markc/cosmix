//! Per-connection IMAP session driver.
//!
//! Phase 1 dispatch: CAPABILITY / NOOP / LOGOUT / ID / LOGIN /
//! AUTHENTICATE plus a hard `BAD` for the literal indicator and a
//! `BAD command not implemented` for everything else.
//!
//! Authentication wiring goes through `crate::auth::basic::verify`
//! (the same path SMTP submission uses), so bcrypt cost and the
//! accounts table layout stay in lockstep across protocols.
//! Per-account concurrency caps are enforced via [`AccountSlots`]
//! handed in by the server module: the count is decremented after
//! `Session::run` returns (both `Ok` and propagated `Err`). A panic
//! inside `run` is *not* caught here, so a slot leak is possible
//! against a panicking handler. Phase 1 has no `unwrap`/`expect` on
//! the hot path and every awaited I/O returns `Result`, so the
//! panic surface is narrow; if Phase 2+ widens it, switch the
//! release to an async-drop guard rather than rely on this invariant.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::auth::basic;
use crate::db::Db;
use crate::imap::codec::{self, ReadCommandError, ReadLineError};
use crate::imap::config::ImapConfig;
use crate::imap::op;
use crate::imap::response::{Status, continuation, tagged, untagged_bye, untagged_ok_with_code};
use crate::imap::sasl::{self, Mechanism};
use crate::imap::state::State;
use crate::mailstore::MailStore;

/// Per-account concurrent-connection counter. Wrapped in `Arc` so
/// every accepted connection shares one map.
#[derive(Default)]
pub struct AccountSlots {
    inner: Mutex<HashMap<i32, u32>>,
}

impl AccountSlots {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Try to reserve a slot for `account_id`. Returns `true` on
    /// success; `false` means the cap is reached.
    pub async fn try_acquire(&self, account_id: i32, cap: u32) -> bool {
        let mut g = self.inner.lock().await;
        let entry = g.entry(account_id).or_insert(0);
        if *entry >= cap {
            return false;
        }
        *entry += 1;
        true
    }

    pub async fn release(&self, account_id: i32) {
        let mut g = self.inner.lock().await;
        if let Some(v) = g.get_mut(&account_id) {
            *v = v.saturating_sub(1);
            if *v == 0 {
                g.remove(&account_id);
            }
        }
    }

    /// Snapshot the live per-account connection counts as
    /// `(account_id, count)` pairs. Backs `maild.stats.online` (the
    /// doveadm `who` analog) — a point-in-time read of who currently
    /// holds an open IMAP session. Only accounts with ≥1 live
    /// connection appear: `release` removes an entry once its count
    /// hits zero, so the map never carries zero-valued rows. The lock
    /// is held only for the clone, so a snapshot never blocks an
    /// acquire/release for longer than the copy.
    pub async fn snapshot(&self) -> Vec<(i32, u32)> {
        let g = self.inner.lock().await;
        g.iter().map(|(&id, &n)| (id, n)).collect()
    }
}

/// Drives one accepted IMAP connection through its lifetime.
///
/// `stream` already has TLS terminated. `peer` is the originating
/// socket address for tracing. `negotiated_hostname` is the SNI-
/// resolved identity name the listener chose for this connection (or
/// the no-SNI fallback identity / `ImapConfig.hostname` if SNI didn't
/// match anything) — woven into the `* OK` greeting text so a
/// multi-identity deployment lets the client read back which name it
/// negotiated.
// Connection handler: each arg is a distinct injected dependency
// (stream, peer/local sockets, config, db, store, slots, SNI name).
#[allow(clippy::too_many_arguments)]
pub async fn handle_connection<S>(
    stream: S,
    peer: SocketAddr,
    lip: Option<SocketAddr>,
    cfg: Arc<ImapConfig>,
    db: Db,
    mailstore: Arc<dyn MailStore>,
    slots: Arc<AccountSlots>,
    negotiated_hostname: String,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    crate::maillog::imap_connect(peer, lip);
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::with_capacity(8192, read_half);
    let mut writer = write_half;

    // Every fallible step (greeting write *and* the command loop) is
    // captured into a single `res` so `handle_connection` emits exactly
    // one `imap-login: Disconnected` line per connection, regardless of
    // where the error originated. The caller (`imap/server.rs`) must
    // NOT log a second disconnect on the returned `Err` — this function
    // owns disconnect logging end-to-end.
    let res: Result<()> = async {
        // Greeting: unsolicited CAPABILITY embedded in the OK banner so
        // a single round-trip leaves the client knowing what to ask
        // for. The hostname is the resolver's pick for the negotiated
        // SNI — Cyrus-style `<host> IMAP ready` so the client sees the
        // identity the cert was issued under, not just a generic
        // banner.
        let caps_body = match cfg.advertise_capabilities.as_ref() {
            Some(o) => crate::imap::capabilities::render_owned(o),
            None => crate::imap::capabilities::render(crate::imap::capabilities::INITIAL),
        };
        let code = format!("CAPABILITY {caps_body}");
        let greeting_text = format!("{negotiated_hostname} cosmix-maild IMAP ready");
        let greeting = untagged_ok_with_code(&code, &greeting_text);
        writer.write_all(greeting.as_bytes()).await?;
        writer.flush().await?;

        let mut session = Session {
            state: State::NotAuthenticated,
            cfg: cfg.clone(),
            db,
            mailstore,
            auth_failures: 0,
            bad_commands: 0,
            slots: slots.clone(),
            account_id: None,
            peer,
            local: lip,
        };

        let r = session.run(&mut reader, &mut writer).await;

        // Release the slot on normal return and on `Err` propagation
        // from `run` (the `?` paths in the loop body still pass through
        // here because `run` is a plain `.await` rather than a
        // `try_join`). A panic inside `run` will *not* execute this
        // line — there is no async-Drop guard on `account_id`. Phase 1
        // wraps every awaited I/O in `Result`, so the panic surface is
        // narrow (only the tracing macros and the SASL/codec parsers,
        // all of which use `Result` rather than unwraps). If a panic
        // ever fires here the slot leak is bounded by the per-account
        // cap and the operator restart cycle.
        if let Some(aid) = session.account_id {
            slots.release(aid).await;
        }
        r
    }
    .await;

    match &res {
        Ok(()) => crate::maillog::imap_disconnect(peer, lip, "clean"),
        Err(e) => crate::maillog::imap_disconnect(peer, lip, &e.to_string()),
    }

    res
}

/// Result of a single `read_command` call.
enum ReadOutcome {
    /// A clean command frame — caller dispatches on it.
    Frame(codec::Command),
    /// Peer closed cleanly (or pre-auth timeout fired). Caller exits.
    Eof,
    /// The codec resolved a literal-framing problem while keeping the
    /// stream in sync. `response` is the pre-formatted tagged BAD/NO
    /// that the caller writes verbatim before reading the next
    /// command. The bad-command counter has already been bumped by
    /// `read_command`.
    LiteralRejected { response: String },
}

struct Session {
    state: State,
    cfg: Arc<ImapConfig>,
    db: Db,
    mailstore: Arc<dyn MailStore>,
    auth_failures: u32,
    bad_commands: u32,
    slots: Arc<AccountSlots>,
    account_id: Option<i32>,
    peer: SocketAddr,
    /// Local (bound) socket the client connected to, for the
    /// dovecot-style `lip=` field. `None` if `local_addr()` failed.
    local: Option<SocketAddr>,
}

impl Session {
    async fn run<R, W>(&mut self, reader: &mut BufReader<R>, writer: &mut W) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        loop {
            if self.state.is_logout() {
                return Ok(());
            }
            let cmd = match self.read_command(reader, writer).await? {
                ReadOutcome::Frame(c) => c,
                ReadOutcome::Eof => return Ok(()),
                ReadOutcome::LiteralRejected { response } => {
                    writer.write_all(response.as_bytes()).await?;
                    writer.flush().await?;
                    if self.over_bad_command_cap() {
                        self.emit_bye(writer, "too many bad commands").await?;
                        return Ok(());
                    }
                    continue;
                }
            };

            let parsed = match codec::parse_line(&cmd.line) {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    self.bad_commands += 1;
                    let tag = sniff_tag(&cmd.line).unwrap_or("*".to_string());
                    let resp = tagged(&tag, Status::Bad, None, &msg);
                    writer.write_all(resp.as_bytes()).await?;
                    writer.flush().await?;
                    if self.over_bad_command_cap() {
                        self.emit_bye(writer, "too many bad commands").await?;
                        return Ok(());
                    }
                    continue;
                }
            };

            // Literal-framing arrived for a verb that does not accept
            // one. The codec already consumed the body, so the stream
            // is in sync; we just refuse the command.
            if cmd.literal.is_some() && !parsed.verb_upper.eq_ignore_ascii_case("APPEND") {
                self.bad_commands += 1;
                let resp = tagged(
                    &parsed.tag,
                    Status::Bad,
                    None,
                    &format!(
                        "{} does not accept literal arguments in this phase",
                        parsed.verb_upper
                    ),
                );
                writer.write_all(resp.as_bytes()).await?;
                writer.flush().await?;
                if self.over_bad_command_cap() {
                    self.emit_bye(writer, "too many bad commands").await?;
                    return Ok(());
                }
                continue;
            }

            tracing::debug!(
                peer = %self.peer,
                tag = %parsed.tag,
                verb = %parsed.verb_upper,
                state = ?self.state,
                "imap command"
            );

            match parsed.verb_upper.as_str() {
                "CAPABILITY" => {
                    writer
                        .write_all(op::capability::handle(&parsed.tag, &self.cfg).as_bytes())
                        .await?;
                }
                "NOOP" => {
                    writer
                        .write_all(op::noop::handle(&parsed.tag).as_bytes())
                        .await?;
                }
                "LOGOUT" => {
                    writer
                        .write_all(op::logout::handle(&parsed.tag).as_bytes())
                        .await?;
                    writer.flush().await?;
                    self.state = State::Logout;
                    return Ok(());
                }
                "ID" => {
                    writer
                        .write_all(op::id::handle(&parsed.tag, &parsed.args).as_bytes())
                        .await?;
                }
                "LOGIN" => {
                    self.handle_login(&parsed.tag, &parsed.args, writer).await?;
                }
                "AUTHENTICATE" => {
                    self.handle_authenticate(&parsed.tag, &parsed.args, reader, writer)
                        .await?;
                }
                "NAMESPACE" => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        writer
                            .write_all(op::namespace::handle(&parsed.tag).as_bytes())
                            .await?;
                    }
                }
                "LIST" => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        // `is_authenticated()` already guarantees `account_id`
                        // was set during LOGIN/AUTHENTICATE — both arms in
                        // `handle_login` / `handle_authenticate` assign
                        // `account_id` *before* moving the state. The
                        // expect is a tripwire if that invariant ever
                        // breaks; it is not a defensible boundary check.
                        let aid = self
                            .account_id
                            .expect("authenticated session must have account_id");
                        let resp = op::list::handle(
                            &parsed.tag,
                            &parsed.args,
                            aid,
                            self.mailstore.clone(),
                        )
                        .await;
                        writer.write_all(resp.as_bytes()).await?;
                    }
                }
                "LSUB" => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        let aid = self
                            .account_id
                            .expect("authenticated session must have account_id");
                        let resp = op::lsub::handle(
                            &parsed.tag,
                            &parsed.args,
                            aid,
                            self.mailstore.clone(),
                        )
                        .await;
                        writer.write_all(resp.as_bytes()).await?;
                    }
                }
                verb @ ("SELECT" | "EXAMINE") => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        // RFC 9051 §6.3.2 — every SELECT/EXAMINE,
                        // *including* a failing one, implicitly
                        // closes the previously selected mailbox.
                        // Drop to Authenticated *before* the lookup
                        // so a failed reselect leaves the session
                        // in a no-selection state, not stuck on the
                        // old mailbox.
                        self.state = State::Authenticated;
                        let aid = self
                            .account_id
                            .expect("authenticated session must have account_id");
                        let read_only = verb == "EXAMINE";
                        let outcome = op::select::handle(
                            &parsed.tag,
                            &parsed.args,
                            aid,
                            self.mailstore.clone(),
                            read_only,
                        )
                        .await;
                        writer.write_all(outcome.response.as_bytes()).await?;
                        if let Some(sm) = outcome.selected {
                            self.state = State::Selected(sm);
                        }
                    }
                }
                "STATUS" => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        let aid = self
                            .account_id
                            .expect("authenticated session must have account_id");
                        let resp = op::status::handle(
                            &parsed.tag,
                            &parsed.args,
                            aid,
                            self.mailstore.clone(),
                        )
                        .await;
                        writer.write_all(resp.as_bytes()).await?;
                    }
                }
                "APPEND" => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        // The codec already consumed the literal body
                        // (or rejected it with a stream-preserving
                        // BAD/NO before we got here). Missing literal
                        // means the client framed APPEND without a
                        // trailing `{N}` marker — RFC 9051 §6.3.12
                        // mandates a literal, so this is a BAD.
                        let body = match cmd.literal.clone() {
                            Some(b) => b,
                            None => {
                                self.bad_commands += 1;
                                let resp = tagged(
                                    &parsed.tag,
                                    Status::Bad,
                                    None,
                                    "APPEND requires a literal message body",
                                );
                                writer.write_all(resp.as_bytes()).await?;
                                writer.flush().await?;
                                if self.over_bad_command_cap() {
                                    self.emit_bye(writer, "too many bad commands").await?;
                                    return Ok(());
                                }
                                continue;
                            }
                        };
                        let aid = self
                            .account_id
                            .expect("authenticated session must have account_id");
                        match op::append::parse_args_for_dispatch(&parsed.args) {
                            Err(e) => {
                                self.bad_commands += 1;
                                let resp = tagged(&parsed.tag, Status::Bad, None, &e);
                                writer.write_all(resp.as_bytes()).await?;
                            }
                            Ok(()) => {
                                let resp = op::append::handle(
                                    &parsed.tag,
                                    &parsed.args,
                                    body,
                                    aid,
                                    self.mailstore.clone(),
                                )
                                .await;
                                writer.write_all(resp.as_bytes()).await?;
                            }
                        }
                    }
                }
                verb @ ("CLOSE" | "UNSELECT") => {
                    // RFC 9051 §6.4.2 / RFC 3691 §2 — both verbs
                    // are zero-argument. Refuse any trailing tokens
                    // so a typo'd argument is loud rather than
                    // silently ignored.
                    if !parsed.args.trim().is_empty() {
                        self.bad_commands += 1;
                        let resp =
                            tagged(&parsed.tag, Status::Bad, None, "command takes no arguments");
                        writer.write_all(resp.as_bytes()).await?;
                    } else if self.state.selected().is_none() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "no mailbox selected");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        let resp = if verb == "CLOSE" {
                            op::close::handle_close(&parsed.tag)
                        } else {
                            op::close::handle_unselect(&parsed.tag)
                        };
                        // Phase 2: no \Deleted purge yet (Phase 4
                        // ships STORE). Both verbs simply drop the
                        // selected mailbox.
                        self.state = State::Authenticated;
                        writer.write_all(resp.as_bytes()).await?;
                    }
                }
                "CHECK" => {
                    // RFC 3501 §6.4.1 — CHECK requests a mailbox
                    // checkpoint. Like CLOSE/UNSELECT it is a
                    // zero-argument, Selected-state-only verb; unlike
                    // them it leaves the mailbox selected. We have no
                    // deferred state to flush, so a successful CHECK is
                    // a NOOP that returns OK (RFC 3501 sanctions this).
                    // Thunderbird issues CHECK against every folder it
                    // touches; without this arm it falls to the
                    // catch-all and the client surfaces a spurious
                    // "command not implemented" alert.
                    if !parsed.args.trim().is_empty() {
                        self.bad_commands += 1;
                        let resp =
                            tagged(&parsed.tag, Status::Bad, None, "command takes no arguments");
                        writer.write_all(resp.as_bytes()).await?;
                    } else if self.state.selected().is_none() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "no mailbox selected");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        writer
                            .write_all(op::check::handle(&parsed.tag).as_bytes())
                            .await?;
                    }
                }
                verb @ ("CREATE" | "DELETE" | "RENAME" | "SUBSCRIBE" | "UNSUBSCRIBE") => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        let aid = self
                            .account_id
                            .expect("authenticated session must have account_id");
                        let resp = match verb {
                            "CREATE" => {
                                op::create::handle(
                                    &parsed.tag,
                                    &parsed.args,
                                    aid,
                                    self.mailstore.clone(),
                                )
                                .await
                            }
                            "DELETE" => {
                                op::delete::handle(
                                    &parsed.tag,
                                    &parsed.args,
                                    aid,
                                    self.mailstore.clone(),
                                )
                                .await
                            }
                            "RENAME" => {
                                op::rename::handle(
                                    &parsed.tag,
                                    &parsed.args,
                                    aid,
                                    self.mailstore.clone(),
                                )
                                .await
                            }
                            "SUBSCRIBE" => {
                                op::subscribe::handle(
                                    &parsed.tag,
                                    &parsed.args,
                                    aid,
                                    self.mailstore.clone(),
                                    true,
                                )
                                .await
                            }
                            _ => {
                                op::subscribe::handle(
                                    &parsed.tag,
                                    &parsed.args,
                                    aid,
                                    self.mailstore.clone(),
                                    false,
                                )
                                .await
                            }
                        };
                        writer.write_all(resp.as_bytes()).await?;
                    }
                }
                verb @ ("FETCH" | "STORE" | "SEARCH" | "COPY" | "MOVE" | "EXPUNGE" | "UID") => {
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        // Route to one of seven actions: Fetch(mode),
                        // Store(mode), Search(mode), Copy(mode),
                        // Move(mode), Expunge(uid?, args), or BadInner
                        // (UID with an unimplemented inner verb).
                        // RFC 9051 §6.4.8 — UID is followed by
                        // FETCH / COPY / MOVE / SEARCH / STORE /
                        // EXPUNGE. T9 shipped FETCH; T10 added STORE;
                        // T13 added SEARCH; T15-T17 add COPY / MOVE /
                        // EXPUNGE.
                        enum Action {
                            Fetch(op::fetch::FetchMode, String),
                            Store(op::store::StoreMode, String),
                            Search(op::search::SearchMode, String),
                            Copy(op::copy::CopyMode, String),
                            Move(op::mv::MoveMode, String),
                            Expunge(bool, String),
                            BadInner,
                        }
                        let action = if verb == "UID" {
                            let trimmed = parsed.args.trim_start();
                            let (inner_verb, inner_args) = match trimmed.split_once(' ') {
                                Some((v, a)) => (v, a),
                                None => (trimmed, ""),
                            };
                            if inner_verb.eq_ignore_ascii_case("FETCH") {
                                Action::Fetch(op::fetch::FetchMode::Uid, inner_args.to_string())
                            } else if inner_verb.eq_ignore_ascii_case("STORE") {
                                Action::Store(op::store::StoreMode::Uid, inner_args.to_string())
                            } else if inner_verb.eq_ignore_ascii_case("SEARCH") {
                                Action::Search(op::search::SearchMode::Uid, inner_args.to_string())
                            } else if inner_verb.eq_ignore_ascii_case("COPY") {
                                Action::Copy(op::copy::CopyMode::Uid, inner_args.to_string())
                            } else if inner_verb.eq_ignore_ascii_case("MOVE") {
                                Action::Move(op::mv::MoveMode::Uid, inner_args.to_string())
                            } else if inner_verb.eq_ignore_ascii_case("EXPUNGE") {
                                Action::Expunge(true, inner_args.to_string())
                            } else {
                                Action::BadInner
                            }
                        } else if verb == "FETCH" {
                            Action::Fetch(op::fetch::FetchMode::Msn, parsed.args.clone())
                        } else if verb == "STORE" {
                            Action::Store(op::store::StoreMode::Msn, parsed.args.clone())
                        } else if verb == "COPY" {
                            Action::Copy(op::copy::CopyMode::Msn, parsed.args.clone())
                        } else if verb == "MOVE" {
                            Action::Move(op::mv::MoveMode::Msn, parsed.args.clone())
                        } else if verb == "EXPUNGE" {
                            Action::Expunge(false, parsed.args.clone())
                        } else {
                            // verb == "SEARCH"
                            Action::Search(op::search::SearchMode::Msn, parsed.args.clone())
                        };

                        match action {
                            Action::BadInner => {
                                self.bad_commands += 1;
                                let resp = tagged(
                                    &parsed.tag,
                                    Status::Bad,
                                    None,
                                    "UID inner command not implemented in this phase",
                                );
                                writer.write_all(resp.as_bytes()).await?;
                            }
                            Action::Fetch(mode, args) => {
                                match self.state.selected() {
                                    Some(sm) => {
                                        let container = sm.container_id;
                                        let aid = self
                                            .account_id
                                            .expect("authenticated session must have account_id");
                                        // Pre-validate so grammar
                                        // failures bump bad_commands
                                        // and feed the abuse-cap.
                                        // `op::fetch::handle` re-parses
                                        // on its own path; the second
                                        // parse is pure and trivial.
                                        match op::fetch::parse_args(&args, mode) {
                                            Err(e) => {
                                                self.bad_commands += 1;
                                                let resp =
                                                    tagged(&parsed.tag, Status::Bad, None, &e);
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                            Ok(_) => {
                                                let resp = op::fetch::handle(
                                                    &parsed.tag,
                                                    &args,
                                                    aid,
                                                    container,
                                                    self.mailstore.clone(),
                                                    mode,
                                                )
                                                .await;
                                                writer.write_all(&resp).await?;
                                            }
                                        }
                                    }
                                    None => {
                                        self.bad_commands += 1;
                                        let resp = tagged(
                                            &parsed.tag,
                                            Status::Bad,
                                            None,
                                            "no mailbox selected",
                                        );
                                        writer.write_all(resp.as_bytes()).await?;
                                    }
                                }
                            }
                            Action::Search(mode, args) => {
                                match self.state.selected() {
                                    Some(sm) => {
                                        let container = sm.container_id;
                                        let aid = self
                                            .account_id
                                            .expect("authenticated session must have account_id");
                                        // Pre-validate so grammar
                                        // failures bump bad_commands.
                                        // `op::search::handle` re-parses
                                        // on its own path; the second
                                        // parse is pure and trivial.
                                        match op::search::parse_args_for_dispatch(&args, mode) {
                                            Err(e) => {
                                                self.bad_commands += 1;
                                                let resp =
                                                    tagged(&parsed.tag, Status::Bad, None, &e);
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                            Ok(()) => {
                                                let resp = op::search::handle(
                                                    &parsed.tag,
                                                    &args,
                                                    aid,
                                                    container,
                                                    self.mailstore.clone(),
                                                    mode,
                                                )
                                                .await;
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                        }
                                    }
                                    None => {
                                        self.bad_commands += 1;
                                        let resp = tagged(
                                            &parsed.tag,
                                            Status::Bad,
                                            None,
                                            "no mailbox selected",
                                        );
                                        writer.write_all(resp.as_bytes()).await?;
                                    }
                                }
                            }
                            Action::Store(mode, args) => {
                                match self.state.selected() {
                                    Some(sm) if sm.read_only => {
                                        // EXAMINE selected the mailbox
                                        // read-only — STORE must not
                                        // mutate. RFC 9051 §6.3.2:
                                        // EXAMINE is identical to
                                        // SELECT but rejects any
                                        // command that would change
                                        // mailbox state. Use the
                                        // CANNOT response code so the
                                        // client can distinguish this
                                        // from a transient NO.
                                        self.bad_commands += 1;
                                        let resp = tagged(
                                            &parsed.tag,
                                            Status::No,
                                            Some("CANNOT"),
                                            "mailbox is read-only",
                                        );
                                        writer.write_all(resp.as_bytes()).await?;
                                    }
                                    Some(sm) => {
                                        let container = sm.container_id;
                                        let aid = self
                                            .account_id
                                            .expect("authenticated session must have account_id");
                                        // Pre-validate so grammar
                                        // failures bump bad_commands.
                                        // `op::store::handle` re-parses
                                        // on its own path; the second
                                        // parse is pure and trivial.
                                        match op::store::parse_args_for_dispatch(&args, mode) {
                                            Err(e) => {
                                                self.bad_commands += 1;
                                                let resp =
                                                    tagged(&parsed.tag, Status::Bad, None, &e);
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                            Ok(()) => {
                                                let resp = op::store::handle(
                                                    &parsed.tag,
                                                    &args,
                                                    aid,
                                                    container,
                                                    self.mailstore.clone(),
                                                    mode,
                                                )
                                                .await;
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                        }
                                    }
                                    None => {
                                        self.bad_commands += 1;
                                        let resp = tagged(
                                            &parsed.tag,
                                            Status::Bad,
                                            None,
                                            "no mailbox selected",
                                        );
                                        writer.write_all(resp.as_bytes()).await?;
                                    }
                                }
                            }
                            Action::Copy(mode, args) => {
                                // COPY *does* permit read-only source
                                // (RFC 9051 §6.4.7 — the dest is the
                                // mutating party). EXAMINE'd mailboxes
                                // can be copied OUT of safely.
                                match self.state.selected() {
                                    Some(sm) => {
                                        let container = sm.container_id;
                                        let read_only_src = sm.read_only;
                                        let aid = self
                                            .account_id
                                            .expect("authenticated session must have account_id");
                                        match op::copy::parse_args_for_dispatch(&args, mode) {
                                            Err(e) => {
                                                self.bad_commands += 1;
                                                let resp =
                                                    tagged(&parsed.tag, Status::Bad, None, &e);
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                            Ok(()) => {
                                                let resp = op::copy::handle(
                                                    &parsed.tag,
                                                    &args,
                                                    aid,
                                                    container,
                                                    self.mailstore.clone(),
                                                    mode,
                                                    read_only_src,
                                                )
                                                .await;
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                        }
                                    }
                                    None => {
                                        self.bad_commands += 1;
                                        let resp = tagged(
                                            &parsed.tag,
                                            Status::Bad,
                                            None,
                                            "no mailbox selected",
                                        );
                                        writer.write_all(resp.as_bytes()).await?;
                                    }
                                }
                            }
                            Action::Move(mode, args) => {
                                match self.state.selected() {
                                    Some(sm) if sm.read_only => {
                                        // MOVE removes the source
                                        // membership — same read-only
                                        // gate as STORE / EXPUNGE.
                                        self.bad_commands += 1;
                                        let resp = tagged(
                                            &parsed.tag,
                                            Status::No,
                                            Some("CANNOT"),
                                            "mailbox is read-only",
                                        );
                                        writer.write_all(resp.as_bytes()).await?;
                                    }
                                    Some(sm) => {
                                        let container = sm.container_id;
                                        let aid = self
                                            .account_id
                                            .expect("authenticated session must have account_id");
                                        match op::mv::parse_args_for_dispatch(&args, mode) {
                                            Err(e) => {
                                                self.bad_commands += 1;
                                                let resp =
                                                    tagged(&parsed.tag, Status::Bad, None, &e);
                                                writer.write_all(resp.as_bytes()).await?;
                                            }
                                            Ok(()) => {
                                                let outcome = op::mv::handle(
                                                    &parsed.tag,
                                                    &args,
                                                    aid,
                                                    container,
                                                    self.mailstore.clone(),
                                                    mode,
                                                )
                                                .await;
                                                writer.write_all(outcome.wire.as_bytes()).await?;
                                                // Prune the session's view UID
                                                // list in lockstep with the
                                                // EXPUNGE lines the handler
                                                // emitted, so a later IDLE
                                                // entry does not re-diff the
                                                // same moved-out UIDs.
                                                if !outcome.expunged_uids.is_empty()
                                                    && let Some(sm) = self.state.selected_mut()
                                                {
                                                    let gone: std::collections::HashSet<u64> =
                                                        outcome
                                                            .expunged_uids
                                                            .iter()
                                                            .copied()
                                                            .collect();
                                                    sm.view_uids.retain(|u| !gone.contains(u));
                                                }
                                            }
                                        }
                                    }
                                    None => {
                                        self.bad_commands += 1;
                                        let resp = tagged(
                                            &parsed.tag,
                                            Status::Bad,
                                            None,
                                            "no mailbox selected",
                                        );
                                        writer.write_all(resp.as_bytes()).await?;
                                    }
                                }
                            }
                            Action::Expunge(uid_mode, args) => match self.state.selected() {
                                Some(sm) if sm.read_only => {
                                    self.bad_commands += 1;
                                    let resp = tagged(
                                        &parsed.tag,
                                        Status::No,
                                        Some("CANNOT"),
                                        "mailbox is read-only",
                                    );
                                    writer.write_all(resp.as_bytes()).await?;
                                }
                                Some(sm) => {
                                    let container = sm.container_id;
                                    let aid = self
                                        .account_id
                                        .expect("authenticated session must have account_id");
                                    match op::expunge::parse_args_for_dispatch(&args, uid_mode) {
                                        Err(e) => {
                                            self.bad_commands += 1;
                                            let resp = tagged(&parsed.tag, Status::Bad, None, &e);
                                            writer.write_all(resp.as_bytes()).await?;
                                        }
                                        Ok(()) => {
                                            let outcome = op::expunge::handle(
                                                &parsed.tag,
                                                &args,
                                                aid,
                                                container,
                                                self.mailstore.clone(),
                                                uid_mode,
                                            )
                                            .await;
                                            writer.write_all(outcome.wire.as_bytes()).await?;
                                            if !outcome.expunged_uids.is_empty()
                                                && let Some(sm) = self.state.selected_mut()
                                            {
                                                let gone: std::collections::HashSet<u64> =
                                                    outcome.expunged_uids.iter().copied().collect();
                                                sm.view_uids.retain(|u| !gone.contains(u));
                                            }
                                        }
                                    }
                                }
                                None => {
                                    self.bad_commands += 1;
                                    let resp = tagged(
                                        &parsed.tag,
                                        Status::Bad,
                                        None,
                                        "no mailbox selected",
                                    );
                                    writer.write_all(resp.as_bytes()).await?;
                                }
                            },
                        }
                    }
                }
                "IDLE" => {
                    // RFC 2177 — IDLE is valid in Authenticated and
                    // Selected states. The handler streams untagged
                    // EXISTS / EXPUNGE while a notifier subscription
                    // is active (Selected only) and waits for DONE.
                    if !self.state.is_authenticated() {
                        self.bad_commands += 1;
                        let resp = tagged(&parsed.tag, Status::Bad, None, "NOT AUTHENTICATED");
                        writer.write_all(resp.as_bytes()).await?;
                    } else {
                        let aid = self
                            .account_id
                            .expect("authenticated session must have account_id");
                        // Pass &mut SelectedMailbox so the handler
                        // can diff against and update `view_uids` in
                        // place. The recv loop mutates the very
                        // same vector the session reads, so the
                        // next IDLE entry sees a coherent view.
                        let outcome = op::idle::handle(
                            &parsed.tag,
                            &parsed.args,
                            self.state.selected_mut(),
                            aid,
                            self.mailstore.clone(),
                            reader,
                            writer,
                            self.cfg.idle_status_interval,
                        )
                        .await?;
                        match outcome {
                            op::idle::IdleOutcome::Done => {}
                            op::idle::IdleOutcome::Close => {
                                return Ok(());
                            }
                        }
                    }
                }
                "STARTTLS" => {
                    // Implicit TLS only — refuse explicitly per the
                    // doc rather than letting the catch-all map it to
                    // "command not implemented".
                    self.bad_commands += 1;
                    writer
                        .write_all(
                            tagged(
                                &parsed.tag,
                                Status::Bad,
                                None,
                                "STARTTLS not supported; use implicit TLS on 993",
                            )
                            .as_bytes(),
                        )
                        .await?;
                }
                _ => {
                    self.bad_commands += 1;
                    writer
                        .write_all(
                            tagged(
                                &parsed.tag,
                                Status::Bad,
                                None,
                                "command not implemented in this phase",
                            )
                            .as_bytes(),
                        )
                        .await?;
                }
            }

            writer.flush().await?;

            if self.over_bad_command_cap() {
                self.emit_bye(writer, "too many bad commands").await?;
                return Ok(());
            }
            if self.auth_failures >= self.cfg.max_auth_failures {
                self.emit_bye(writer, "too many auth failures").await?;
                return Ok(());
            }
        }
    }

    fn over_bad_command_cap(&self) -> bool {
        let cap = if self.state.is_authenticated() {
            self.cfg.max_bad_commands_post_auth
        } else {
            self.cfg.max_bad_commands_pre_auth
        };
        self.bad_commands >= cap
    }

    async fn emit_bye<W: AsyncWrite + Unpin>(&self, writer: &mut W, reason: &str) -> Result<()> {
        let code = if self.state.is_authenticated() {
            None
        } else {
            Some("CLIENTBUG")
        };
        let line = match code {
            Some(c) => format!("* BYE [{c}] {reason}\r\n"),
            None => untagged_bye(reason),
        };
        writer.write_all(line.as_bytes()).await?;
        let _ = writer.flush().await;
        Ok(())
    }

    /// Read one IMAP command (line + optional trailing literal),
    /// applying the pre-auth idle timeout.
    ///
    /// Returns `Ok(ReadOutcome::Eof)` on clean peer close,
    /// `Ok(ReadOutcome::Frame(cmd))` on a clean read, and
    /// `Ok(ReadOutcome::LiteralRejected { tag, response })` when the
    /// codec resolved a literal-framing problem without losing stream
    /// sync — caller emits `response` and continues.
    async fn read_command<R, W>(
        &mut self,
        reader: &mut BufReader<R>,
        writer: &mut W,
    ) -> Result<ReadOutcome>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let max_lit = self.cfg.max_literal_bytes;
        // Refuse literals before the session is authenticated, and
        // post-auth refuse them for any verb that doesn't actually
        // consume a literal body. With `LITERAL+` advertised in the
        // greeting an unauthenticated peer — or an authenticated one
        // issuing `CAPABILITY {N+}` etc. — could otherwise push
        // `max_literal_bytes` (default 64 MiB) into RAM before any
        // account-scoped cap applies. The only verb in T14 that legally
        // takes a literal is APPEND; later phases (AUTHENTICATE
        // continuation, SEARCH, STORE flags via literal8) extend this
        // allow-list as those surfaces land. Refused literals leave the
        // stream in sync — sync form withholds the `+ Ready`
        // continuation; non-sync form drains the body + trailing CRLF
        // before responding.
        let authed = self.state.is_authenticated();
        let accept_literals = move |line: &[u8]| -> bool {
            if !authed {
                return false;
            }
            let mut parts = line.splitn(3, |b| *b == b' ');
            let _tag = parts.next();
            let verb = parts.next().unwrap_or(b"");
            verb.eq_ignore_ascii_case(b"APPEND")
        };
        let fut = codec::read_command(reader, writer, max_lit, accept_literals);
        let result = if !self.state.is_authenticated() {
            match timeout(self.cfg.pre_auth_timeout, fut).await {
                Ok(r) => r,
                Err(_) => {
                    tracing::debug!(peer = %self.peer, "imap pre-auth timeout");
                    return Ok(ReadOutcome::Eof);
                }
            }
        } else {
            // Phase 1 has no post-auth idle command yet; rely on TCP
            // keepalive / OS timeouts. Phase 5 (IDLE) wires explicit
            // post-auth read timeouts.
            fut.await
        };
        match result {
            Ok(cmd) => Ok(ReadOutcome::Frame(cmd)),
            Err(ReadCommandError::Eof) => Ok(ReadOutcome::Eof),
            Err(ReadCommandError::LineTooLong) => Err(anyhow::anyhow!("line too long")),
            Err(ReadCommandError::Io(e)) => Err(anyhow::Error::new(e)),
            Err(ReadCommandError::MalformedLiteral { line, reason }) => {
                self.bad_commands += 1;
                let tag = sniff_tag(&line).unwrap_or_else(|| "*".to_string());
                let response = tagged(
                    &tag,
                    Status::Bad,
                    None,
                    &format!("malformed literal: {reason}"),
                );
                Ok(ReadOutcome::LiteralRejected { response })
            }
            Err(ReadCommandError::SyncLiteralRefused { line, declared }) => {
                self.bad_commands += 1;
                let tag = sniff_tag(&line).unwrap_or_else(|| "*".to_string());
                let authed = self.state.is_authenticated();
                tracing::debug!(
                    peer = %self.peer,
                    declared,
                    authed,
                    "imap sync literal refused (pre-auth or verb does not take literal)"
                );
                let msg = if authed {
                    "literal not accepted for this command"
                } else {
                    "literal not accepted before authentication"
                };
                let response = tagged(&tag, Status::Bad, None, msg);
                Ok(ReadOutcome::LiteralRejected { response })
            }
            Err(ReadCommandError::NonSyncLiteralRefused { line, declared }) => {
                self.bad_commands += 1;
                let tag = sniff_tag(&line).unwrap_or_else(|| "*".to_string());
                let authed = self.state.is_authenticated();
                tracing::debug!(
                    peer = %self.peer,
                    declared,
                    authed,
                    "imap non-sync literal refused (bytes drained); pre-auth or verb does not take literal"
                );
                let msg = if authed {
                    "literal not accepted for this command"
                } else {
                    "literal not accepted before authentication"
                };
                let response = tagged(&tag, Status::Bad, None, msg);
                Ok(ReadOutcome::LiteralRejected { response })
            }
            Err(ReadCommandError::SyncLiteralTooLarge {
                line,
                declared,
                cap,
            }) => {
                self.bad_commands += 1;
                let tag = sniff_tag(&line).unwrap_or_else(|| "*".to_string());
                tracing::debug!(
                    peer = %self.peer,
                    declared,
                    cap,
                    "imap sync literal rejected (over cap)"
                );
                let response = tagged(
                    &tag,
                    Status::No,
                    Some("TOOBIG"),
                    &format!("literal {declared} exceeds server cap {cap}"),
                );
                Ok(ReadOutcome::LiteralRejected { response })
            }
            Err(ReadCommandError::NonSyncLiteralTooLarge {
                line,
                declared,
                cap,
            }) => {
                self.bad_commands += 1;
                let tag = sniff_tag(&line).unwrap_or_else(|| "*".to_string());
                tracing::debug!(
                    peer = %self.peer,
                    declared,
                    cap,
                    "imap non-sync literal rejected (over cap; bytes drained)"
                );
                let response = tagged(
                    &tag,
                    Status::No,
                    Some("TOOBIG"),
                    &format!("literal {declared} exceeds server cap {cap}"),
                );
                Ok(ReadOutcome::LiteralRejected { response })
            }
        }
    }

    async fn handle_login<W: AsyncWrite + Unpin>(
        &mut self,
        tag: &str,
        args: &str,
        writer: &mut W,
    ) -> Result<()> {
        if !self.state.is_not_authenticated() {
            self.bad_commands += 1;
            let resp = tagged(tag, Status::Bad, None, "LOGIN only valid before auth");
            writer.write_all(resp.as_bytes()).await?;
            return Ok(());
        }
        let (user, pass) = match op::login::parse_args(args) {
            Ok(t) => t,
            Err(e) => {
                self.bad_commands += 1;
                let msg = format!("invalid LOGIN syntax: {e}");
                writer
                    .write_all(tagged(tag, Status::Bad, None, &msg).as_bytes())
                    .await?;
                return Ok(());
            }
        };
        self.complete_auth(tag, &user, &pass, "cleartext", writer)
            .await
    }

    async fn handle_authenticate<R, W>(
        &mut self,
        tag: &str,
        args: &str,
        reader: &mut BufReader<R>,
        writer: &mut W,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        if !self.state.is_not_authenticated() {
            self.bad_commands += 1;
            let resp = tagged(
                tag,
                Status::Bad,
                None,
                "AUTHENTICATE only valid before auth",
            );
            writer.write_all(resp.as_bytes()).await?;
            return Ok(());
        }
        let parsed = match op::authenticate::parse_args(args) {
            Ok(p) => p,
            Err(e) => {
                self.bad_commands += 1;
                let msg = format!("invalid AUTHENTICATE: {e}");
                writer
                    .write_all(tagged(tag, Status::Bad, None, &msg).as_bytes())
                    .await?;
                return Ok(());
            }
        };

        match parsed.mechanism {
            Mechanism::Plain => {
                let ir = match parsed.initial_response {
                    Some(s) => s,
                    None => {
                        writer.write_all(continuation("").as_bytes()).await?;
                        writer.flush().await?;
                        match self.read_continuation(reader).await? {
                            Some(s) => s,
                            None => return Ok(()),
                        }
                    }
                };
                // RFC 4959 §3 — client may send `*` to abort.
                if ir.trim() == "*" {
                    self.auth_failures += 1;
                    writer
                        .write_all(
                            tagged(tag, Status::Bad, None, "AUTHENTICATE aborted").as_bytes(),
                        )
                        .await?;
                    return Ok(());
                }
                let creds = match sasl::parse_plain(&ir) {
                    Ok(c) => c,
                    Err(e) => {
                        self.auth_failures += 1;
                        let msg = format!("PLAIN decode failed: {e}");
                        writer
                            .write_all(tagged(tag, Status::Bad, None, &msg).as_bytes())
                            .await?;
                        return Ok(());
                    }
                };
                self.complete_auth(tag, &creds.authcid, &creds.password, "PLAIN", writer)
                    .await
            }
            Mechanism::Login => {
                // SASL-IR (RFC 4959): if the client supplied an
                // initial response, it stands in for the first
                // continuation (the base64-encoded username) and we
                // jump straight to prompting for the password.
                let user_b64 = match parsed.initial_response {
                    Some(s) => s,
                    None => {
                        // Step 1: "Username:" base64 = "VXNlcm5hbWU6"
                        writer
                            .write_all(continuation(&sasl::encode_b64(b"Username:")).as_bytes())
                            .await?;
                        writer.flush().await?;
                        match self.read_continuation(reader).await? {
                            Some(s) => s,
                            None => return Ok(()),
                        }
                    }
                };
                if user_b64.trim() == "*" {
                    self.auth_failures += 1;
                    writer
                        .write_all(
                            tagged(tag, Status::Bad, None, "AUTHENTICATE aborted").as_bytes(),
                        )
                        .await?;
                    return Ok(());
                }
                let user = match sasl::decode_login_field(&user_b64) {
                    Ok(s) => s,
                    Err(e) => {
                        self.auth_failures += 1;
                        let msg = format!("LOGIN username decode failed: {e}");
                        writer
                            .write_all(tagged(tag, Status::Bad, None, &msg).as_bytes())
                            .await?;
                        return Ok(());
                    }
                };
                // Step 2: "Password:" base64 = "UGFzc3dvcmQ6"
                writer
                    .write_all(continuation(&sasl::encode_b64(b"Password:")).as_bytes())
                    .await?;
                writer.flush().await?;
                let pass_b64 = match self.read_continuation(reader).await? {
                    Some(s) => s,
                    None => return Ok(()),
                };
                if pass_b64.trim() == "*" {
                    self.auth_failures += 1;
                    writer
                        .write_all(
                            tagged(tag, Status::Bad, None, "AUTHENTICATE aborted").as_bytes(),
                        )
                        .await?;
                    return Ok(());
                }
                let pass = match sasl::decode_login_field(&pass_b64) {
                    Ok(s) => s,
                    Err(e) => {
                        self.auth_failures += 1;
                        let msg = format!("LOGIN password decode failed: {e}");
                        writer
                            .write_all(tagged(tag, Status::Bad, None, &msg).as_bytes())
                            .await?;
                        return Ok(());
                    }
                };
                self.complete_auth(tag, &user, &pass, "LOGIN", writer).await
            }
            Mechanism::OAuthBearer => {
                // Provider integration not landed — clear NO, not BAD
                // (per `_doc/maild/imap.md` SASL note: authentication
                // refusal is NO, not BAD).
                self.auth_failures += 1;
                if parsed.initial_response.is_none() {
                    // Send empty challenge so a compliant client cancels
                    // with "*" without us waiting on a real token blob.
                    writer.write_all(continuation("").as_bytes()).await?;
                    writer.flush().await?;
                    let _ = self.read_continuation(reader).await?;
                }
                let resp = tagged(
                    tag,
                    Status::No,
                    Some("AUTHENTICATIONFAILED"),
                    "OAUTHBEARER not configured",
                );
                writer.write_all(resp.as_bytes()).await?;
                Ok(())
            }
        }
    }

    async fn read_continuation<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut BufReader<R>,
    ) -> Result<Option<String>> {
        let line_fut = codec::read_line(reader);
        let line = match timeout(self.cfg.pre_auth_timeout, line_fut).await {
            Ok(Ok(v)) => v,
            Ok(Err(ReadLineError::Eof)) => return Ok(None),
            Ok(Err(ReadLineError::LineTooLong)) => {
                return Err(anyhow::anyhow!("continuation line too long"));
            }
            Ok(Err(ReadLineError::Io(e))) => return Err(anyhow::Error::new(e)),
            Err(_) => return Ok(None),
        };
        let text = std::str::from_utf8(&line)
            .map_err(|_| anyhow::anyhow!("non-UTF8 continuation"))?
            .trim_end_matches('\r')
            .to_string();
        Ok(Some(text))
    }

    async fn complete_auth<W: AsyncWrite + Unpin>(
        &mut self,
        tag: &str,
        user: &str,
        pass: &str,
        method: &str,
        writer: &mut W,
    ) -> Result<()> {
        let aid = match basic::verify(&self.db, user, pass).await {
            Ok(Some(a)) => a,
            Ok(None) => {
                self.auth_failures += 1;
                crate::maillog::imap_login_fail(
                    user,
                    method,
                    self.peer,
                    self.local,
                    self.auth_failures,
                    "auth failed",
                );
                let resp = tagged(
                    tag,
                    Status::No,
                    Some("AUTHENTICATIONFAILED"),
                    "invalid credentials",
                );
                writer.write_all(resp.as_bytes()).await?;
                return Ok(());
            }
            Err(e) => {
                self.auth_failures += 1;
                tracing::error!(peer = %self.peer, error = %e, "imap auth lookup failed");
                crate::maillog::imap_login_fail(
                    user,
                    method,
                    self.peer,
                    self.local,
                    self.auth_failures,
                    "server error",
                );
                let resp = tagged(
                    tag,
                    Status::No,
                    Some("SERVERBUG"),
                    "authentication unavailable",
                );
                writer.write_all(resp.as_bytes()).await?;
                return Ok(());
            }
        };

        // Reserve a per-account slot before flipping state. Failure
        // closes the connection with a polite `* BYE [INUSE]`.
        if !self
            .slots
            .try_acquire(aid, self.cfg.max_concurrent_per_account)
            .await
        {
            let resp = tagged(tag, Status::No, Some("INUSE"), "too many connections");
            writer.write_all(resp.as_bytes()).await?;
            self.emit_bye(writer, "concurrent connection limit reached")
                .await?;
            // Force the loop to exit.
            self.state = State::Logout;
            return Ok(());
        }
        self.account_id = Some(aid);
        self.state = State::Authenticated;

        // Per RFC 9051 §6.2.2 / §6.2.3, embed CAPABILITY in the OK
        // response code to defeat client-side caching of the pre-auth
        // capability list.
        let caps_body = match self.cfg.advertise_capabilities.as_ref() {
            Some(o) => crate::imap::capabilities::render_owned(o),
            None => crate::imap::capabilities::render(crate::imap::capabilities::INITIAL),
        };
        let code = format!("CAPABILITY {caps_body}");
        let resp = tagged(tag, Status::Ok, Some(&code), "authenticated");
        writer.write_all(resp.as_bytes()).await?;
        crate::maillog::imap_login_ok(user, method, self.peer, self.local);
        Ok(())
    }
}

/// Best-effort tag extraction for a structurally malformed command
/// line (so the `BAD` response can still echo the client's tag).
fn sniff_tag(line: &[u8]) -> Option<String> {
    let end = line.iter().position(|b| *b == b' ' || *b == b'\r')?;
    if end == 0 {
        return None;
    }
    let slice = &line[..end];
    if slice.iter().all(|b| b.is_ascii_graphic() && *b != b'+') {
        std::str::from_utf8(slice).ok().map(str::to_string)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `snapshot` reflects live acquires, drops to zero-count rows once
    /// released, and never reports a row whose count reached zero —
    /// the contract `maild.stats.online` depends on.
    #[tokio::test]
    async fn snapshot_reflects_acquire_and_release() {
        let slots = AccountSlots::new();
        // Empty before any connection.
        assert!(slots.snapshot().await.is_empty());

        // Two sessions for account 7, one for account 3 (cap high
        // enough that none are refused).
        assert!(slots.try_acquire(7, 10).await);
        assert!(slots.try_acquire(7, 10).await);
        assert!(slots.try_acquire(3, 10).await);

        let mut snap = slots.snapshot().await;
        snap.sort_by_key(|(id, _)| *id);
        assert_eq!(snap, vec![(3, 1), (7, 2)]);

        // Releasing one of account 7's two sessions leaves it at 1.
        slots.release(7).await;
        let mut snap = slots.snapshot().await;
        snap.sort_by_key(|(id, _)| *id);
        assert_eq!(snap, vec![(3, 1), (7, 1)]);

        // Releasing account 3's only session drops the row entirely
        // (no zero-valued rows in the snapshot).
        slots.release(3).await;
        let snap = slots.snapshot().await;
        assert_eq!(snap, vec![(7, 1)]);
    }

    /// A refused acquire (cap reached) does not inflate the snapshot
    /// count — only successful reservations are visible.
    #[tokio::test]
    async fn snapshot_excludes_refused_acquire() {
        let slots = AccountSlots::new();
        assert!(slots.try_acquire(5, 1).await);
        // Second acquire for account 5 is over the cap of 1 → refused.
        assert!(!slots.try_acquire(5, 1).await);
        assert_eq!(slots.snapshot().await, vec![(5, 1)]);
    }
}
