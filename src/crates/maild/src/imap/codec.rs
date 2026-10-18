//! Minimal CRLF line codec.
//!
//! Phase 1 verbs (CAPABILITY, NOOP, LOGOUT, ID, LOGIN, AUTHENTICATE)
//! never include IMAP literals on the wire, so a strict CRLF line
//! reader was sufficient for them. Phase 4 (APPEND, T14) introduces
//! a single trailing literal at the end of a command — handled here
//! by [`read_command`], which:
//!
//! * reads the first line via [`read_line`];
//! * detects a trailing `{N}` (synchronizing) or `{N+}` (RFC 7888
//!   non-synchronizing) literal marker;
//! * for sync literals, writes `+ Ready for literal data\r\n` and
//!   flushes before reading the body — clients block on this
//!   continuation per RFC 9051 §6.3.12;
//! * reads exactly `N` bytes of body via `read_exact`;
//! * reads the trailing CRLF that terminates the command (which must
//!   be empty for T14's "literal is the final argument" scope —
//!   embedded mid-command literals are not supported until the full
//!   imap-codec swap lands).
//!
//! Oversized literals are handled per protocol:
//! * **Sync** (`{N}`): we refuse the continuation, so the client
//!   never sends the body. Stream stays in sync; we return
//!   [`ReadCommandError::SyncLiteralTooLarge`] and the caller emits
//!   `BAD literal too large` / `NO [TOOBIG]`.
//! * **Non-sync** (`{N+}`): the client sends the body without
//!   waiting, so we drain the announced bytes (and the trailing
//!   CRLF) to preserve stream sync (RFC 7888 §4), then return
//!   [`ReadCommandError::NonSyncLiteralTooLarge`].
//!
//! [`parse_line`] is unchanged from Phase 1 and is called on
//! [`Command::line`] by the session dispatcher. It still rejects any
//! line that contains an unstripped literal marker, which after
//! [`read_command`] can only happen if the marker was malformed — in
//! that case [`read_command`] returns [`ReadCommandError::MalformedLiteral`]
//! ahead of [`parse_line`] ever seeing the line.
//!
//! Command framing (no literal):
//!   `<tag> <command> [<args>]\r\n`
//! where tag/command are ASCII tokens (no whitespace, no NUL), and
//! `<args>` is the remainder of the line verbatim. The session loop
//! re-parses `<args>` per-verb because the v1 verb set has very
//! different argument shapes (`LOGIN user pass` vs `ID NIL` vs
//! `AUTHENTICATE PLAIN [<initial-response>]`).

use std::io::ErrorKind;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Per-line read limit. Phase 1 commands fit comfortably under 4 KiB
/// (tag + verb + LOGIN args ≤ ~200 B; AUTHENTICATE SASL-IR base64 of
/// a 2 KiB credential is ~3 KiB). 8 KiB gives headroom without
/// allowing a hostile client to exhaust memory pre-auth.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// A parsed IMAP command line — Phase 1 framing only.
#[derive(Debug, Clone)]
pub struct Line {
    /// The literal tag bytes (ASCII, ≥1 character, no whitespace).
    pub tag: String,
    /// The verb upper-cased for case-insensitive dispatch.
    pub verb_upper: String,
    /// Argument tail verbatim, with the trailing CRLF stripped. May
    /// be empty (e.g. `a NOOP\r\n`).
    pub args: String,
}

/// Read a single CRLF-terminated line.
///
/// Errors:
/// - `LineTooLong` — line exceeds [`MAX_LINE_BYTES`] before CRLF.
/// - `Eof` — peer closed before a complete line arrived.
/// - I/O — underlying transport failure.
///
/// The CRLF is consumed but NOT included in the returned `Vec<u8>`.
pub async fn read_line<R>(reader: &mut BufReader<R>) -> Result<Vec<u8>, ReadLineError>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(128);
    loop {
        let mut byte = [0u8; 1];
        match reader.read_exact(&mut byte).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Err(ReadLineError::Eof),
            Err(e) => return Err(ReadLineError::Io(e)),
        }
        if byte[0] == b'\n' {
            if buf.last() == Some(&b'\r') {
                buf.pop();
                return Ok(buf);
            }
            // Bare LF without preceding CR — accept as a line ending
            // for tolerance against telnet-style debug clients, but
            // do not silently drop the CR for normal traffic.
            return Ok(buf);
        }
        if buf.len() >= MAX_LINE_BYTES {
            return Err(ReadLineError::LineTooLong);
        }
        buf.push(byte[0]);
    }
}

#[derive(Debug)]
pub enum ReadLineError {
    Eof,
    LineTooLong,
    Io(std::io::Error),
}

impl std::fmt::Display for ReadLineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => write!(f, "peer closed"),
            Self::LineTooLong => write!(f, "line exceeded {MAX_LINE_BYTES} bytes"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for ReadLineError {}

/// Parse a command framing line (tag + verb + args).
///
/// Returns `Err` if the line is structurally malformed (no tag, no
/// verb, contains NUL, or signals an unsupported literal). The
/// caller maps `Err` to `BAD` and increments the bad-command counter.
pub fn parse_line(line: &[u8]) -> Result<Line> {
    if line.is_empty() {
        return Err(anyhow!("empty command line"));
    }
    if line.contains(&0u8) {
        return Err(anyhow!("NUL byte in command line"));
    }
    // Reject IMAP literal indicators at this phase. A line whose
    // last non-whitespace token is `{NNN}` or `{NNN+}` means the
    // client expects to send NNN bytes of literal data on the next
    // line; Phase 1 has no verb that should receive a literal.
    if has_unsupported_literal(line) {
        return Err(anyhow!("literal not supported in this phase"));
    }
    let text = std::str::from_utf8(line).map_err(|_| anyhow!("non-UTF8 command line"))?;

    let mut parts = text.splitn(3, ' ');
    let tag = parts
        .next()
        .ok_or_else(|| anyhow!("missing tag"))?
        .trim_end_matches('\r');
    if tag.is_empty() || !is_valid_tag(tag) {
        return Err(anyhow!("invalid tag"));
    }
    let verb = parts.next().ok_or_else(|| anyhow!("missing command"))?;
    if verb.is_empty() {
        return Err(anyhow!("missing command"));
    }
    let args = parts
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .to_string();
    Ok(Line {
        tag: tag.to_string(),
        verb_upper: verb.to_ascii_uppercase(),
        args,
    })
}

/// A complete IMAP command frame: the command line (with any trailing
/// literal marker stripped) plus the literal body, if present.
///
/// Phase 4 T14 scope: exactly one literal, anchored at the end of the
/// command. Embedded mid-command literals (e.g. `LOGIN {n}\r\n...{n}\r\n...`)
/// would require re-entering the literal protocol for every fragment;
/// they remain unsupported until the imap-codec/imap-next swap.
#[derive(Debug, Clone)]
pub struct Command {
    /// Command line bytes with any trailing literal marker (and the
    /// single whitespace that separated it) stripped. Fed to
    /// [`parse_line`] by the session dispatcher.
    pub line: Vec<u8>,
    /// Literal body bytes — exactly `N` octets when present. `None`
    /// means the command had no literal indicator.
    pub literal: Option<Vec<u8>>,
}

/// Possible outcomes from [`read_command`].
#[derive(Debug)]
pub enum ReadCommandError {
    /// Peer closed cleanly. Caller treats as connection-end, not error.
    Eof,
    /// Command line (or post-literal trailing line) exceeded
    /// [`MAX_LINE_BYTES`] before CRLF.
    LineTooLong,
    /// Underlying transport failure.
    Io(std::io::Error),
    /// Literal indicator was structurally malformed — non-numeric `N`,
    /// missing `}`, empty body, integer overflow, or content after the
    /// literal body in a position we don't support in this phase. No
    /// continuation was issued; the read stream is in sync. Caller
    /// maps to `BAD <message>` and continues. `line` carries the
    /// command bytes preceding the bad marker so the dispatcher can
    /// sniff the tag and emit a *tagged* `BAD` rather than an untagged
    /// one (a strict client may keep waiting for tagged completion
    /// otherwise).
    MalformedLiteral { line: Vec<u8>, reason: String },
    /// Sync literal refused before the continuation was issued —
    /// e.g. pre-auth literals when `accept_literals = false`. No
    /// continuation, stream stays in sync because the client must
    /// wait for `+ Ready` before sending body. Carries the line so
    /// the dispatcher can sniff the tag.
    SyncLiteralRefused { line: Vec<u8>, declared: u64 },
    /// Non-sync literal `{N+}` refused — the announced body bytes and
    /// trailing CRLF have already been drained to keep the stream in
    /// sync (RFC 7888 §4). Caller emits a tagged `BAD` / `NO`.
    NonSyncLiteralRefused { line: Vec<u8>, declared: u64 },
    /// Sync literal `{N}` declared size exceeds the configured cap.
    /// No continuation was sent → client will not send the body, so
    /// the stream remains in sync. Carries the line bytes (with the
    /// marker stripped) so the caller can still recover the tag for
    /// the `BAD`/`NO [TOOBIG]` response.
    SyncLiteralTooLarge {
        line: Vec<u8>,
        declared: u64,
        cap: u64,
    },
    /// Non-sync literal `{N+}` declared size exceeds the configured
    /// cap. The N body bytes and the trailing CRLF have already been
    /// drained from the stream (RFC 7888 §4: "the server SHOULD
    /// process the rest of the command"); the stream is in sync.
    /// Caller emits `NO [TOOBIG]`.
    NonSyncLiteralTooLarge {
        line: Vec<u8>,
        declared: u64,
        cap: u64,
    },
}

impl std::fmt::Display for ReadCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => write!(f, "peer closed"),
            Self::LineTooLong => write!(f, "line exceeded {MAX_LINE_BYTES} bytes"),
            Self::Io(e) => write!(f, "{e}"),
            Self::MalformedLiteral { reason, .. } => write!(f, "malformed literal: {reason}"),
            Self::SyncLiteralTooLarge { declared, cap, .. } => {
                write!(f, "sync literal {declared} exceeds cap {cap}")
            }
            Self::NonSyncLiteralTooLarge { declared, cap, .. } => {
                write!(f, "non-sync literal {declared} exceeds cap {cap}")
            }
            Self::SyncLiteralRefused { declared, .. } => {
                write!(f, "sync literal {declared} refused in current state")
            }
            Self::NonSyncLiteralRefused { declared, .. } => {
                write!(f, "non-sync literal {declared} refused in current state")
            }
        }
    }
}
impl std::error::Error for ReadCommandError {}

/// Parsed literal indicator: declared size + sync/non-sync flag.
#[derive(Debug, Clone, Copy)]
struct LiteralMarker {
    declared: u64,
    non_sync: bool,
}

/// Read one IMAP command, including an optional trailing literal.
///
/// `max_literal` is the inclusive cap on the literal body. For sync
/// literals over the cap, we refuse the continuation (the client
/// won't send the body). For non-sync (`{N+}`) literals over the cap
/// we drain the announced bytes plus the trailing CRLF so the stream
/// stays in sync per RFC 7888 §4.
///
/// `accept_literals` is the session/verb gate: the closure is called
/// with the line bytes *before* the literal marker (so it can sniff
/// the tag and verb) and returns `true` only if the literal body
/// should actually be consumed. Refusal cases:
///
/// - Pre-auth — an attacker would otherwise be able to push
///   `max_literal` bytes (default 64 MiB) into memory before any
///   account-scoped cap applies. The session gates on
///   `state.is_authenticated()`.
/// - Verbs that don't accept literal arguments — APPEND is the only
///   T14 verb that does; everything else is rejected by dispatch
///   anyway, so consuming the body wastes bandwidth + RAM.
///
/// On refusal we refuse the literal at the codec layer: sync → no
/// continuation (returns `SyncLiteralRefused`, stream remains in sync
/// because the client waits for `+ Ready`); non-sync → drain body +
/// trailing CRLF (`NonSyncLiteralRefused`). Caller emits a tagged
/// `BAD`.
///
/// On `Ok`, the returned [`Command::line`] never contains a trailing
/// literal marker; [`parse_line`] sees only the stripped prefix.
pub async fn read_command<R, W, F>(
    reader: &mut BufReader<R>,
    writer: &mut W,
    max_literal: u64,
    accept_literals: F,
) -> Result<Command, ReadCommandError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnOnce(&[u8]) -> bool,
{
    let raw = match read_line(reader).await {
        Ok(l) => l,
        Err(ReadLineError::Eof) => return Err(ReadCommandError::Eof),
        Err(ReadLineError::LineTooLong) => return Err(ReadCommandError::LineTooLong),
        Err(ReadLineError::Io(e)) => return Err(ReadCommandError::Io(e)),
    };

    let (line, marker) = match strip_trailing_literal_marker(&raw) {
        Ok(Some((prefix, m))) => (prefix, m),
        Ok(None) => {
            return Ok(Command {
                line: raw,
                literal: None,
            });
        }
        Err(e) => {
            return Err(ReadCommandError::MalformedLiteral {
                line: raw,
                reason: e,
            });
        }
    };

    // State/verb gate. Refuse before consuming any body bytes / before
    // ack'ing a sync continuation. Cap-check intentionally runs after
    // this, since refusal is the stricter outcome.
    if !accept_literals(&line) {
        if marker.non_sync {
            if let Err(e) = drain_n(reader, marker.declared).await {
                return Err(io_to_err(e));
            }
            match read_line(reader).await {
                Ok(_) => {}
                Err(ReadLineError::Eof) => return Err(ReadCommandError::Eof),
                Err(ReadLineError::LineTooLong) => {
                    return Err(ReadCommandError::LineTooLong);
                }
                Err(ReadLineError::Io(e)) => return Err(ReadCommandError::Io(e)),
            }
            return Err(ReadCommandError::NonSyncLiteralRefused {
                line,
                declared: marker.declared,
            });
        }
        return Err(ReadCommandError::SyncLiteralRefused {
            line,
            declared: marker.declared,
        });
    }

    // Cap check.
    if marker.declared > max_literal {
        if marker.non_sync {
            // Client is already sending the body — drain to keep
            // sync, then consume the command-terminator CRLF that
            // follows. read_line errors here propagate as Io/Eof
            // because the framing has already gone off the rails.
            if let Err(e) = drain_n(reader, marker.declared).await {
                return Err(io_to_err(e));
            }
            // The trailing line MAY contain more args before CRLF.
            // For the cap-rejection path we don't care what's there
            // — we just need to swallow it so the next command lands
            // cleanly. A line-too-long here is rare (the trailing
            // line caps at MAX_LINE_BYTES like any other) but we
            // surface it rather than silently truncating.
            match read_line(reader).await {
                Ok(_) => {}
                Err(ReadLineError::Eof) => return Err(ReadCommandError::Eof),
                Err(ReadLineError::LineTooLong) => {
                    return Err(ReadCommandError::LineTooLong);
                }
                Err(ReadLineError::Io(e)) => return Err(ReadCommandError::Io(e)),
            }
            return Err(ReadCommandError::NonSyncLiteralTooLarge {
                line,
                declared: marker.declared,
                cap: max_literal,
            });
        }
        // Sync literal — refuse continuation, stream stays in sync.
        return Err(ReadCommandError::SyncLiteralTooLarge {
            line,
            declared: marker.declared,
            cap: max_literal,
        });
    }

    // Issue continuation for sync literals only.
    if !marker.non_sync {
        if let Err(e) = writer.write_all(b"+ Ready for literal data\r\n").await {
            return Err(ReadCommandError::Io(e));
        }
        if let Err(e) = writer.flush().await {
            return Err(ReadCommandError::Io(e));
        }
    }

    // Read literal body exactly. usize cast is safe because we already
    // bounded `marker.declared <= max_literal`, and `max_literal` is a
    // u64 derived from `ImapConfig::max_literal_bytes` (64 MiB default,
    // operator-overridable). We still guard against a usize-overflow
    // host (32-bit) — refuse rather than silently truncate.
    let declared_usize = match usize::try_from(marker.declared) {
        Ok(v) => v,
        Err(_) => {
            return Err(ReadCommandError::MalformedLiteral {
                line,
                reason: "literal size exceeds host usize".to_string(),
            });
        }
    };
    let mut body = vec![0u8; declared_usize];
    if let Err(e) = reader.read_exact(&mut body).await {
        if e.kind() == ErrorKind::UnexpectedEof {
            return Err(ReadCommandError::Eof);
        }
        return Err(ReadCommandError::Io(e));
    }

    // After the literal body, the command continues. T14 only supports
    // the "literal is the final argument" shape — the trailing line
    // must be empty (just CRLF). Anything else is treated as malformed
    // for now; a future imap-codec swap accepts mid-command literals.
    let trailing = match read_line(reader).await {
        Ok(t) => t,
        Err(ReadLineError::Eof) => return Err(ReadCommandError::Eof),
        Err(ReadLineError::LineTooLong) => return Err(ReadCommandError::LineTooLong),
        Err(ReadLineError::Io(e)) => return Err(ReadCommandError::Io(e)),
    };
    if !trailing.is_empty() {
        return Err(ReadCommandError::MalformedLiteral {
            line,
            reason: "data after literal body; mid-command literals not supported in this phase"
                .to_string(),
        });
    }

    Ok(Command {
        line,
        literal: Some(body),
    })
}

/// Walk back through `line` looking for a trailing `{N}` / `{N+}`
/// indicator. Returns `Ok(Some((prefix, marker)))` on a syntactically
/// valid marker, `Ok(None)` if the line has none, or `Err(reason)` if
/// the indicator is structurally malformed (non-numeric, overflow,
/// missing `}`, etc.).
fn strip_trailing_literal_marker(line: &[u8]) -> Result<Option<(Vec<u8>, LiteralMarker)>, String> {
    // Trim trailing spaces (`read_line` already stripped the CR).
    let mut end = line.len();
    while end > 0 && line[end - 1] == b' ' {
        end -= 1;
    }
    if end == 0 || line[end - 1] != b'}' {
        return Ok(None);
    }
    let close = end - 1; // index of '}'
    // Walk back to matching `{`.
    let mut open = close;
    while open > 0 && line[open] != b'{' {
        open -= 1;
    }
    if line[open] != b'{' {
        // `}` with no `{` before it — not a literal indicator.
        return Ok(None);
    }
    let inner = &line[open + 1..close];
    if inner.is_empty() {
        return Err("empty literal marker `{}`".to_string());
    }
    let (digits, non_sync) = if inner.last() == Some(&b'+') {
        (&inner[..inner.len() - 1], true)
    } else {
        (inner, false)
    };
    if digits.is_empty() {
        return Err("missing digits in literal marker".to_string());
    }
    if digits.iter().any(|b| !b.is_ascii_digit()) {
        return Err("non-digit byte in literal marker".to_string());
    }
    let digits_str = std::str::from_utf8(digits).expect("ascii digits");
    let declared: u64 = digits_str
        .parse()
        .map_err(|_| "literal size overflows u64".to_string())?;
    // Trim the marker plus a single whitespace gap so `parse_line`
    // sees a clean tail (e.g. `a001 APPEND INBOX {123}` → `a001 APPEND INBOX`).
    let mut prefix_end = open;
    while prefix_end > 0 && line[prefix_end - 1] == b' ' {
        prefix_end -= 1;
    }
    Ok(Some((
        line[..prefix_end].to_vec(),
        LiteralMarker { declared, non_sync },
    )))
}

/// Drain exactly `n` bytes from the reader and discard them.
async fn drain_n<R: AsyncRead + Unpin>(reader: &mut BufReader<R>, n: u64) -> std::io::Result<()> {
    let mut remaining = n;
    let mut buf = [0u8; 4096];
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        reader.read_exact(&mut buf[..take]).await?;
        remaining -= take as u64;
    }
    Ok(())
}

fn io_to_err(e: std::io::Error) -> ReadCommandError {
    if e.kind() == ErrorKind::UnexpectedEof {
        ReadCommandError::Eof
    } else {
        ReadCommandError::Io(e)
    }
}

/// Detect a trailing IMAP literal indicator `{NNN}` or `{NNN+}`.
/// We accept the line so the dispatcher can emit a clean `BAD`.
fn has_unsupported_literal(line: &[u8]) -> bool {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\r' || line[end - 1] == b' ') {
        end -= 1;
    }
    if end == 0 || line[end - 1] != b'}' {
        return false;
    }
    // Walk back to matching `{`.
    let mut i = end - 1;
    while i > 0 && line[i] != b'{' {
        i -= 1;
    }
    line[i] == b'{'
}

fn is_valid_tag(tag: &str) -> bool {
    // RFC 9051 tag = 1*<any ASTRING-CHAR except "+">. Phase 1 enforces
    // the practical subset: printable ASCII, no whitespace, no '+'.
    tag.chars().all(|c| {
        c.is_ascii_graphic() && c != '+' && c != '(' && c != ')' && c != '{' && c != '%' && c != '*'
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_noop() {
        let l = parse_line(b"a001 NOOP").unwrap();
        assert_eq!(l.tag, "a001");
        assert_eq!(l.verb_upper, "NOOP");
        assert_eq!(l.args, "");
    }

    #[test]
    fn parses_login_with_args() {
        let l = parse_line(b"a002 LOGIN alice@example.com hunter2").unwrap();
        assert_eq!(l.tag, "a002");
        assert_eq!(l.verb_upper, "LOGIN");
        assert_eq!(l.args, "alice@example.com hunter2");
    }

    #[test]
    fn uppercases_verb() {
        let l = parse_line(b"a003 capability").unwrap();
        assert_eq!(l.verb_upper, "CAPABILITY");
    }

    #[test]
    fn rejects_literal() {
        let err = parse_line(b"a004 LOGIN alice {7}").unwrap_err();
        assert!(err.to_string().contains("literal"));
    }

    #[test]
    fn rejects_literal_with_plus() {
        let err = parse_line(b"a005 LOGIN alice {7+}").unwrap_err();
        assert!(err.to_string().contains("literal"));
    }

    #[test]
    fn rejects_nul() {
        assert!(parse_line(b"a006 NOOP\0").is_err());
    }

    #[test]
    fn rejects_empty_tag() {
        assert!(parse_line(b" NOOP").is_err());
    }

    #[test]
    fn rejects_plus_in_tag() {
        assert!(parse_line(b"+ NOOP").is_err());
    }

    // ----- strip_trailing_literal_marker --------------------------

    #[test]
    fn strip_marker_simple_sync() {
        let (prefix, m) = strip_trailing_literal_marker(b"a001 APPEND INBOX {7}")
            .unwrap()
            .unwrap();
        assert_eq!(prefix, b"a001 APPEND INBOX");
        assert_eq!(m.declared, 7);
        assert!(!m.non_sync);
    }

    #[test]
    fn strip_marker_non_sync() {
        let (prefix, m) = strip_trailing_literal_marker(b"a002 APPEND INBOX {0+}")
            .unwrap()
            .unwrap();
        assert_eq!(prefix, b"a002 APPEND INBOX");
        assert_eq!(m.declared, 0);
        assert!(m.non_sync);
    }

    #[test]
    fn strip_marker_trims_trailing_spaces() {
        let (prefix, m) = strip_trailing_literal_marker(b"a003 APPEND INBOX {12}  ")
            .unwrap()
            .unwrap();
        assert_eq!(prefix, b"a003 APPEND INBOX");
        assert_eq!(m.declared, 12);
    }

    #[test]
    fn strip_marker_none_when_no_brace() {
        assert!(
            strip_trailing_literal_marker(b"a001 NOOP")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn strip_marker_none_when_close_without_open() {
        // Trailing `}` but no `{` before it — must not be misparsed.
        assert!(
            strip_trailing_literal_marker(b"a001 SOMETHING )}")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn strip_marker_rejects_empty_body() {
        let e = strip_trailing_literal_marker(b"a001 LOGIN {}").unwrap_err();
        assert!(e.contains("empty"), "{e}");
    }

    #[test]
    fn strip_marker_rejects_non_digit() {
        let e = strip_trailing_literal_marker(b"a001 APPEND {abc}").unwrap_err();
        assert!(e.contains("non-digit"), "{e}");
    }

    #[test]
    fn strip_marker_rejects_double_plus() {
        // `{1++}` — the trailing `+` is the non-sync marker, but the
        // remaining `1+` is not a clean digit-only body.
        let e = strip_trailing_literal_marker(b"a001 APPEND {1++}").unwrap_err();
        assert!(e.contains("non-digit"), "{e}");
    }

    #[test]
    fn strip_marker_rejects_overflow() {
        // 21 digits — well past u64 max (20 digits).
        let e = strip_trailing_literal_marker(b"a001 APPEND {999999999999999999999}").unwrap_err();
        assert!(e.contains("overflow"), "{e}");
    }

    #[test]
    fn strip_marker_accepts_zero_sync() {
        let (_, m) = strip_trailing_literal_marker(b"a001 APPEND {0}")
            .unwrap()
            .unwrap();
        assert_eq!(m.declared, 0);
        assert!(!m.non_sync);
    }

    // ----- read_command ------------------------------------------

    #[tokio::test]
    async fn read_command_no_literal() {
        let input: &[u8] = b"a001 NOOP\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let cmd = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap();
        assert_eq!(cmd.line, b"a001 NOOP");
        assert!(cmd.literal.is_none());
        assert!(writer.is_empty(), "no continuation expected");
    }

    #[tokio::test]
    async fn read_command_sync_literal_round_trip() {
        let input: &[u8] = b"a001 APPEND INBOX {7}\r\nhunter2\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let cmd = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap();
        assert_eq!(cmd.line, b"a001 APPEND INBOX");
        assert_eq!(cmd.literal.as_deref(), Some(b"hunter2".as_ref()));
        assert_eq!(writer, b"+ Ready for literal data\r\n");
    }

    #[tokio::test]
    async fn read_command_non_sync_literal_no_continuation() {
        let input: &[u8] = b"a002 APPEND INBOX {5+}\r\nhello\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let cmd = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap();
        assert_eq!(cmd.line, b"a002 APPEND INBOX");
        assert_eq!(cmd.literal.as_deref(), Some(b"hello".as_ref()));
        assert!(writer.is_empty(), "non-sync must not emit continuation");
    }

    #[tokio::test]
    async fn read_command_zero_byte_sync_literal() {
        let input: &[u8] = b"a003 APPEND INBOX {0}\r\n\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let cmd = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap();
        assert_eq!(cmd.literal.as_deref(), Some(b"".as_ref()));
        assert_eq!(writer, b"+ Ready for literal data\r\n");
    }

    #[tokio::test]
    async fn read_command_literal_with_embedded_crlf_and_8bit() {
        let body: &[u8] = b"line1\r\nline2\r\n\x80\x81\x00trailing";
        let mut input = Vec::new();
        input.extend_from_slice(format!("a004 APPEND INBOX {{{}+}}\r\n", body.len()).as_bytes());
        input.extend_from_slice(body);
        input.extend_from_slice(b"\r\n");
        let mut reader = BufReader::new(&input[..]);
        let mut writer: Vec<u8> = Vec::new();
        let cmd = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap();
        assert_eq!(cmd.literal.as_deref(), Some(body));
    }

    #[tokio::test]
    async fn read_command_literal_split_across_reads() {
        // Use a chunk-limited reader to verify read_exact handles
        // partial reads correctly. Tokio's BufReader on a `&[u8]`
        // already exercises this when the buffer is smaller than the
        // request — force it by capping the inner buffer.
        let input: &[u8] = b"a005 APPEND INBOX {10+}\r\n0123456789\r\n";
        let mut reader = BufReader::with_capacity(4, input);
        let mut writer: Vec<u8> = Vec::new();
        let cmd = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap();
        assert_eq!(cmd.literal.as_deref(), Some(b"0123456789".as_ref()));
    }

    #[tokio::test]
    async fn read_command_sync_literal_too_large_no_continuation() {
        let input: &[u8] = b"a006 APPEND INBOX {1000}\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 100, |_| true)
            .await
            .unwrap_err();
        match err {
            ReadCommandError::SyncLiteralTooLarge {
                declared,
                cap,
                line,
            } => {
                assert_eq!(declared, 1000);
                assert_eq!(cap, 100);
                assert_eq!(line, b"a006 APPEND INBOX");
            }
            other => panic!("expected SyncLiteralTooLarge, got {other:?}"),
        }
        assert!(
            writer.is_empty(),
            "no continuation for oversized sync literal"
        );
    }

    #[tokio::test]
    async fn read_command_non_sync_too_large_drains_body() {
        let mut input = Vec::new();
        input.extend_from_slice(b"a007 APPEND INBOX {1000+}\r\n");
        input.extend_from_slice(&vec![b'X'; 1000]);
        input.extend_from_slice(b"\r\n");
        // Append a follow-up command to verify stream sync recovery.
        input.extend_from_slice(b"a008 NOOP\r\n");
        let mut reader = BufReader::new(&input[..]);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 100, |_| true)
            .await
            .unwrap_err();
        match err {
            ReadCommandError::NonSyncLiteralTooLarge { declared, cap, .. } => {
                assert_eq!(declared, 1000);
                assert_eq!(cap, 100);
            }
            other => panic!("expected NonSyncLiteralTooLarge, got {other:?}"),
        }
        // Stream must still be in sync — next read returns `a008 NOOP`.
        let next = read_command(&mut reader, &mut writer, 100, |_| true)
            .await
            .unwrap();
        assert_eq!(next.line, b"a008 NOOP");
    }

    #[tokio::test]
    async fn read_command_malformed_marker_no_continuation() {
        let input: &[u8] = b"a009 APPEND INBOX {abc}\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap_err();
        match err {
            ReadCommandError::MalformedLiteral { line, .. } => {
                assert_eq!(line, b"a009 APPEND INBOX {abc}");
            }
            other => panic!("expected MalformedLiteral, got {other:?}"),
        }
        assert!(writer.is_empty());
    }

    #[tokio::test]
    async fn read_command_eof_mid_literal() {
        let input: &[u8] = b"a010 APPEND INBOX {10+}\r\nfoo"; // only 3 bytes
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap_err();
        assert!(matches!(err, ReadCommandError::Eof), "got {err:?}");
    }

    #[tokio::test]
    async fn read_command_data_after_literal_body_rejected() {
        // Mid-command literal: APPEND with literal followed by another
        // argument. T14 only supports the "literal is final" shape.
        let input: &[u8] = b"a011 LOGIN {5+}\r\nalice hunter2\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap_err();
        match err {
            ReadCommandError::MalformedLiteral { reason, line } => {
                assert!(reason.contains("mid-command"), "{reason}");
                assert_eq!(line, b"a011 LOGIN");
            }
            other => panic!("expected MalformedLiteral, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_command_refuses_sync_literal_when_disabled() {
        // accept_literals=false (e.g. pre-auth) — sync literal must
        // be refused before any continuation is sent. Stream stays in
        // sync because the client is waiting for `+ Ready`.
        let input: &[u8] = b"a012 APPEND INBOX {7}\r\n";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 1024, |_| false)
            .await
            .unwrap_err();
        match err {
            ReadCommandError::SyncLiteralRefused { line, declared } => {
                assert_eq!(declared, 7);
                assert_eq!(line, b"a012 APPEND INBOX");
            }
            other => panic!("expected SyncLiteralRefused, got {other:?}"),
        }
        assert!(writer.is_empty(), "no continuation expected");
    }

    #[tokio::test]
    async fn read_command_refuses_non_sync_literal_when_disabled() {
        // accept_literals=false — non-sync body must be drained so
        // the stream stays in sync for the next command.
        let mut input = Vec::new();
        input.extend_from_slice(b"a013 APPEND INBOX {7+}\r\n");
        input.extend_from_slice(b"hunter2");
        input.extend_from_slice(b"\r\n");
        input.extend_from_slice(b"a014 NOOP\r\n");
        let mut reader = BufReader::new(&input[..]);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 1024, |_| false)
            .await
            .unwrap_err();
        match err {
            ReadCommandError::NonSyncLiteralRefused { line, declared } => {
                assert_eq!(declared, 7);
                assert_eq!(line, b"a013 APPEND INBOX");
            }
            other => panic!("expected NonSyncLiteralRefused, got {other:?}"),
        }
        // Stream sync recovery — next command parses cleanly.
        let next = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap();
        assert_eq!(next.line, b"a014 NOOP");
    }

    #[tokio::test]
    async fn read_command_eof_clean() {
        let input: &[u8] = b"";
        let mut reader = BufReader::new(input);
        let mut writer: Vec<u8> = Vec::new();
        let err = read_command(&mut reader, &mut writer, 1024, |_| true)
            .await
            .unwrap_err();
        assert!(matches!(err, ReadCommandError::Eof));
    }
}
