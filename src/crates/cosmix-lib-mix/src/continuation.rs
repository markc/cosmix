//! Explicit `\` physical-line continuation shared by the Mix lexer and the
//! shell-facing logical-line assembler.
//!
//! Detection is deliberately lexical-context aware. A trailing backslash in a
//! Mix string or heredoc body is data, while an odd trailing run in ordinary
//! source removes its final backslash plus the following newline. The lexer
//! consumes the recorded sites from the original source so physical line
//! numbers remain accurate; shell callers splice the same sites before they
//! classify or execute a command.

use std::borrow::Cow;

use crate::error::{MixError, MixResult, Span};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ContinuationSite {
    pub(crate) backslash: usize,
    pub(crate) newline_chars: usize,
}

pub(crate) struct SplicedContinuations<'a> {
    pub(crate) source: Cow<'a, str>,
    /// One-based logical line numbers containing at least one splice.
    pub(crate) continued_lines: Vec<usize>,
}

#[derive(Debug)]
enum State {
    Normal,
    SingleQuoted,
    DoubleQuoted,
    LineComment,
    Heredoc(String),
}

/// Locate explicit continuation sites as character offsets into `source`.
///
/// An odd trailing run continues; the final backslash is the marker and any
/// preceding even pairs remain literal. A marker at EOF is typed incomplete
/// input so interactive callers retain their accumulator without matching
/// diagnostic prose.
pub(crate) fn continuation_sites(source: &str) -> MixResult<Vec<ContinuationSite>> {
    let chars: Vec<char> = source.chars().collect();
    let mut sites = Vec::new();
    let mut state = State::Normal;
    let mut pending_heredoc: Option<String> = None;
    let mut i = 0;
    let mut line = 1;
    let mut column = 1;

    while i < chars.len() {
        if let State::Heredoc(tag) = &state {
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            let body_line: String = chars[start..i]
                .iter()
                .collect::<String>()
                .trim_end_matches('\r')
                .to_string();
            let closes = body_line.trim() == tag;
            column += i - start;
            if i < chars.len() {
                i += 1;
                line += 1;
                column = 1;
            }
            if closes {
                state = State::Normal;
            }
            continue;
        }

        match state {
            State::Normal => match chars[i] {
                '\'' => {
                    state = State::SingleQuoted;
                    i += 1;
                    column += 1;
                }
                '"' => {
                    state = State::DoubleQuoted;
                    i += 1;
                    column += 1;
                }
                '#' => {
                    state = State::LineComment;
                    i += 1;
                    column += 1;
                }
                '-' if chars.get(i + 1) == Some(&'-') => {
                    state = State::LineComment;
                    i += 2;
                    column += 2;
                }
                '<' if chars.get(i + 1) == Some(&'<') => {
                    let mut end = i + 2;
                    while chars
                        .get(end)
                        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                    {
                        end += 1;
                    }
                    if end > i + 2 {
                        pending_heredoc = Some(chars[i + 2..end].iter().collect());
                    }
                    i += 2;
                    column += 2;
                }
                '\\' => {
                    let run_start = i;
                    while chars.get(i) == Some(&'\\') {
                        i += 1;
                    }
                    let run_len = i - run_start;
                    let newline_chars = if chars.get(i) == Some(&'\n') {
                        Some(1)
                    } else if chars.get(i) == Some(&'\r') && chars.get(i + 1) == Some(&'\n') {
                        Some(2)
                    } else {
                        None
                    };

                    if run_len % 2 == 1 {
                        if let Some(newline_chars) = newline_chars {
                            sites.push(ContinuationSite {
                                backslash: i - 1,
                                newline_chars,
                            });
                            i += newline_chars;
                            line += 1;
                            column = 1;
                            continue;
                        }
                        if i == chars.len() {
                            return Err(MixError::IncompleteInput {
                                msg: "expected another physical line after trailing `\\`"
                                    .to_string(),
                                span: Span {
                                    line,
                                    column: column + run_len - 1,
                                    file: None,
                                },
                            });
                        }
                    }
                    column += run_len;
                }
                '\n' => {
                    i += 1;
                    line += 1;
                    column = 1;
                    if let Some(tag) = pending_heredoc.take() {
                        state = State::Heredoc(tag);
                    }
                }
                _ => {
                    i += 1;
                    column += 1;
                }
            },
            State::SingleQuoted | State::DoubleQuoted => {
                let quote = if matches!(state, State::SingleQuoted) {
                    '\''
                } else {
                    '"'
                };
                match chars[i] {
                    c if c == quote => {
                        state = State::Normal;
                        i += 1;
                        column += 1;
                    }
                    '\\' => {
                        i += 1;
                        column += 1;
                        if let Some(next) = chars.get(i) {
                            if *next == '\n' {
                                line += 1;
                                column = 1;
                            } else {
                                column += 1;
                            }
                            i += 1;
                        }
                    }
                    '\n' => {
                        i += 1;
                        line += 1;
                        column = 1;
                    }
                    _ => {
                        i += 1;
                        column += 1;
                    }
                }
            }
            State::LineComment => {
                if chars[i] == '\n' {
                    state = State::Normal;
                    i += 1;
                    line += 1;
                    column = 1;
                    if let Some(tag) = pending_heredoc.take() {
                        state = State::Heredoc(tag);
                    }
                } else {
                    i += 1;
                    column += 1;
                }
            }
            State::Heredoc(_) => unreachable!("heredoc handled above"),
        }
    }

    Ok(sites)
}

/// Remove every explicit continuation marker and its physical newline.
///
/// The returned source is the logical input that must be classified and, for
/// shell dispatch, executed. No allocation occurs when no continuation exists.
pub(crate) fn splice_with_metadata(source: &str) -> MixResult<SplicedContinuations<'_>> {
    let sites = continuation_sites(source)?;
    if sites.is_empty() {
        return Ok(SplicedContinuations {
            source: Cow::Borrowed(source),
            continued_lines: Vec::new(),
        });
    }

    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut site_idx = 0;
    let mut logical_line = 1;
    let mut continued_lines = Vec::new();
    while i < chars.len() {
        if let Some(site) = sites.get(site_idx)
            && i == site.backslash
        {
            if continued_lines.last() != Some(&logical_line) {
                continued_lines.push(logical_line);
            }
            i += 1 + site.newline_chars;
            site_idx += 1;
            continue;
        }
        out.push(chars[i]);
        if chars[i] == '\n' {
            logical_line += 1;
        }
        i += 1;
    }
    Ok(SplicedContinuations {
        source: Cow::Owned(out),
        continued_lines,
    })
}

/// Remove every explicit continuation marker and its physical newline.
///
/// The returned source is the logical input that must be classified and, for
/// shell dispatch, executed. No allocation occurs when no continuation exists.
pub fn splice_explicit_continuations(source: &str) -> MixResult<Cow<'_, str>> {
    Ok(splice_with_metadata(source)?.source)
}

#[cfg(test)]
mod tests {
    use super::{continuation_sites, splice_explicit_continuations};
    use crate::MixError;

    #[test]
    fn odd_runs_continue_and_even_runs_stay_literal() {
        assert_eq!(
            splice_explicit_continuations("echo one \\\ntwo").unwrap(),
            "echo one two"
        );
        assert_eq!(
            splice_explicit_continuations("echo one \\\\\ntwo").unwrap(),
            "echo one \\\\\ntwo"
        );
        assert_eq!(
            splice_explicit_continuations("echo one \\\\\\\ntwo").unwrap(),
            "echo one \\\\two"
        );
    }

    #[test]
    fn strings_comments_and_heredoc_bodies_are_untouched() {
        let source = concat!(
            "$p = \"C:\\\\path\"\n",
            "print(run(\"printf 'a\\\nb'\"))\n",
            "# comment \\\n",
            "-- comment \\\n",
            "$body = <<END\n",
            "body\\\n",
            "END\n",
        );
        assert_eq!(splice_explicit_continuations(source).unwrap(), source);
        assert!(continuation_sites(source).unwrap().is_empty());
    }

    #[test]
    fn eof_marker_is_typed_incomplete_but_even_run_is_not() {
        let error = splice_explicit_continuations("echo one \\")
            .expect_err("odd EOF run must be incomplete");
        assert!(error.is_incomplete_input(), "error: {error:?}");
        let MixError::IncompleteInput { span, .. } = error else {
            unreachable!("is_incomplete_input accepted another variant")
        };
        assert_eq!(span.line, 1);
        assert!(splice_explicit_continuations("echo one \\\\").is_ok());
    }

    #[test]
    fn crlf_is_one_continuation_boundary() {
        assert_eq!(
            splice_explicit_continuations("echo one \\\r\ntwo").unwrap(),
            "echo one two"
        );
    }
}
