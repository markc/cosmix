use std::fmt;

#[derive(Debug, Clone)]
pub struct Span {
    pub line: usize,
    pub column: usize,
    pub file: Option<String>,
}

/// Whether a traceback frame is a Mix function or a builtin call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Script,
    Builtin,
}

impl FrameKind {
    pub fn wire_name(self) -> &'static str {
        match self {
            FrameKind::Script => "script",
            FrameKind::Builtin => "builtin",
        }
    }
}

/// One traceback frame. Stored outermost-to-innermost; the final frame
/// is the failure site. `line` is the source line *within* this frame
/// where the next call (or the failure itself) happened — the Python
/// convention. `column` is always `None` at runtime today (statements
/// carry line precision only).
#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: FrameKind,
    pub function: String,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

/// Structured payload for code-carrying runtime errors (0.29.0).
///
/// `code` is a stable uppercase identifier (`SSH_TIMEOUT`,
/// `VALIDATION_REQUIRED`, ...; shape checked by [`is_valid_error_code`]).
/// Legacy errors surface as `RUNTIME_ERROR` / `USER_DIE`. `details` is a
/// `Value::Map` (or `Nil`) of operation-specific data; `frames` is the
/// call traceback snapshotted at the first function-boundary the error
/// crossed. Scripts see this as the optional second `catch` binding.
#[derive(Debug, Clone)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
    pub details: crate::value::Value,
    pub cause: Option<Box<ErrorInfo>>,
    pub frames: Vec<Frame>,
    pub span: Option<Span>,
}

impl ErrorInfo {
    /// A bare code+message error with no details/frames yet (frames are
    /// filled in by the evaluator when the error first crosses a
    /// function boundary — see `Evaluator::snapshot_error`).
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        ErrorInfo {
            code: code.into(),
            message: message.into(),
            details: crate::value::Value::Nil,
            cause: None,
            frames: Vec::new(),
            span: None,
        }
    }

    pub fn with_details(mut self, details: crate::value::Value) -> Self {
        self.details = details;
        self
    }

    /// The script-visible error map bound to the optional second
    /// `catch` variable: `{code, message, details, cause, frames}`,
    /// frames as `[{kind, function, file, line, column}]`
    /// outermost-to-innermost.
    pub fn to_value(&self) -> crate::value::Value {
        use crate::value::Value;
        let mut m = indexmap::IndexMap::new();
        m.insert("code".to_string(), Value::String(self.code.clone()));
        m.insert("message".to_string(), Value::String(self.message.clone()));
        m.insert("details".to_string(), self.details.clone());
        m.insert(
            "cause".to_string(),
            self.cause.as_ref().map_or(Value::Nil, |c| c.to_value()),
        );
        let frames = self
            .frames
            .iter()
            .map(|f| {
                let mut fm = indexmap::IndexMap::new();
                fm.insert(
                    "kind".to_string(),
                    Value::String(f.kind.wire_name().to_string()),
                );
                fm.insert("function".to_string(), Value::String(f.function.clone()));
                fm.insert(
                    "file".to_string(),
                    f.file
                        .as_ref()
                        .map_or(Value::Nil, |p| Value::String(p.clone())),
                );
                fm.insert(
                    "line".to_string(),
                    f.line.map_or(Value::Nil, |l| Value::Number(l as f64)),
                );
                fm.insert(
                    "column".to_string(),
                    f.column.map_or(Value::Nil, |c| Value::Number(c as f64)),
                );
                Value::map(fm)
            })
            .collect();
        m.insert("frames".to_string(), Value::list(frames));
        Value::map(m)
    }
}

/// Error-code shape from the D6 contract:
/// `[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*`. Scripts may raise their own codes;
/// codes are stable identifiers, not authority claims.
pub fn is_valid_error_code(code: &str) -> bool {
    if code.is_empty() {
        return false;
    }
    let mut prev_underscore = true; // segment start
    for (i, c) in code.chars().enumerate() {
        match c {
            'A'..='Z' => prev_underscore = false,
            '0'..='9' => {
                if i == 0 {
                    return false;
                }
                prev_underscore = false;
            }
            '_' => {
                if prev_underscore {
                    return false; // leading/double underscore
                }
                prev_underscore = true;
            }
            _ => return false,
        }
    }
    !prev_underscore // no trailing underscore
}

#[derive(Debug, Clone)]
pub enum MixError {
    LexerError {
        msg: String,
        span: Span,
    },
    ParseError {
        msg: String,
        span: Span,
    },
    /// The parser consumed a construct that can become valid only if more
    /// source is appended. Interactive callers use this typed signal to retain
    /// their input buffer; file and one-shot callers still render it as an
    /// ordinary parse error.
    IncompleteInput {
        msg: String,
        span: Span,
    },
    /// A parser rejection for an assignment used as any operand of a
    /// statement-level `&&` / `||` chain. Kept distinct from free-form parse
    /// prose so shell classifiers can recognise it structurally before their
    /// command-list fallback.
    AssignmentChainParseError {
        operator: String,
        span: Span,
    },
    /// Source contained a construct that strict-data mode rejects
    /// (variable reference, function call, IO, control flow, etc.).
    /// `construct` names the offending production; `hint` tells the
    /// author what's allowed instead. The companion of strict-data
    /// parsing — see `parse_data` in lib.rs.
    StrictDataViolation {
        construct: String,
        line: usize,
        hint: String,
    },
    /// A `Value` could not be serialised as strict-data Mix source —
    /// e.g. a non-finite `Value::Number(f64)`. The serializer rejects
    /// rather than emitting output that wouldn't round-trip through
    /// `parse_data`. See `Value::write_mix_data`.
    DataSerializeError {
        msg: String,
    },
    RuntimeError {
        msg: String,
        span: Option<Span>,
    },
    DieError {
        msg: String,
    },
    /// Code-carrying structured error (0.29.0) — see [`ErrorInfo`].
    /// Caught by `try/catch` exactly like `RuntimeError`; the message
    /// string binds to the first catch variable, the structured payload
    /// to the optional second. Legacy `RuntimeError`/`DieError` are
    /// converted to this (codes `RUNTIME_ERROR`/`USER_DIE`) when the
    /// evaluator snapshots a traceback.
    Structured(Box<ErrorInfo>),
    Return {
        value: crate::value::Value,
    },
    Break(Option<String>),
    Continue(Option<String>),
    /// Uncatchable process-exit control flow. The evaluator propagates this
    /// through functions, loops, and `finally`; binary entrypoints consume it
    /// and terminate with the requested status.
    ExitRequest {
        code: i32,
    },
}

impl fmt::Display for MixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MixError::LexerError { msg, span } => {
                if let Some(file) = &span.file {
                    write!(
                        f,
                        "Lexer error at {}:{}:{}: {}",
                        file, span.line, span.column, msg
                    )
                } else {
                    write!(
                        f,
                        "Lexer error at line {}:{}: {}",
                        span.line, span.column, msg
                    )
                }
            }
            MixError::ParseError { msg, span } | MixError::IncompleteInput { msg, span } => {
                if let Some(file) = &span.file {
                    write!(
                        f,
                        "Parse error at {}:{}:{}: {}",
                        file, span.line, span.column, msg
                    )
                } else {
                    write!(
                        f,
                        "Parse error at line {}:{}: {}",
                        span.line, span.column, msg
                    )
                }
            }
            MixError::AssignmentChainParseError { operator, span } => {
                let msg = assignment_chain_parse_message(operator);
                if let Some(file) = &span.file {
                    write!(
                        f,
                        "Parse error at {}:{}:{}: {}",
                        file, span.line, span.column, msg
                    )
                } else {
                    write!(
                        f,
                        "Parse error at line {}:{}: {}",
                        span.line, span.column, msg
                    )
                }
            }
            MixError::StrictDataViolation {
                construct,
                line,
                hint,
            } => {
                write!(
                    f,
                    "Strict-data violation at line {}: {} not allowed in data files. {}",
                    line, construct, hint
                )
            }
            MixError::DataSerializeError { msg } => {
                write!(f, "Data serialize error: {}", msg)
            }
            MixError::RuntimeError { msg, span } => {
                if let Some(span) = span {
                    if let Some(file) = &span.file {
                        write!(f, "Runtime error at {}:{}: {}", file, span.line, msg)
                    } else {
                        write!(f, "Runtime error at line {}: {}", span.line, msg)
                    }
                } else {
                    write!(f, "Runtime error: {}", msg)
                }
            }
            MixError::DieError { msg } => write!(f, "{}", msg),
            // Structured errors keep the legacy single-line rendering for
            // compatibility (`--no-traceback` consumers, log scrapers):
            // USER_DIE renders like DieError (bare message), everything
            // else like RuntimeError. The traceback rendering lives in
            // `MixError::render_traceback`, used by the CLI for uncaught
            // errors unless --no-traceback.
            MixError::Structured(info) => {
                if info.code == "USER_DIE" {
                    write!(f, "{}", info.message)
                } else if let Some(span) = &info.span {
                    if let Some(file) = &span.file {
                        write!(
                            f,
                            "Runtime error at {}:{}: {}",
                            file, span.line, info.message
                        )
                    } else {
                        write!(f, "Runtime error at line {}: {}", span.line, info.message)
                    }
                } else {
                    write!(f, "Runtime error: {}", info.message)
                }
            }
            MixError::Return { .. } => write!(f, "unexpected return outside function"),
            MixError::Break(_) => write!(f, "unexpected break outside loop"),
            MixError::Continue(_) => write!(f, "unexpected continue outside loop"),
            MixError::ExitRequest { code } => write!(f, "exit requested with code {code}"),
        }
    }
}

impl MixError {
    pub fn assignment_chain_parse(operator: &str, span: Span) -> Self {
        Self::AssignmentChainParseError {
            operator: operator.to_string(),
            span,
        }
    }

    pub fn is_assignment_chain_parse_error(&self) -> bool {
        matches!(self, Self::AssignmentChainParseError { .. })
    }

    /// True only for a parser-produced incomplete-input signal. Callers must
    /// not infer this state from diagnostic prose.
    pub fn is_incomplete_input(&self) -> bool {
        matches!(self, Self::IncompleteInput { .. })
    }

    /// Convenience constructor for a code-carrying structured error
    /// with no details/frames/span yet — the evaluator fills span and
    /// traceback at its boundaries (`attach_span` / `snapshot_error`).
    pub fn structured(code: &str, msg: impl Into<String>) -> MixError {
        MixError::Structured(Box::new(ErrorInfo::new(code, msg)))
    }

    /// The D6 uncaught-error rendering: a `Traceback (most recent call
    /// last):` block (when frames exist) followed by `CODE: message`.
    /// Non-structured errors (and structured errors with no frames)
    /// fall back to the legacy single-line `Display` rendering — the
    /// same output `--no-traceback` forces.
    pub fn render_traceback(&self) -> String {
        let MixError::Structured(info) = self else {
            return self.to_string();
        };
        if info.frames.is_empty() {
            return self.to_string();
        }
        let mut out = String::from("Traceback (most recent call last):\n");
        // Deep stacks (e.g. a recursion-limit error carries one frame per
        // call) elide the middle: first 10, marker, last 10. The full
        // frame list is still available to scripts via `catch $m, $e`.
        const HEAD: usize = 10;
        const TAIL: usize = 10;
        let elide = info.frames.len() > HEAD + TAIL + 4;
        for (i, frame) in info.frames.iter().enumerate() {
            if elide && i == HEAD {
                out.push_str(&format!(
                    "  ... ({} frames elided)\n",
                    info.frames.len() - HEAD - TAIL
                ));
            }
            if elide && i >= HEAD && i < info.frames.len() - TAIL {
                continue;
            }
            out.push_str("  at ");
            match frame.kind {
                FrameKind::Builtin => {
                    out.push_str("<builtin:");
                    out.push_str(&frame.function);
                    out.push('>');
                }
                FrameKind::Script => out.push_str(&frame.function),
            }
            match (&frame.file, frame.line) {
                (Some(file), Some(line)) => {
                    out.push_str(&format!(" ({file}:{line}"));
                    if let Some(col) = frame.column {
                        out.push_str(&format!(":{col}"));
                    }
                    out.push(')');
                }
                (None, Some(line)) => out.push_str(&format!(" (line {line})")),
                (Some(file), None) => out.push_str(&format!(" ({file})")),
                (None, None) => {}
            }
            out.push('\n');
        }
        out.push_str(&format!("{}: {}", info.code, info.message));
        out
    }

    /// The structured payload, when this error carries one.
    pub fn info(&self) -> Option<&ErrorInfo> {
        match self {
            MixError::Structured(info) => Some(info),
            _ => None,
        }
    }

    /// The `(file, line)` where this error was raised — the failure site an
    /// error-line display anchors to. Prefers the innermost traceback frame
    /// (frames are outermost-to-innermost, so the last frame with a line is
    /// the failure site), falling back to the structured span, then the
    /// legacy `RuntimeError` span. `file` is `None` for a `-c` body (stdin
    /// `mix -` is keyed `Some("-")`). Returns `None` when the error carries no
    /// position at all.
    pub fn error_site(&self) -> Option<(Option<String>, usize)> {
        if let Some(info) = self.info() {
            if let Some(frame) = info.frames.iter().rev().find(|f| f.line.is_some()) {
                return Some((frame.file.clone(), frame.line.unwrap()));
            }
            if let Some(span) = &info.span {
                return Some((span.file.clone(), span.line));
            }
            return None;
        }
        if let MixError::RuntimeError {
            span: Some(span), ..
        } = self
        {
            return Some((span.file.clone(), span.line));
        }
        None
    }
}

pub fn assignment_chain_parse_message(operator: &str) -> String {
    format!(
        "assignment cannot be chained with `{operator}`; use `and`/`or` inside the assigned expression for logical values, or split this into two statements if you meant shell-style command chaining"
    )
}

impl std::error::Error for MixError {}

pub type MixResult<T> = Result<T, MixError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_shape() {
        for good in [
            "SSH_TIMEOUT",
            "RUNTIME_ERROR",
            "A",
            "E1",
            "PROCESS_EXIT_NONZERO",
            "HTTP_TLS_PIN",
        ] {
            assert!(is_valid_error_code(good), "{good} should be valid");
        }
        for bad in [
            "",
            "ssh_timeout",
            "_SSH",
            "SSH_",
            "SSH__TIMEOUT",
            "1SSH",
            "SSH-TIMEOUT",
            "Ssh_Timeout",
            "SSH TIMEOUT",
        ] {
            assert!(!is_valid_error_code(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn structured_display_matches_legacy_shapes() {
        let plain = MixError::Structured(Box::new(ErrorInfo::new(
            "SSH_TIMEOUT",
            "remote operation exceeded 60 seconds",
        )));
        assert_eq!(
            plain.to_string(),
            "Runtime error: remote operation exceeded 60 seconds"
        );

        let mut die = ErrorInfo::new("USER_DIE", "boom");
        die.span = Some(Span {
            line: 3,
            column: 0,
            file: None,
        });
        assert_eq!(MixError::Structured(Box::new(die)).to_string(), "boom");
    }

    #[test]
    fn traceback_rendering() {
        let mut info = ErrorInfo::new("SSH_TIMEOUT", "remote operation exceeded 60 seconds");
        info.frames = vec![
            Frame {
                kind: FrameKind::Script,
                function: "<main>".into(),
                file: Some("/path/job.mix".into()),
                line: Some(20),
                column: None,
            },
            Frame {
                kind: FrameKind::Script,
                function: "provision".into(),
                file: Some("/path/job.mix".into()),
                line: Some(84),
                column: None,
            },
            Frame {
                kind: FrameKind::Builtin,
                function: "ssh_must".into(),
                file: Some("/path/job.mix".into()),
                line: Some(91),
                column: None,
            },
        ];
        let rendered = MixError::Structured(Box::new(info)).render_traceback();
        assert_eq!(
            rendered,
            "Traceback (most recent call last):\n  at <main> (/path/job.mix:20)\n  at provision (/path/job.mix:84)\n  at <builtin:ssh_must> (/path/job.mix:91)\nSSH_TIMEOUT: remote operation exceeded 60 seconds"
        );
    }
}
