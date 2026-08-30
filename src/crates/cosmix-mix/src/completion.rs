use std::cell::RefCell;
use std::collections::HashSet;
use std::env;
use std::rc::Rc;

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

use crate::shell;

/// Shared state between the REPL loop and the completer.
pub struct CompletionState {
    pub variable_names: Vec<String>,
    pub alias_names: Vec<String>,
    pub _function_names: Vec<String>,
}

pub struct MixHelper {
    path_commands: Vec<String>,
    pub state: Rc<RefCell<CompletionState>>,
}

impl MixHelper {
    pub fn new() -> Self {
        let path_commands = scan_path_commands();
        MixHelper {
            path_commands,
            state: Rc::new(RefCell::new(CompletionState {
                variable_names: Vec::new(),
                alias_names: Vec::new(),
                _function_names: Vec::new(),
            })),
        }
    }
}

impl Helper for MixHelper {}
impl Hinter for MixHelper {
    type Hint = String;
}
impl Highlighter for MixHelper {}
impl Validator for MixHelper {}

impl Completer for MixHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];

        // Find current word start
        let word_start = before
            .rfind(|c: char| c.is_whitespace() || c == '|' || c == ';')
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &before[word_start..];

        // Variable completion: $...
        if let Some(prefix) = word.strip_prefix('$') {
            let state = self.state.borrow();
            let matches: Vec<Pair> = state
                .variable_names
                .iter()
                .filter(|n| n.starts_with(prefix))
                .map(|n| Pair {
                    display: format!("${}", n),
                    replacement: format!("${}", n),
                })
                .collect();
            return Ok((word_start, matches));
        }

        let is_first_word = before[..word_start].trim().is_empty();

        // Complete `mix` subcommands when first word is "mix"
        if !is_first_word {
            let leading = before[..word_start].trim();
            if leading == "mix" {
                let subcmds = &[
                    "vars",
                    "aliases",
                    "functions",
                    "all",
                    "type",
                    "history",
                    "config",
                    "reload",
                    "build",
                    "test",
                    "update",
                    "help",
                    "man",
                    "status",
                    "check",
                    "trace",
                    "time",
                    "mesh",
                    "ports",
                    "ping",
                ];
                let matches: Vec<Pair> = subcmds
                    .iter()
                    .filter(|s| s.starts_with(word))
                    .map(|s| Pair {
                        display: s.to_string(),
                        replacement: s.to_string(),
                    })
                    .collect();
                return Ok((word_start, matches));
            }
        }

        if is_first_word {
            // Complete commands: PATH commands + builtins + keywords + aliases
            let mut matches: Vec<Pair> = Vec::new();

            // Shell builtins
            for name in shell::SHELL_BUILTINS {
                if name.starts_with(word) {
                    matches.push(Pair {
                        display: name.to_string(),
                        replacement: name.to_string(),
                    });
                }
            }

            // Mix keywords (subset useful in REPL)
            for kw in &[
                "if",
                "then",
                "else",
                "end",
                "for",
                "each",
                "in",
                "to",
                "step",
                "next",
                "while",
                "done",
                "loop",
                "break",
                "continue",
                "function",
                "fn",
                "return",
                "select",
                "when",
                "otherwise",
                "print",
                "eprint",
                "die",
                "try",
                "catch",
                "parse",
                "with",
                "export",
                "alias",
                "send",
                "address",
                "emit",
                "source",
                "sh",
                "true",
                "false",
                "nil",
                "label",
            ] {
                if kw.starts_with(word) {
                    matches.push(Pair {
                        display: kw.to_string(),
                        replacement: kw.to_string(),
                    });
                }
            }

            // Aliases
            {
                let state = self.state.borrow();
                for name in &state.alias_names {
                    if name.starts_with(word) {
                        matches.push(Pair {
                            display: name.clone(),
                            replacement: name.clone(),
                        });
                    }
                }
            }

            // PATH commands
            for name in &self.path_commands {
                if name.starts_with(word) {
                    matches.push(Pair {
                        display: name.clone(),
                        replacement: name.clone(),
                    });
                }
            }

            Ok((word_start, matches))
        } else {
            // Complete file paths
            complete_path(word_start, word)
        }
    }
}

fn complete_path(word_start: usize, word: &str) -> rustyline::Result<(usize, Vec<Pair>)> {
    let (dir, prefix) = if let Some(slash_pos) = word.rfind('/') {
        let dir_part = &word[..=slash_pos];
        let file_part = &word[slash_pos + 1..];
        // Handle ~ expansion
        let dir_expanded = if let Some(rest) = dir_part.strip_prefix('~') {
            let home = env::var("HOME").unwrap_or_default();
            format!("{}{}", home, rest)
        } else {
            dir_part.to_string()
        };
        (dir_expanded, file_part)
    } else {
        (".".to_string(), word)
    };

    let mut matches = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(prefix) {
                let full = if word.contains('/') {
                    let dir_part = &word[..word.rfind('/').unwrap() + 1];
                    format!("{}{}", dir_part, name)
                } else {
                    name.clone()
                };
                // Append / for directories
                let replacement = if entry.path().is_dir() {
                    format!("{}/", full)
                } else {
                    full
                };
                matches.push(Pair {
                    display: name,
                    replacement,
                });
            }
        }
    }
    matches.sort_by(|a, b| a.display.cmp(&b.display));
    Ok((word_start, matches))
}

fn scan_path_commands() -> Vec<String> {
    let mut commands = HashSet::new();
    let path_var = env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type()
                    && (ft.is_file() || ft.is_symlink())
                    && let Some(name) = entry.file_name().to_str()
                {
                    commands.insert(name.to_string());
                }
            }
        }
    }
    let mut sorted: Vec<String> = commands.into_iter().collect();
    sorted.sort();
    sorted
}
