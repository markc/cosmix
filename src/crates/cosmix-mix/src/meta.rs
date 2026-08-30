use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use cosmix_mix::builtin_info::{BuiltinContract, BuiltinInfo, TypeShape, render_signature};
use cosmix_mix::builtins::BUILTINS;
use cosmix_mix::builtins_hof::HOFS;
use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::value::Value;

fn cosmix_src_str() -> String {
    crate::cosmix_paths::cosmix_src()
        .to_string_lossy()
        .into_owned()
}

/// Process start time, initialized on first access.
static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Initialize the start time. Call early in the program.
pub fn init_start_time() {
    START_TIME.get_or_init(Instant::now);
}

/// Mix language keywords (for `mix type` lookups).
const MIX_KEYWORDS: &[&str] = &[
    "if",
    "else",
    "end",
    "for",
    "in",
    "while",
    "loop",
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
    "finally",
    "parse",
    "export",
    "alias",
    "break",
    "continue",
    "true",
    "false",
    "nil",
    "send",
    "address",
    "emit",
    "source",
    "sh",
    "and",
    "or",
    "not",
];

/// Extra entries for `mix help` / `mix builtins` / `mix what` that
/// don't correspond to a builtin function call — they're statements
/// (`print`, `eprint`). The main builtin metadata lives co-located with
/// dispatch in `cosmix_mix::builtins::BUILTINS` and
/// `cosmix_mix::builtins_hof::HOFS`; this list is a small append-on.
/// They carry full structured contracts like real builtins, but surface
/// with `kind: "statement"` and the synthetic capability `"statement"`
/// in discovery output.
struct ExtraStatement {
    name: &'static str,
    category: &'static str,
    description: &'static str,
    contract: BuiltinContract,
}

const EXTRA_STATEMENTS: &[ExtraStatement] = &[
    ExtraStatement {
        name: "print",
        category: "io",
        description: "Print values to stdout with newline (statement, not a builtin call); bare `print` emits a blank line",
        contract: cosmix_mix::contract!((values: ...any) -> nil),
    },
    ExtraStatement {
        name: "eprint",
        category: "io",
        description: "Print values to stderr with newline (statement, not a builtin call); bare `eprint` emits a blank line",
        contract: cosmix_mix::contract!((values: ...any) -> nil),
    },
];

/// One row of the unified discovery surface (`mix builtins` and friends):
/// a real builtin/HOF (`kind: "builtin"`) or a statement extra
/// (`kind: "statement"`, synthetic capability `"statement"`).
struct EntryRow {
    kind: &'static str,
    name: &'static str,
    category: &'static str,
    description: &'static str,
    capability: &'static str,
    contract: &'static BuiltinContract,
}

impl EntryRow {
    fn signature(&self) -> String {
        render_signature(self.name, self.contract)
    }
}

/// Iterate every (name, category, description) tuple across the three
/// sources: pure builtins, HOFs, and extras. Keeps consumers oblivious
/// to which registry holds which entry.
fn all_builtin_entries() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    all_builtin_rows().map(|r| (r.name, r.category, r.description))
}

/// Distinct builtin categories in first-appearance (registry) order.
/// **Derived** from the tables, not hand-maintained — so `mix builtins`
/// shows EVERY builtin (a hardcoded subset silently hid `db`/`jmap`/`bus`/
/// `datastar`) and `mix builtins <cat>` accepts every real category. A
/// `mix builtins <token>` that is not one of these is treated as a builtin
/// name (detail lookup) before erroring.
fn builtin_categories() -> Vec<&'static str> {
    let mut seen = std::collections::HashSet::new();
    let mut order = Vec::new();
    for row in all_builtin_rows() {
        if seen.insert(row.category) {
            order.push(row.category);
        }
    }
    order
}

/// The unified row iterator behind every discovery surface: builtins,
/// then HOFs, then statement extras — registry order preserved.
fn all_builtin_rows() -> impl Iterator<Item = EntryRow> {
    let core = BUILTINS.iter().map(|b: &BuiltinInfo| EntryRow {
        kind: "builtin",
        name: b.name,
        category: b.category,
        description: b.description,
        capability: b.capability.as_str(),
        contract: &b.contract,
    });
    let hofs = HOFS.iter().map(|b: &BuiltinInfo| EntryRow {
        kind: "builtin",
        name: b.name,
        category: b.category,
        description: b.description,
        capability: b.capability.as_str(),
        contract: &b.contract,
    });
    let extras = EXTRA_STATEMENTS.iter().map(|e| EntryRow {
        kind: "statement",
        name: e.name,
        category: e.category,
        description: e.description,
        capability: "statement",
        contract: &e.contract,
    });
    core.chain(hofs).chain(extras)
}

/// Total introspectable builtins (pure + HOF + statement extras).
fn builtin_count() -> usize {
    all_builtin_rows().count()
}

/// One builtin name per line, registry order — the bare machine list
/// (`mix builtins --names`). No sort, no dedup: the count equals the
/// registry total.
fn builtins_names_string() -> String {
    let mut out = String::new();
    for row in all_builtin_rows() {
        out.push_str(row.name);
        out.push('\n');
    }
    out
}

/// JSON encoding of a [`TypeShape`] for the discovery surface
/// (metadata_schema 1): `{"type": "..."}` scalars,
/// `{"type": "list", "items": ...}`, `{"type": "map", "shape":
/// string|null, "fields": [...]}`, `{"any_of": [...]}`.
fn shape_json(shape: &TypeShape) -> serde_json::Value {
    match shape {
        TypeShape::List(inner) => serde_json::json!({
            "type": "list",
            "items": shape_json(inner),
        }),
        TypeShape::Map { shape, fields } => serde_json::json!({
            "type": "map",
            "shape": shape,
            "fields": fields
                .iter()
                .map(|f| serde_json::json!({"name": f.name, "kind": shape_json(f.kind)}))
                .collect::<Vec<_>>(),
        }),
        TypeShape::AnyOf(shapes) => serde_json::json!({
            "any_of": shapes.iter().map(shape_json).collect::<Vec<_>>(),
        }),
        scalar => serde_json::json!({ "type": scalar.wire_name() }),
    }
}

/// One row as a metadata_schema-1 JSON object (shared by `--json` and,
/// via the Value mirror below, `--data`).
fn row_json(row: &EntryRow) -> serde_json::Value {
    let c = row.contract;
    serde_json::json!({
        "metadata_schema": 1,
        "kind": row.kind,
        "name": row.name,
        "category": row.category,
        "capability": row.capability,
        "conditional_capabilities": c.cond_caps
            .iter()
            .map(|cc| serde_json::json!({
                "option": cc.option,
                "capability": cc.capability.as_str(),
            }))
            .collect::<Vec<_>>(),
        "description": row.description,
        "signature": row.signature(),
        "arity": {
            "min": c.arity_min(),
            "max": c.arity_max(),
            "exact": c.exact_arities,
        },
        "args": c.args
            .iter()
            .map(|a| serde_json::json!({
                "name": a.name,
                "required": a.required,
                "variadic": a.variadic,
                "kind": shape_json(&a.kind),
            }))
            .collect::<Vec<_>>(),
        "returns": shape_json(&c.returns),
        "effects": {
            "must_use": c.effects.must_use,
            "blocking": c.effects.blocking,
            "shell": c.effects.shell,
            "mutates_args": c.effects.mutates_args,
            "terminates": c.effects.terminates,
        },
        "operational_failure": c.failure.wire_name(),
    })
}

/// The full builtin table as a JSON array (metadata_schema 1) — the
/// stable wire surface an agent parses to learn a node's capabilities.
/// Top-level shape stays a plain array for compatibility with pre-0.29
/// consumers; the per-entry fields are additive.
fn builtins_json_string() -> String {
    let arr: Vec<serde_json::Value> = all_builtin_rows().map(|r| row_json(&r)).collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(arr))
        .unwrap_or_else(|_| "[]".to_string())
}

/// Convert a serde_json value into a Mix `Value` (JSON is a subset, so
/// this is total). Backs `mix builtins --data` — the same logical array
/// as `--json`, emitted as strict-data Mix source.
pub(crate) fn json_to_mix_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => {
            Value::list(items.iter().map(json_to_mix_value).collect())
        }
        serde_json::Value::Object(map) => {
            let mut out = cosmix_mix::IndexMap::new();
            for (k, val) in map {
                out.insert(k.clone(), json_to_mix_value(val));
            }
            Value::map(out)
        }
    }
}

/// `mix builtins --data`: the discovery table as strict-data Mix source.
fn builtins_data_string() -> String {
    let rows: Vec<Value> = all_builtin_rows()
        .map(|r| json_to_mix_value(&row_json(&r)))
        .collect();
    Value::list(rows)
        .to_mix_data_string_pretty()
        .unwrap_or_else(|e| format!("-- data encode failed: {e}"))
}

/// One category's rows, each `  signature  description` (signatures are
/// derived from the structured contracts, so they are always present).
/// Shared by the full dump and the single-category view.
fn push_category(out: &mut String, cat: &str) {
    out.push_str(&format!("{} builtins:\n", cat));
    for row in all_builtin_rows() {
        if row.category == cat {
            out.push_str(&format!("  {:32} {}\n", row.signature(), row.description));
        }
    }
}

/// The human/agent-readable listing. `None` = every category in order (the
/// full remit, the default `mix builtins`); `Some(cat)` = one category.
fn builtins_report_string(filter: Option<&str>) -> String {
    let mut out = String::new();
    match filter {
        Some(cat) => push_category(&mut out, cat),
        None => {
            for cat in builtin_categories() {
                push_category(&mut out, cat);
                out.push('\n');
            }
        }
    }
    out
}

/// Detail block for a single builtin (`mix builtins <name>`): signature (or
/// bare name), category, capability class, and description. `None` if the
/// name is not a registered builtin.
fn builtin_detail_string(name: &str) -> Option<String> {
    all_builtin_rows().find(|r| r.name == name).map(|r| {
        format!(
            "{}\n  category:   {}\n  capability: {}\n  {}\n",
            r.signature(),
            r.category,
            r.capability,
            r.description
        )
    })
}

/// The markdown-friendly capability-discovery overview printed by both
/// `mix --help` (CLI) and `mix help` (REPL/subcommand). It deliberately
/// does NOT list the builtin functions — that is `mix builtins`' job; this
/// is the fast "what can this version do, and how do I invoke it" surface.
pub fn help_overview_string(version: &str) -> String {
    format!(
        "\
# mix {version} — scripting language & shell for the Cosmix stack

Run `mix builtins` for the full remit of all {count} built-in functions
(`mix builtins --json` machine-readable · `mix builtins <name>` one function ·
`mix builtins <category>` filter · or the `mix --builtins` flag).

## Usage
- `mix`                     Interactive shell (REPL)
- `mix <script> [args...]`  Run a .mix script
- `mix -c '<code>'`         Run Mix code, or dispatch a shell command
- `mix -i -c '<code>'`      As -c, but load ~/.mixrc first (aliases/PATH)
- `mix - [args...]`         Read a script from stdin (e.g. ssh host mix -)
- `mix --check <script>`    Parse without executing (syntax check)
- `mix --serve <script>`    Run as a supervised Bus daemon citizen [--name <svc>]

## Options
- `--help`, `-h`      This overview
- `--version`, `-V`   Version
- `--builtins`        Full builtin list (same as `mix builtins`)
- `--no-prelude`      Skip loading the standard prelude
- `--no-traceback`    Legacy single-line rendering for uncaught errors (default: traceback)
- `--strict-arity`    Wrong-arity calls raise ARITY_MISMATCH (default: missing->nil, extra ignored)

## Subcommands
- Reference    `help`  `builtins`  `what <name>`  `man <topic>`  `keywords`  `syntax`  `operators`
- Introspect   `vars`  `functions`  `aliases`  `all`  `type`  `config`  `status`
- Build        `build`  `clean`  `update`  `test`
- Diagnostics  `stats`  `time`  `check`  `lint`  `trace`  `history`  `reload`
- Ecosystem    `mesh`  `ports`  `ping <svc>`
- AI-powered   `fix`  `extend`  `review`  `explain`  `evolve`  `dogfood`  `fuzz`  `teach`

## Keywords
`if/then/else/end` · `for/to/step` · `for each/in` · `while` · `loop` · `select/when/otherwise` · `function`/`fn`/`return` · `try/catch` · `parse/with` · `on/end` · `export` · `alias` · `source` · `sh` · `die` · `break` · `continue`

## Prelude helpers
`lines`  `chars`  `sum`  `avg`  `read_lines`

## Learn more
- `mix builtins`        every builtin — name, signature, capability, description
- `mix what <name>`     one-line lookup for any builtin or keyword
- `mix man <topic>`     full manual page (online with local fallback)
- Manual online: https://cosmix.dev/mix/
",
        version = version,
        count = builtin_count(),
    )
}

/// One-line descriptions for keywords.
const KEYWORD_DESCRIPTIONS: &[(&str, &str)] = &[
    ("if", "Conditional branch: if EXPR then ... else ... end"),
    ("else", "Alternative branch in if/select"),
    (
        "end",
        "Close ANY block (v0.2.2): if, select, function, try, address, for, while, loop, on",
    ),
    (
        "for",
        "Numeric loop: for $i = 1 to 10 ... end; or for each $x in LIST ... end",
    ),
    ("in", "Iterator source in for-each loops"),
    ("while", "Conditional loop: while EXPR ... end"),
    ("loop", "Infinite loop: loop ... end (use break to exit)"),
    (
        "function",
        "Define a function: function name($a, $b) ... end. Also anonymous: $f = function($x) = $x + 1 (v0.2.0)",
    ),
    (
        "fn",
        "Short alias for `function` (Rust-style): fn name($a) ... end, or fn($x) = expr (v0.13.0)",
    ),
    ("return", "Return a value from a function"),
    (
        "select",
        "Pattern match: select EXPR when VAL ... otherwise ... end",
    ),
    ("when", "Case arm in a select block"),
    ("otherwise", "Default arm in a select block"),
    ("print", "Print values to stdout with newline"),
    ("eprint", "Print values to stderr with newline"),
    ("die", "Print message to stderr and exit with error"),
    (
        "try",
        "Error handling: try ... catch $e[, $err] ... [finally ...] end",
    ),
    (
        "catch",
        "Catch block in try/catch; catch $msg, $err binds the structured error",
    ),
    (
        "finally",
        "Cleanup block that runs on every exit path of a try (v0.30.0)",
    ),
    ("parse", "ARexx-style parsing: parse $src with template"),
    ("export", "Export variable to environment: export $VAR"),
    (
        "alias",
        "Define shell alias: alias name = expansion (name/expansion may be any expression)",
    ),
    ("break", "Exit innermost loop"),
    ("continue", "Skip to next iteration of innermost loop"),
    ("true", "Boolean true literal"),
    ("false", "Boolean false literal"),
    ("nil", "Nil literal (no value)"),
    ("send", "Send message to Bus port: send PORT \"msg\""),
    ("address", "Set default Bus target: address PORT ... end"),
    ("emit", "Fire-and-forget Bus message"),
    ("source", "Execute a .mix file in current scope"),
    ("sh", "Execute external shell command"),
    ("and", "Logical AND operator"),
    ("or", "Logical OR operator"),
    ("not", "Logical NOT operator"),
    (
        "next",
        "Close for/for-each block (v0.1.x legacy — use 'end' in new code, v0.2.2)",
    ),
    (
        "done",
        "Close while/loop/on block (v0.1.x legacy — use 'end' in new code, v0.2.2)",
    ),
];

/// The canonical `mix X.Y.Z` version line. Shared by the CLI `--version` flag,
/// the REPL `mix --version` meta-command, and the status-overview header so the
/// three can never drift out of a common format.
pub fn version_line(version: &str) -> String {
    format!("mix {version}")
}

/// Dispatch a `mix` meta-command. `args` contains the tokens after "mix".
/// Returns Some(path) if the REPL should save history and exec into a new binary.
/// Subcommands whose `dispatch` arm reads the live `Evaluator` (vars,
/// aliases, functions, session context) or exec-restarts the current
/// process (`build`/`update`); the empty string is bare `mix` (status).
/// An external `mix <sub>` child would answer these from a fresh, empty
/// evaluator — silently different state — so the interactive REPL refuses
/// to run them externally when the line carries plumbing (repl.rs). Kept
/// NEXT TO `dispatch` so a new eval-consuming arm and this list change in
/// the same edit.
pub fn needs_live_eval(sub: &str) -> bool {
    matches!(
        sub,
        "" | "vars"
            | "aliases"
            | "functions"
            | "all"
            | "type"
            | "status"
            | "context"
            | "snapshot"
            | "ask"
            | "chat"
            | "build"
            | "update"
    )
}

pub fn dispatch(args: &[&str], eval: &Evaluator, version: &str) -> Option<String> {
    if args.is_empty() {
        cmd_status(eval, version);
        return None;
    }

    match args[0] {
        "vars" => cmd_vars(eval),
        "aliases" => cmd_aliases(eval),
        "functions" => cmd_functions(eval),
        "all" => cmd_all(eval),
        "type" => {
            if args.len() < 2 {
                eprintln!("mix type: requires a name argument");
            } else {
                cmd_type(args[1], eval);
            }
        }
        "config" => cmd_config(version),
        "build" => return cmd_build(),
        "clean" => cmd_clean(),
        "update" => return cmd_update(),
        "test" => cmd_test(),
        "self" => {
            if args.len() >= 2 && args[1] == "check" {
                cmd_self_check();
            } else {
                eprintln!("mix self: unknown subcommand (try: mix self check)");
            }
        }
        "status" => cmd_status(eval, version),
        // `mix --version` / `-V` / `version` at the REPL prints the version line,
        // matching the CLI `mix --version`. Without an explicit arm it fell through
        // to `_ => cmd_help_overview()` and dumped the whole meta-command help
        // instead of the version (2026-07-14 report).
        "--version" | "-V" | "version" => println!("{}", version_line(version)),
        "check" => {
            if args.len() < 2 {
                eprintln!("mix check: requires a filename");
            } else {
                cmd_check(args[1]);
            }
        }
        "diff" => {
            if args.len() < 2 {
                eprintln!("mix diff: requires a language argument (e.g. mix diff bash)");
            } else {
                cmd_diff(args[1]);
            }
        }
        "mesh" => cmd_mesh(),
        "ports" => cmd_ports(),
        "ping" => {
            if args.len() < 2 {
                eprintln!("mix ping: requires a service name argument");
            } else {
                cmd_ping(args[1]);
            }
        }
        "tutorial" => cmd_tutorial(),
        "examples" => cmd_examples(),
        "man" => cmd_man(if args.len() > 1 { Some(args[1]) } else { None }),
        "help" => cmd_help_full(version),
        "keywords" => cmd_keywords(),
        "builtins" => cmd_builtins(&args[1..], version),
        "what" => {
            if args.len() < 2 {
                eprintln!("mix what: requires a name argument");
            } else {
                cmd_what(args[1]);
            }
        }
        "syntax" => cmd_man(Some("variables")),
        "operators" => cmd_man(Some("operators")),

        // AI-powered commands (require Claude Code CLI)
        "fix" => {
            if args.len() < 2 {
                eprintln!("mix fix: requires a description argument");
            } else {
                let desc = args[1..].join(" ");
                cmd_ai(
                    "fix",
                    &format!(
                        "Fix this issue in the Mix language project at {}/crates/cosmix-lib-mix/: {}. \
                     Read the relevant source, write a fix and test, then run cargo test.",
                        cosmix_src_str(),
                        desc
                    ),
                );
            }
        }
        "extend" => {
            if args.len() < 2 {
                eprintln!("mix extend: requires a feature description argument");
            } else {
                let desc = args[1..].join(" ");
                cmd_ai(
                    "extend",
                    &format!(
                        "Implement this feature in the Mix language at {}/crates/cosmix-lib-mix/: {}. \
                     Write tests first, then implement. Run cargo test when done.",
                        cosmix_src_str(),
                        desc
                    ),
                );
            }
        }
        "review" => cmd_ai(
            "review",
            &format!(
                "Review recent changes in {}. Run git log --oneline -10 and \
             git diff HEAD~1 to see what changed. Provide a code review.",
                cosmix_src_str()
            ),
        ),
        "explain" => {
            if args.len() < 2 {
                eprintln!("mix explain: requires a builtin name argument");
            } else {
                cmd_ai(
                    "explain",
                    &format!(
                        "Explain how the '{}' builtin works in the Mix language. \
                     Read {}/crates/cosmix-lib-mix/src/builtins.rs and evaluator.rs.",
                        args[1],
                        cosmix_src_str()
                    ),
                );
            }
        }
        "evolve" => cmd_ai(
            "evolve",
            &format!(
                "Read {}/MIX_TODO.md. Pick the highest-value uncompleted item. \
             Implement it with tests. Run cargo test.",
                cosmix_src_str()
            ),
        ),
        "dogfood" => cmd_ai(
            "dogfood",
            &format!(
                "Write 5 practical Mix scripts that test real-world usage patterns. \
             Run them in {}/ and report any awkward syntax or missing features.",
                cosmix_src_str()
            ),
        ),
        "fuzz" => cmd_ai(
            "fuzz",
            &format!(
                "Generate random Mix code to fuzz-test the parser and evaluator. \
             Try edge cases. Report any crashes or panics found. Work in {}/.",
                cosmix_src_str()
            ),
        ),
        "teach" => cmd_ai(
            "teach",
            &format!(
                "Read recent git history in {}. Create a tutorial .mix script \
             demonstrating the newest features. Save it to {}/examples/.",
                cosmix_src_str(),
                cosmix_src_str()
            ),
        ),

        // Session context and AI integration
        "context" | "snapshot" => cmd_context(eval),
        "ask" => {
            if args.len() < 2 {
                eprintln!("mix ask: requires a question");
            } else {
                let question = args[1..].join(" ");
                cmd_ask(&question, eval);
            }
        }
        "chat" => cmd_chat(eval),

        // Mesh orchestration
        "deploy" => {
            if args.len() < 2 {
                eprintln!("mix deploy: requires a service name");
            } else {
                cmd_deploy(args[1]);
            }
        }
        "health" => cmd_health(if args.len() > 1 { Some(args[1]) } else { None }),
        "logs" => {
            if args.len() < 2 {
                eprintln!("mix logs: requires a service name");
            } else {
                let follow = args.iter().any(|a| *a == "--follow" || *a == "-f");
                cmd_logs(args[1], follow);
            }
        }

        // Claude Bus port management
        "claude-start" => cmd_claude_port("start"),
        "claude-stop" => cmd_claude_port("stop"),
        "claude-status" => cmd_claude_port("status"),

        // Watch mode
        "watch" => {
            if args.len() < 3 {
                eprintln!("Usage: mix watch PATTERN COMMAND");
                eprintln!("Example: mix watch '*.mix' 'cargo test'");
            } else {
                let pattern = args[1];
                let command = args[2..].join(" ");
                cmd_watch(pattern, &command);
            }
        }

        filename if Path::new(filename).is_file() => {
            // `run_source` has one-shot process semantics: it installs global
            // script argv, owns a Tokio runtime, and exits on an uncaught error.
            // Re-enter the CLI script path in a child instead: it gives the file
            // the same clean evaluator and positional argv as `mix FILE`, without
            // replacing or contaminating the live REPL process.
            let status = std::process::Command::new("/proc/self/exe")
                .arg(filename)
                .args(&args[1..])
                .status();
            match status {
                Ok(status) if !status.success() => {
                    eprintln!("mix: script '{}' exited with {}", filename, status);
                }
                Err(e) => {
                    eprintln!("mix: failed to run script '{}': {}", filename, e);
                }
                _ => {}
            }
        }
        unknown => {
            eprintln!("mix: unknown meta-command '{}' (and no such file)", unknown);
            cmd_help_overview();
        }
    }
    None
}

/// `mix` (no args) or `mix status`: status overview
fn cmd_status(eval: &Evaluator, version: &str) {
    let var_count = eval.scope().variable_names().len();
    let alias_count = eval.aliases().len();
    let func_count = eval.scope().function_names().len();
    let scope_depth = eval.scope().frame_count();
    let pid = std::process::id();

    // Uptime
    let uptime_str = if let Some(start) = START_TIME.get() {
        let elapsed = start.elapsed();
        let secs = elapsed.as_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m{}s", secs / 60, secs % 60)
        } else {
            format!("{}h{}m{}s", secs / 3600, (secs % 3600) / 60, secs % 60)
        }
    } else {
        "?".to_string()
    };

    // Memory from /proc/self/status (Linux only)
    let mem_str = read_vm_rss().unwrap_or_else(|| "?".to_string());

    println!("{}", version_line(version));
    println!("  pid:       {}", pid);
    println!("  uptime:    {}", uptime_str);
    println!("  memory:    {}", mem_str);
    println!("  variables: {}", var_count);
    println!("  aliases:   {}", alias_count);
    println!("  functions: {}", func_count);
    println!("  scope:     {} frame(s)", scope_depth);
    println!("  trace:     {}", if eval.trace() { "on" } else { "off" });
}

/// `mix vars`: list all variables
fn cmd_vars(eval: &Evaluator) {
    let mut names = eval.scope().variable_names();
    names.sort();
    for name in &names {
        if let Some(val) = eval.scope().get(name) {
            let display = val.to_mix_string();
            if display.len() > 60 {
                println!("${} = {}...", name, &display[..60]);
            } else {
                println!("${} = {}", name, display);
            }
        }
    }
}

/// `mix aliases`: list all aliases
fn cmd_aliases(eval: &Evaluator) {
    let aliases = eval.aliases();
    let mut names: Vec<&String> = aliases.keys().collect();
    names.sort();
    for name in names {
        if let Some(expansion) = aliases.get(name) {
            println!("{} = {}", name, expansion);
        }
    }
}

/// `mix functions`: list all user-defined functions
fn cmd_functions(eval: &Evaluator) {
    let mut funcs = eval.scope().function_names();
    funcs.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, params) in &funcs {
        println!("{}({})", name, params.join(", "));
    }
}

/// `mix all`: vars + aliases + functions with section headers
fn cmd_all(eval: &Evaluator) {
    println!("--- Variables ---");
    cmd_vars(eval);
    println!();
    println!("--- Aliases ---");
    cmd_aliases(eval);
    println!();
    println!("--- Functions ---");
    cmd_functions(eval);
}

/// `mix type NAME`: identify what a name is
fn cmd_type(name: &str, eval: &Evaluator) {
    // Builtin function
    if cosmix_mix::builtins::is_builtin(name) {
        println!("{} is a builtin function", name);
        return;
    }

    // Keyword
    if MIX_KEYWORDS.contains(&name) {
        println!("{} is a keyword", name);
        return;
    }

    // Alias
    if let Some(expansion) = eval.aliases().get(name) {
        println!("{} is an alias for {}", name, expansion);
        return;
    }

    // User function
    if eval.has_function(name) {
        println!("{} is a user-defined function", name);
        return;
    }

    // Variable
    if eval.scope().get(name).is_some() {
        println!("{} is a variable", name);
        return;
    }

    // PATH command
    if let Some(path) = which_command(name) {
        println!("{} is {}", name, path);
        return;
    }

    println!("{}: not found", name);
}

/// `mix config`: runtime configuration info
fn cmd_config(version: &str) {
    // Mirror `load_mixrc`/`cmd_self_check`: empty HOME is treated as
    // unavailable, not as a meaningful root-home path.
    let home = env::var("HOME").ok().filter(|h| !h.is_empty());
    let os_id = read_os_id();
    let arch = std::env::consts::ARCH;
    let pid = std::process::id();

    println!("version:  mix {}", version);
    match &home {
        Some(h) => println!("home:     {}", h),
        None => println!("home:     <unavailable: HOME unset>"),
    }
    println!(
        "prelude:  {}/crates/cosmix-lib-mix/std/prelude.mix",
        cosmix_src_str()
    );
    match &home {
        Some(h) => println!("rc file:  {}/.mixrc", h),
        None => println!("rc file:  <unavailable: HOME unset>"),
    }
    println!("os:       {}", os_id);
    println!("arch:     {}", arch);
    println!("pid:      {}", pid);
}

/// `mix build`: cargo build --release, strip, install to $COSMIX_BIN/
/// Returns Some(path) if the REPL should save history and exec into the new binary.
fn cmd_build() -> Option<String> {
    let mix_dir = cosmix_src_str();
    let bin_dir = crate::cosmix_paths::cosmix_path(crate::cosmix_paths::CosmixDir::Bin)
        .to_string_lossy()
        .into_owned();

    // Ensure bin dir exists
    let _ = std::fs::create_dir_all(&bin_dir);

    println!("Building mix (release)...");
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&mix_dir)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("cargo build failed with {}", s);
            return None;
        }
        Err(e) => {
            eprintln!("Failed to run cargo: {}", e);
            return None;
        }
    }

    // Install the binary.
    // On Linux, a running executable can't be written to (ETXTBSY),
    // but it CAN be unlinked/removed — the process keeps running from
    // the old inode in memory. So: remove first, then copy, then strip.
    let src = format!("{}/target/release/mix", mix_dir);
    let dst = format!("{}/mix", bin_dir);

    println!("Installing {} -> {}", src, dst);
    // Direct fs calls — no `sh -c` string, so a COSMIX_BIN/COSMIX_SRC path
    // containing spaces or shell metacharacters can't be re-interpreted.
    // Remove-then-copy (never write in place) per the ETXTBSY note above;
    // a missing destination is the normal first-install case, not an error.
    if let Err(e) = std::fs::remove_file(&dst)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("Failed to remove {}: {}", dst, e);
        return None;
    }
    match std::fs::copy(&src, &dst) {
        Ok(_) => {
            println!("Installed. Restarting...");
            // Return the path — the REPL will save history then exec()
            Some(dst)
        }
        Err(e) => {
            eprintln!("Failed to install binary: {}", e);
            None
        }
    }
}

/// `mix clean`: cargo clean to remove build artifacts ($COSMIX_SRC/src/target/)
fn cmd_clean() {
    let mix_dir = cosmix_src_str();

    println!("Cleaning build artifacts...");
    match std::process::Command::new("cargo")
        .arg("clean")
        .current_dir(&mix_dir)
        .status()
    {
        Ok(s) if s.success() => {
            // Show reclaimed space
            println!("Removed {}/target/", mix_dir);
        }
        Ok(s) => eprintln!("cargo clean failed with {}", s),
        Err(e) => eprintln!("Failed to run cargo: {}", e),
    }
}

/// `mix update`: git pull then build
fn cmd_update() -> Option<String> {
    let mix_home = cosmix_src_str();

    println!("Updating from git...");
    let status = std::process::Command::new("git")
        .args(["pull"])
        .current_dir(&mix_home)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("git pull failed with {}", s);
            return None;
        }
        Err(e) => {
            eprintln!("Failed to run git: {}", e);
            return None;
        }
    }

    cmd_build()
}

/// `mix test`: run cargo test, streaming output
fn cmd_test() {
    let mix_dir = cosmix_src_str();

    let status = std::process::Command::new("cargo")
        .args(["test"])
        .current_dir(&mix_dir)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("cargo test failed with {}", s),
        Err(e) => eprintln!("Failed to run cargo: {}", e),
    }
}

/// `mix self check`: syntax-check ~/.mixrc and std/prelude.mix.
///
/// `.mixrc` is a hybrid Mix-plus-bareword-shell file (loaded via the
/// `source` per-line shell fallback — see `evaluator::exec_source`),
/// so a pure-Mix parse failure is not necessarily an error. When the
/// whole-file parse fails on `.mixrc`, fall back to the same
/// classification logic the loader uses: every non-blank line must
/// be `Empty`, `Shell`, or `Mix`; a `classify` `Err` (line is
/// neither valid Mix nor a known shell command) is the real
/// diagnostic.
///
/// `prelude.mix` stays strict pure-Mix — it's part of the library
/// surface and must always whole-file parse cleanly.
fn cmd_self_check() {
    let mixrc_label = "~/.mixrc";
    let home = env::var("HOME").ok().filter(|h| !h.is_empty());
    match home {
        Some(h) => {
            let mixrc_path = format!("{}/.mixrc", h);
            check_mixrc(&mixrc_path, mixrc_label);
        }
        None => eprintln!("{}: skipped (HOME unset)", mixrc_label),
    }
    let prelude_label = "std/prelude.mix";
    let prelude_path = format!("{}/crates/cosmix-lib-mix/std/prelude.mix", cosmix_src_str());
    check_pure_mix(&prelude_path, prelude_label);
}

fn check_pure_mix(path: &str, label: &str) {
    match std::fs::read_to_string(path) {
        Ok(source) => match cosmix_mix::lexer::Lexer::new(&source).tokenize() {
            // Authoritative, not speculative: this parse *is* what the user
            // asked for, so a deprecation in the checked file is part of the
            // answer and must still print.
            Ok(tokens) => match cosmix_mix::parser::Parser::new(tokens, &source).parse_program() {
                Ok(_) => println!("{}: OK", label),
                Err(e) => eprintln!("{}: {}", label, e),
            },
            Err(e) => eprintln!("{}: {}", label, e),
        },
        Err(e) => eprintln!("{}: {}", label, e),
    }
}

fn check_mixrc(path: &str, label: &str) {
    use cosmix_mix::evaluator::{ShellHandler, ShellLineKind};

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: {}", label, e);
            return;
        }
    };

    // Whole-file pure-Mix parse first — the cheap common case for a
    // `.mixrc` with no bareword shell lines. Authoritative: `check` was
    // pointed at this file, so a deprecation inside it is part of the answer.
    // The per-line probe below is the speculative one — it runs over lines
    // that may turn out to be shell, not Mix.
    let whole_file_err = match cosmix_mix::lexer::Lexer::new(&source)
        .tokenize()
        .and_then(|tokens| cosmix_mix::parser::Parser::new(tokens, &source).parse_program())
    {
        Ok(_) => {
            println!("{}: OK", label);
            return;
        }
        Err(e) => e,
    };

    // Per-line shell-fallback validation (mirrors `exec_source_per_line`
    // Phase 1's unambiguous-shell probe + Phase 2's strict classify,
    // minus the actual execute). Carries an evolving alias map so
    // lines like `gg world` after an earlier `alias gg = "ls"`
    // classify correctly.
    //
    // `has_unambiguous_shell` mirrors the runtime's gate: the
    // per-line fallback only engages when at least one line is
    // classified `Shell` AND does NOT also parse standalone as Mix.
    // Without this, a syntax-invalid pure-Mix `.mixrc` (e.g. an
    // unterminated `function` block) would slip through here while
    // `exec_source_per_line` rejects it with the whole-file error —
    // making `mix self check` lie about loadability.
    let handler = crate::shell_handler::ReplShellHandler::new();
    let mut aliases: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut classify_errors: Vec<(usize, String)> = Vec::new();
    let mut has_unambiguous_shell = false;
    for (idx, raw) in source.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        // Pre-parse once: used by both the unambiguous-shell gate
        // (Mix-parseable lines are ambiguous) and the
        // alias-evolution scan below. Speculative — only its `is_err()` is
        // read — so it must not print parser diagnostics: a `check` emits its
        // own report, and `mix lint` is what reports deprecations.
        let parsed = cosmix_mix::lexer::Lexer::new(line)
            .tokenize()
            .and_then(|t| cosmix_mix::parser::Parser::new_speculative(t, line).parse_program());
        let classified = handler.classify(line, &aliases);
        match &classified {
            Ok(ShellLineKind::Empty) | Ok(ShellLineKind::Mix) => {}
            Ok(ShellLineKind::Shell) => {
                if parsed.is_err() {
                    has_unambiguous_shell = true;
                }
                // Classify only checks the first word; some Shell
                // lines (unsupported REPL builtins, malformed
                // pipelines) only fail at execute time. Run the same
                // static checks `execute_shell` does, minus the
                // process spawn and `$VAR` expansion.
                if let Err(msg) = handler.validate_shell(line, &aliases) {
                    classify_errors.push((idx + 1, msg));
                }
            }
            Err(msg) => classify_errors.push((idx + 1, msg.clone())),
        }
        // Best-effort alias tracking — same shape as
        // `evaluator::exec_source_per_line` Phase 1.
        if let Ok(stmts) = parsed {
            for stmt in stmts {
                if let cosmix_mix::ast::StmtKind::Alias {
                    name: Some(cosmix_mix::ast::Expr::StringLiteral(n)),
                    command: Some(cosmix_mix::ast::Expr::StringLiteral(c)),
                } = stmt.kind
                {
                    aliases.insert(n, c);
                }
            }
        }
    }

    if !classify_errors.is_empty() {
        eprintln!("{}: {}", label, whole_file_err);
        for (line_no, msg) in classify_errors {
            eprintln!("  line {}: {}", line_no, msg);
        }
    } else if has_unambiguous_shell {
        println!("{}: OK (mixed shell+Mix)", label);
    } else {
        // No classify errors, but also no shell content the per-line
        // fallback would engage on — the runtime would propagate
        // `whole_file_err` here. Report it instead of falsely OK.
        eprintln!("{}: {}", label, whole_file_err);
    }
}

/// `mix check FILE`: syntax-check an arbitrary file
fn cmd_check(filename: &str) {
    match std::fs::read_to_string(filename) {
        Ok(source) => {
            let mut lexer = cosmix_mix::lexer::Lexer::new(&source);
            match lexer.tokenize() {
                Ok(tokens) => {
                    let mut parser = cosmix_mix::parser::Parser::new(tokens, &source);
                    match parser.parse_program() {
                        Ok(_) => println!("OK: {}", filename),
                        Err(e) => eprintln!("{}: {}", filename, e),
                    }
                }
                Err(e) => eprintln!("{}: {}", filename, e),
            }
        }
        Err(e) => eprintln!("{}: {}", filename, e),
    }
}

/// `mix diff LANG`: translation cheatsheet from another language to Mix
fn cmd_diff(lang: &str) {
    match lang {
        "bash" | "sh" => cmd_diff_bash(),
        _ => eprintln!("mix diff: no cheatsheet for '{}' (available: bash)", lang),
    }
}

/// `mix diff bash`: bash-to-mix translation cheatsheet
#[allow(clippy::print_literal)] // raw-string literals carry embedded quotes that don't inline cleanly
fn cmd_diff_bash() {
    println!("Bash to Mix Translation Cheatsheet");
    println!();
    println!("{:<34} {}", "Bash", "Mix");
    println!("{:<34} {}", "─".repeat(30), "─".repeat(30));
    println!("{:<34} {}", r#"VAR="value""#, r#"$var = "value""#);
    println!("{:<34} {}", r#"echo "$VAR""#, r#"print $var"#);
    println!(
        "{:<34} {}",
        r#"echo -n "no newline""#, r#"sh "echo -n 'no newline'""#
    );
    println!(
        "{:<34} {}",
        r#"if [ -f file ]; then"#, r#"if is_file("file") then"#
    );
    println!(
        "{:<34} {}",
        r#"if [ -d dir ]; then"#, r#"if is_dir("dir") then"#
    );
    println!(
        "{:<34} {}",
        r#"if [ -e path ]; then"#, r#"if exists("path") then"#
    );
    println!("{:<34} {}", r#"[[ $a == $b ]]"#, r#"if $a == $b then"#);
    println!("{:<34} {}", r#"[[ $a != $b ]]"#, r#"if $a != $b then"#);
    println!("{:<34} {}", r#"[[ -z $a ]]"#, r#"if $a == "" then"#);
    println!(
        "{:<34} {}",
        r#"for i in 1 2 3; do"#, r#"for each $i in [1, 2, 3]"#
    );
    println!(
        "{:<34} {}",
        r#"for ((i=1; i<=10; i++)); do"#, r#"for $i = 1 to 10"#
    );
    println!("{:<34} {}", r#"while true; do"#, r#"loop"#);
    println!("{:<34} {}", r#"while COND; do"#, r#"while COND"#);
    println!("{:<34} {}", r#"done"#, r#"done / next / end"#);
    println!("{:<34} {}", r#"function foo() {"#, r#"function foo()"#);
    println!(
        "{:<34} {}",
        r#"local var="val""#, r#"$var = "val"  (auto-local in functions)"#
    );
    println!("{:<34} {}", r#"return 0"#, r#"return $value"#);
    println!(
        "{:<34} {}",
        r#"$(command)"#, r#"$(command)  or  sh "command""#
    );
    println!("{:<34} {}", r#"| pipe"#, r#"| (same syntax)"#);
    println!("{:<34} {}", r#"> redirect"#, r#"> (same syntax)"#);
    println!("{:<34} {}", r#">> append"#, r#">> (same syntax)"#);
    println!(
        "{:<34} {}",
        r#"grep pattern file"#, r#"sh "grep pattern file""#
    );
    println!("{:<34} {}", r#"${var:-default}"#, r#"$var ?? "default""#);
    println!("{:<34} {}", r#"# comment"#, r#"-- comment  or  # comment"#);
    println!("{:<34} {}", r#"source file.sh"#, r#"source "file.mix""#);
    println!("{:<34} {}", r#"export VAR=val"#, r#"export VAR = "val""#);
    println!(
        "{:<34} {}",
        r#"alias ll='ls -la'"#, r#"alias ll = "ls -la""#
    );
    println!(
        "{:<34} {}",
        r#"read -p "? " var"#, r#"$var = readline("? ")"#
    );
    println!(
        "{:<34} {}",
        r#"case $x in ... esac"#, r#"select $x when ... end"#
    );
    println!(
        "{:<34} {}",
        r#"cat file.txt"#, r#"print read_file("file.txt")"#
    );
    println!(
        "{:<34} {}",
        r#"echo "$x" > file"#, r#"write_file("file", $x)"#
    );
    println!(
        "{:<34} {}",
        r#"wc -l file"#, r#"print length(lines(read_file("file")))"#
    );
    println!(
        "{:<34} {}",
        r#"basename "$path""#, r#"sh "basename " .. $path"#
    );
    println!("{:<34} {}", r#"$? (exit code)"#, r#"$rc (set by sh/send)"#);
    println!("{:<34} {}", r#"$@ (all args)"#, r#"args() or $1, $2, ..."#);
    println!("{:<34} {}", r#"$0 (script name)"#, r#"$0 (same)"#);
    println!("{:<34} {}", r#"sleep 5"#, r#"sleep(5)"#);
    println!("{:<34} {}", r#"exit 1"#, r#"exit(1)"#);
    println!();
    println!("Key differences:");
    println!("  - Variables always use $sigil: $name, $list, $map");
    println!("  - String concatenation: .. (dot-dot), not bare juxtaposition");
    println!("  - Arithmetic: + - * / ** work if both operands are numeric");
    println!("  - Blocks end with end/done/next, not braces or fi/esac");
    println!("  - Functions have isolated scope (no access to caller variables)");
}

/// Read VmRSS from /proc/self/status (Linux only).
fn read_vm_rss() -> Option<String> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Print available subcommands (fallback for unknown commands)
fn cmd_help_overview() {
    println!("mix meta-commands:");
    println!("  mix              Status overview (version, counts, pid)");
    println!("  mix FILE [ARGS]  Run a script in a clean child process");
    println!("  mix --version    Print the version line (also: mix -V)");
    println!("  mix status       Detailed status (uptime, memory, scope depth, trace)");
    println!("  mix vars         List all variables");
    println!("  mix aliases      List all aliases");
    println!("  mix functions    List all user-defined functions");
    println!("  mix all          List vars, aliases, and functions");
    println!("  mix type NAME    Identify what NAME is (builtin, keyword, alias, etc.)");
    println!("  mix config       Show runtime configuration");
    println!("  mix check FILE   Syntax-check a .mix file without executing");
    println!("  mix trace on|off Toggle statement tracing (handled in REPL)");
    println!("  time LINE        Time a command, pipeline, or Mix code (also: mix time LINE)");
    println!("  mix build        Build release binary, strip, install to ~/.local/bin/");
    println!("  mix clean        Remove build artifacts (cargo clean)");
    println!("  mix update       Git pull then build and install");
    println!("  mix test         Run cargo test suite");
    println!("  mix self check   Syntax-check ~/.mixrc and std/prelude.mix");
    println!("  mix history [P]  Show history (optionally filtered by pattern P)");
    println!("  mix reload       Re-execute ~/.mixrc");
    println!();
    println!("Mesh/ecosystem commands:");
    println!("  mix mesh         Check Bus mesh status and list active sockets");
    println!("  mix ports        List Bus port sockets with detail");
    println!("  mix ping SERVICE Check if a specific Bus service is reachable");
    println!();
    println!("Reference commands:");
    println!("  mix help         Capability overview (usage, subcommands, keywords)");
    println!("  mix tutorial     Guided walkthrough of Mix basics");
    println!("  mix examples     Runnable code snippets by category");
    println!("  mix man [TOPIC]  Read manual pages (no args = index)");
    println!("  mix keywords     List all reserved words");
    println!("  mix builtins [X] Full builtin list; X = a name, category, --json, or --names");
    println!("  mix what NAME    One-line description of a builtin or keyword");
    println!("  mix diff LANG    Translation cheatsheet (e.g. mix diff bash)");
    println!("  mix syntax       Shortcut for: mix man variables");
    println!("  mix operators    Shortcut for: mix man operators");
    println!();
    println!("Usage tracking:");
    println!("  mix stats            Overview: top 20 most-used across all categories");
    println!("  mix stats builtins   Builtin function usage counts");
    println!("  mix stats functions  User-defined function usage counts");
    println!("  mix stats aliases    Alias expansion counts");
    println!("  mix stats commands   External command usage counts");
    println!("  mix stats keywords   Keyword usage counts");
    println!("  mix stats sessions   Session history (start, duration, commands)");
    println!("  mix stats never      Builtins and keywords never used");
    println!("  mix stats modes      Per-mode event and completed-run breakdown");
    println!("  mix stats scripts N  Top N script basenames (default 10)");
    println!("  mix stats all        Aggregate all weekly stats files");
    println!("  mix stats week WEEK  Report one ISO week");
    println!("  mix stats raw        Print current stats as JSON");
    println!("  mix stats reset      Reset current stats");
    println!("  mix stats trend NAME Show counts over the last 30 recorded days");
    println!("  mix stats since DATE Aggregate JSON buckets since DATE");
    println!("  mix stats coverage D Static authorship coverage under directory D");
    println!("  mix stats query SQL  Run arbitrary SQL against mix.db");
    println!();
    println!("AI-powered commands (require Claude Code CLI):");
    println!("  mix fix DESC     Agent reads source, writes fix + test, rebuilds");
    println!("  mix extend DESC  Agent implements feature, tests, commits");
    println!("  mix review       Agent reviews recent changes");
    println!("  mix explain NAME Agent explains source code for a builtin");
    println!("  mix evolve       Agent scans TODO, picks highest-value item, implements");
    println!("  mix dogfood      Agent writes mix scripts, reports awkward patterns");
    println!("  mix fuzz         Agent generates random Mix code, finds crashes");
    println!("  mix teach        Agent creates tutorial from recent features");
    println!("  mix ask QUESTION One-shot Claude query with session context");
    println!("  mix chat         Interactive Claude with Mix-aware system prompt");
    println!();
    println!("Session and integration:");
    println!("  mix context      Export session state (vars, functions, aliases) as JSON");
    println!("  mix watch P CMD  Watch files matching pattern P, run CMD on changes");
    println!();
    println!("Mesh orchestration:");
    println!("  mix deploy SVC      Restart a Cosmix service (systemctl --user restart)");
    println!("  mix health [SVC]    Check Bus service health (socket connectivity)");
    println!("  mix logs SVC [-f]   Show service logs (journalctl, -f to follow)");
    println!();
    println!("Claude Bus port:");
    println!("  mix claude-start    Start cosmix-claude daemon (claude.sock)");
    println!("  mix claude-stop     Stop the Claude Bus port");
    println!("  mix claude-status   Check Claude port status");
}

/// Check for Claude Code CLI and run an AI-powered command.
fn cmd_ai(label: &str, prompt: &str) {
    // Check if claude CLI is available on PATH
    let has_claude = std::process::Command::new("which")
        .arg("claude")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !has_claude {
        eprintln!("mix {}: requires Claude Code CLI", label);
        eprintln!("Install: https://claude.ai/code");
        return;
    }

    println!("Running mix {}...", label);
    let status = std::process::Command::new("claude")
        .arg("-p")
        .arg(prompt)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match status {
        Ok(s) if !s.success() => {
            eprintln!("mix {}: claude exited with {}", label, s);
        }
        Err(e) => {
            eprintln!("mix {}: failed to run claude: {}", label, e);
        }
        _ => {}
    }
}

/// `mix context` / `mix snapshot`: export session state as JSON
fn cmd_context(eval: &Evaluator) {
    let mut map = serde_json::Map::new();

    // Variables
    let vars: serde_json::Map<String, serde_json::Value> = eval
        .scope()
        .all_variables()
        .into_iter()
        .map(|(k, v)| (k, value_to_json(&v)))
        .collect();
    map.insert("variables".to_string(), serde_json::Value::Object(vars));

    // Functions with signatures
    let funcs: Vec<serde_json::Value> = eval
        .scope()
        .function_names()
        .into_iter()
        .map(|(name, params)| {
            serde_json::json!({
                "name": name,
                "params": params,
            })
        })
        .collect();
    map.insert("functions".to_string(), serde_json::Value::Array(funcs));

    // Aliases
    let aliases: serde_json::Map<String, serde_json::Value> = eval
        .aliases()
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    map.insert("aliases".to_string(), serde_json::Value::Object(aliases));

    // Extensions
    let exts: Vec<serde_json::Value> = eval
        .extension_names()
        .into_iter()
        .map(serde_json::Value::String)
        .collect();
    map.insert("extensions".to_string(), serde_json::Value::Array(exts));

    // Runtime info
    map.insert("pid".to_string(), serde_json::json!(std::process::id()));
    map.insert(
        "cwd".to_string(),
        serde_json::json!(
            env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        ),
    );
    map.insert(
        "scope_depth".to_string(),
        serde_json::json!(eval.scope().frame_count()),
    );
    map.insert("trace".to_string(), serde_json::json!(eval.trace()));

    if let Some(file) = eval.current_file() {
        map.insert("current_file".to_string(), serde_json::json!(file));
    }
    if eval.current_line() > 0 {
        map.insert(
            "current_line".to_string(),
            serde_json::json!(eval.current_line()),
        );
    }

    if let Some(start) = START_TIME.get() {
        map.insert(
            "uptime_secs".to_string(),
            serde_json::json!(start.elapsed().as_secs()),
        );
    }

    // Bus address stack
    let addr_stack = eval.address_stack();
    if !addr_stack.is_empty() {
        map.insert("address_stack".to_string(), serde_json::json!(addr_stack));
    }

    let json = serde_json::Value::Object(map);
    println!(
        "{}",
        serde_json::to_string_pretty(&json).unwrap_or_default()
    );
}

/// Convert a Mix Value to serde_json::Value for context export.
fn value_to_json(val: &cosmix_mix::value::Value) -> serde_json::Value {
    use cosmix_mix::value::Value;
    match val {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Number(n) => serde_json::json!(n),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Nil => serde_json::Value::Null,
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        Value::Function(_) => serde_json::Value::Null,
        // Bytes have no native JSON type — emit Null, mirroring the
        // Function policy. See `cosmix-mix/src/bus.rs::value_to_json`
        // for the rationale; callers who need bytes in exported
        // context must `base64_encode($bytes)` explicitly.
        Value::Bytes(_) => serde_json::Value::Null,
        // Same policy as Bytes — a mutable byte buffer has no JSON form.
        Value::Buffer(_) => serde_json::Value::Null,
    }
}

/// Build a brief context summary for AI commands.
fn build_context_summary(eval: &Evaluator) -> String {
    let var_count = eval.scope().variable_names().len();
    let func_names: Vec<String> = eval
        .scope()
        .function_names()
        .into_iter()
        .map(|(name, params)| {
            if params.is_empty() {
                name
            } else {
                format!("{}({})", name, params.join(", "))
            }
        })
        .collect();
    let alias_count = eval.aliases().len();
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut summary = format!(
        "Mix session: {} variables, {} aliases, cwd={}",
        var_count, alias_count, cwd
    );
    if !func_names.is_empty() {
        summary.push_str(&format!(". Functions: {}", func_names.join(", ")));
    }
    summary
}

/// `mix ask QUESTION`: one-shot Claude query with session context
fn cmd_ask(question: &str, eval: &Evaluator) {
    let has_claude = std::process::Command::new("which")
        .arg("claude")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !has_claude {
        eprintln!("mix ask: requires Claude Code CLI");
        eprintln!("Install: https://claude.ai/code");
        return;
    }

    let context = build_context_summary(eval);
    let prompt = format!(
        "You are a Mix shell assistant. Mix is an ARexx-inspired scripting language \
         with Bus IPC (send/address/emit), PARSE, everything-is-a-string semantics. \
         Session context: {}. \n\nQuestion: {}",
        context, question
    );

    let status = std::process::Command::new("claude")
        .args(["-p", &prompt])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match status {
        Ok(s) if !s.success() => eprintln!("mix ask: claude exited with {}", s),
        Err(e) => eprintln!("mix ask: failed to run claude: {}", e),
        _ => {}
    }
}

/// `mix chat`: start interactive Claude with Mix-aware system prompt
fn cmd_chat(eval: &Evaluator) {
    let has_claude = std::process::Command::new("which")
        .arg("claude")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !has_claude {
        eprintln!("mix chat: requires Claude Code CLI");
        eprintln!("Install: https://claude.ai/code");
        return;
    }

    let context = build_context_summary(eval);
    let system_prompt = format!(
        "You are a Mix shell assistant embedded in an interactive Mix REPL session. \
         Mix is a pure-Rust ARexx-inspired scripting language for the Cosmix sovereign stack. \
         Key features: everything-is-a-string, PARSE instruction, Bus IPC (send/address/emit), \
         function scope isolation, $sigil variables, keyword-driven syntax (no braces). \
         Session: {}. \
         Help the user with Mix scripting, debugging, and Cosmix operations.",
        context
    );

    let status = std::process::Command::new("claude")
        .args(["--system-prompt", &system_prompt])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match status {
        Ok(s) if !s.success() => eprintln!("mix chat: claude exited with {}", s),
        Err(e) => eprintln!("mix chat: failed to run claude: {}", e),
        _ => {}
    }
}

/// `mix deploy SERVICE`: restart a Cosmix service
fn cmd_deploy(service: &str) {
    println!("Deploying {}...", service);

    // Try systemd user service first
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", &format!("cosmix-{}", service)])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("{}: restarted successfully", service);
            // Check status after restart
            let _ = std::process::Command::new("systemctl")
                .args([
                    "--user",
                    "status",
                    &format!("cosmix-{}", service),
                    "--no-pager",
                    "-l",
                ])
                .status();
        }
        Ok(s) => {
            eprintln!("{}: restart failed (exit {})", service, s);
            // Try direct socket check as fallback
            if let Some(dir) = bus_socket_dir() {
                let sock = dir.join(format!("{}.sock", service));
                if sock.exists() {
                    eprintln!("  Note: Bus socket exists at {}", sock.display());
                    eprintln!("  Service may not be managed by systemd");
                }
            }
        }
        Err(e) => eprintln!("{}: failed to run systemctl: {}", service, e),
    }
}

/// `mix health [SERVICE]`: check service health via Bus sockets
fn cmd_health(service: Option<&str>) {
    let Some(dir) = bus_socket_dir() else {
        println!("No Bus services found (no socket directory)");
        return;
    };

    if let Some(name) = service {
        // Check single service
        check_service_health(&dir, name);
    } else {
        // Check all services
        let socks = list_sock_files(&dir);
        if socks.is_empty() {
            println!("No Bus services found");
            return;
        }

        println!("Service health ({}):", dir.display());
        for entry in &socks {
            let name = entry.file_name().to_string_lossy().to_string();
            let service_name = name.trim_end_matches(".sock");
            check_service_health(&dir, service_name);
        }
    }
}

/// Check health of a single Bus service by attempting socket connection.
fn check_service_health(dir: &std::path::Path, service: &str) {
    let sock_name = if service.ends_with(".sock") {
        service.to_string()
    } else {
        format!("{}.sock", service)
    };
    let path = dir.join(&sock_name);

    if !path.exists() {
        println!("  {} \x1b[31m✗ not found\x1b[0m", service);
        return;
    }

    // Try connecting with a timeout
    let start = std::time::Instant::now();
    match UnixStream::connect(&path) {
        Ok(_stream) => {
            let latency = start.elapsed();
            println!(
                "  {} \x1b[32m✓ up\x1b[0m ({:.1}ms)",
                service,
                latency.as_secs_f64() * 1000.0
            );
        }
        Err(e) => {
            println!("  {} \x1b[31m✗ down\x1b[0m ({})", service, e);
        }
    }
}

/// `mix logs SERVICE [--follow]`: show service logs
fn cmd_logs(service: &str, follow: bool) {
    let unit = format!("cosmix-{}", service);

    let mut cmd_args = vec!["--user", "-u", &unit, "--no-pager"];
    if follow {
        cmd_args.push("-f");
    } else {
        cmd_args.push("-n");
        cmd_args.push("50");
    }

    let status = std::process::Command::new("journalctl")
        .args(&cmd_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match status {
        Ok(s) if !s.success() => {
            eprintln!("mix logs: journalctl exited with {}", s);
        }
        Err(e) => eprintln!("mix logs: failed to run journalctl: {}", e),
        _ => {}
    }
}

/// `mix claude-start|stop|status`: manage the Claude Bus port daemon
fn cmd_claude_port(action: &str) {
    let uid = crate::cosmix_paths::current_uid();
    let sock_path = format!("/run/user/{}/cosmix/ports/claude.sock", uid);

    match action {
        "start" => {
            // Check if already running
            if std::path::Path::new(&sock_path).exists() {
                println!("Claude port already running ({})", sock_path);
                return;
            }

            // Find cosmix-claude binary
            let candidates = [
                format!("{}/target/release/cosmix-claude", cosmix_src_str()),
                format!("{}/target/debug/cosmix-claude", cosmix_src_str()),
                "cosmix-claude".to_string(),
            ];

            let binary = candidates.iter().find(|p| {
                if p.contains('/') {
                    std::path::Path::new(p).is_file()
                } else {
                    std::process::Command::new("which")
                        .arg(p)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                }
            });

            match binary {
                Some(bin) => {
                    match std::process::Command::new(bin)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(child) => println!("Claude port started (pid {})", child.id()),
                        Err(e) => eprintln!("Failed to start cosmix-claude: {}", e),
                    }
                }
                None => {
                    eprintln!("cosmix-claude binary not found");
                    eprintln!(
                        "Build it: cd {} && cargo build -p cosmix-claude --release",
                        cosmix_src_str()
                    );
                }
            }
        }
        "stop" => {
            if !std::path::Path::new(&sock_path).exists() {
                println!("Claude port not running");
                return;
            }
            // Send a connection to trigger shutdown (or just remove socket)
            let _ = std::fs::remove_file(&sock_path);
            println!("Claude port stopped");
        }
        "status" => {
            if std::path::Path::new(&sock_path).exists() {
                // Try connecting
                match UnixStream::connect(&sock_path) {
                    Ok(_) => println!("Claude port: \x1b[32mrunning\x1b[0m ({})", sock_path),
                    Err(_) => {
                        println!("Claude port: \x1b[33mstale socket\x1b[0m ({})", sock_path);
                        println!("  Remove with: mix claude-stop");
                    }
                }
            } else {
                println!("Claude port: \x1b[31mnot running\x1b[0m");
                println!("  Start with: mix claude-start");
            }
        }
        _ => unreachable!(),
    }
}

/// `mix watch PATTERN COMMAND`: watch files for changes and run command
fn cmd_watch(pattern: &str, command: &str) {
    use std::path::Path;
    use std::time::{Duration, Instant};

    let watch_dir = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());

    println!(
        "Watching {} for changes to '{}' ...",
        watch_dir.display(),
        pattern
    );
    println!("Running: {}", command);
    println!("Press Ctrl-C to stop.\n");

    // Simple polling watcher — no extra dependency needed
    let mut last_run = Instant::now() - Duration::from_secs(10);
    let debounce = Duration::from_millis(500);

    // Collect initial file modification times
    let mut mtimes = collect_mtimes(&watch_dir, pattern);

    loop {
        std::thread::sleep(Duration::from_millis(250));

        let new_mtimes = collect_mtimes(&watch_dir, pattern);
        let mut changed_file = None;

        for (path, mtime) in &new_mtimes {
            match mtimes.get(path) {
                Some(old_mtime) if old_mtime != mtime => {
                    changed_file = Some(path.clone());
                    break;
                }
                None => {
                    changed_file = Some(path.clone());
                    break;
                }
                _ => {}
            }
        }

        if let Some(file) = changed_file
            && last_run.elapsed() >= debounce
        {
            println!("\x1b[33m[watch]\x1b[0m {} changed", file);
            let status = std::process::Command::new("sh")
                .args(["-c", command])
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();

            match status {
                Ok(s) if s.success() => println!("\x1b[32m[watch]\x1b[0m command succeeded\n"),
                Ok(s) => println!("\x1b[31m[watch]\x1b[0m command failed ({})\n", s),
                Err(e) => println!("\x1b[31m[watch]\x1b[0m failed to run: {}\n", e),
            }
            last_run = Instant::now();
            mtimes = collect_mtimes(&watch_dir, pattern);
        }
    }
}

/// Collect modification times for files matching a glob pattern.
fn collect_mtimes(
    dir: &std::path::Path,
    pattern: &str,
) -> std::collections::HashMap<String, std::time::SystemTime> {
    let mut result = std::collections::HashMap::new();

    // Simple glob matching: support *.ext and **/*.ext patterns
    let is_recursive = pattern.contains("**");
    let file_pattern = pattern.trim_start_matches("**/").trim_start_matches("*/");

    fn visit_dir(
        dir: &std::path::Path,
        file_pattern: &str,
        recursive: bool,
        result: &mut std::collections::HashMap<String, std::time::SystemTime>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip hidden dirs and target/
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "target" {
                    continue;
                }
                if recursive {
                    visit_dir(&path, file_pattern, recursive, result);
                }
            } else if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if matches_simple_glob(&name, file_pattern)
                    && let Ok(meta) = entry.metadata()
                    && let Ok(mtime) = meta.modified()
                {
                    result.insert(path.to_string_lossy().to_string(), mtime);
                }
            }
        }
    }

    visit_dir(dir, file_pattern, is_recursive, &mut result);
    result
}

/// Simple glob matching: supports *.ext and exact name matches.
fn matches_simple_glob(filename: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(ext) = pattern.strip_prefix("*.") {
        return filename.ends_with(&format!(".{}", ext));
    }
    filename == pattern
}

/// Locate the Bus socket directory, returning the first that exists.
fn bus_socket_dir() -> Option<std::path::PathBuf> {
    for candidate in &["/run/bus", "/tmp/bus"] {
        let p = std::path::PathBuf::from(candidate);
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// List `.sock` files inside a directory.
fn list_sock_files(dir: &std::path::Path) -> Vec<std::fs::DirEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sock"))
        .collect()
}

/// Query the local noded broker for its registered Bus service roster.
///
/// The mesh is **TCP noded** (`ws://<wg_ip>:4200/ws`, resolved like the
/// `send` keyword does) — there is no `/run/bus` unix socket. Returns the
/// resolved broker URL + the service-name list, or an error string for a
/// clean "no mesh" message. One-shot: builds its own current-thread
/// runtime so the sync meta-command path can call the async client.
fn query_noded_services() -> Result<(String, Vec<String>), String> {
    let url = crate::node_config::resolve_noded_url();
    // The meta-command path already runs inside the binary's tokio
    // runtime, so a nested `block_on` here would panic ("Cannot start a
    // runtime from within a runtime"). Run the one-shot client on a
    // dedicated thread with its own current-thread runtime.
    let url_for_thread = url.clone();
    let handle = std::thread::Builder::new()
        .name("noded-query".into())
        .spawn(move || -> Result<Vec<String>, String> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("runtime: {e}"))?;
            rt.block_on(async {
                let query = async {
                    let client = cosmix_lib_client::NodedClient::connect_anonymous(&url_for_thread)
                        .await
                        .map_err(|e| format!("connect {url_for_thread}: {e}"))?;
                    client
                        .list_services()
                        .await
                        .map_err(|e| format!("list_services: {e}"))
                };
                // Fail fast — a diagnostic command must not hang on the OS
                // TCP timeout when the wg_ip is stale/blackholed.
                match tokio::time::timeout(std::time::Duration::from_secs(5), query).await {
                    Ok(r) => r,
                    Err(_) => Err(format!("timed out after 5s contacting {url_for_thread}")),
                }
            })
        })
        .map_err(|e| format!("spawn noded query thread: {e}"))?;
    let services = handle
        .join()
        .map_err(|_| "noded query thread panicked".to_string())??;
    Ok((url, services))
}

/// `mix mesh`: Bus mesh status — the local noded's registered services.
fn cmd_mesh() {
    match query_noded_services() {
        Ok((url, services)) => {
            println!("Bus mesh active via local noded ({url})");
            if services.is_empty() {
                println!("  (no services registered)");
            } else {
                for s in &services {
                    println!("  {s}");
                }
                println!("{} service(s) registered", services.len());
            }
        }
        Err(e) => println!("No mesh — local noded not reachable ({e})"),
    }
}

/// `mix ports`: list the Bus services registered with the local noded.
fn cmd_ports() {
    match query_noded_services() {
        Ok((url, services)) => {
            if services.is_empty() {
                println!("No Bus ports registered ({url})");
            } else {
                println!("Bus ports via local noded ({url})");
                for s in &services {
                    println!("  {s}");
                }
                println!("{} port(s)", services.len());
            }
        }
        Err(e) => println!("No Bus ports active ({e})"),
    }
}

/// `mix ping SERVICE`: is `SERVICE` registered with the local noded?
fn cmd_ping(service: &str) {
    match query_noded_services() {
        Ok((_url, services)) => {
            if service == "noded" {
                // A successful query means we connected to the broker, so
                // noded itself is reachable (it isn't in its own list).
                println!("noded: reachable (local broker)");
            } else if services.iter().any(|s| s == service) {
                println!("{service}: reachable (registered with noded)");
            } else {
                println!("{service}: not found");
            }
        }
        Err(e) => println!("{service}: not reachable ({e})"),
    }
}

const DEFAULT_MAN_URL: &str = "https://cosmix.dev/mix";
const MAN_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAN_CLOCK_SKEW_ALLOWANCE: Duration = Duration::from_secs(60);
const MAN_CONNECT_TIMEOUT: Duration = Duration::from_millis(750);
const MAN_READ_TIMEOUT: Duration = Duration::from_millis(1_500);
const MAN_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_MAN_PAGE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManSource {
    Auto,
    Local,
}

struct CachedManPage {
    content: String,
    fresh: bool,
}

fn parse_man_source(value: Option<&str>) -> Result<ManSource, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(ManSource::Auto),
        Some(value) if value.eq_ignore_ascii_case("auto") => Ok(ManSource::Auto),
        Some(value) if value.eq_ignore_ascii_case("local") => Ok(ManSource::Local),
        Some(value) => Err(format!(
            "invalid COSMIX_MAN_SOURCE '{value}' (expected auto or local)"
        )),
    }
}

fn man_base_url(value: Option<&str>) -> String {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    value
        .unwrap_or(DEFAULT_MAN_URL)
        .trim_end_matches('/')
        .to_string()
}

fn man_page_filename(topic: Option<&str>) -> Result<String, String> {
    let Some(topic) = topic else {
        return Ok("README.md".to_string());
    };
    if topic.is_empty()
        || !topic
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "invalid manual topic '{topic}' (use letters, numbers, '-' or '_')"
        ));
    }
    Ok(format!("{topic}.md"))
}

fn man_page_url(base_url: &str, filename: &str) -> String {
    format!("{}/{filename}", base_url.trim_end_matches('/'))
}

/// The manual's source of truth lives in the checkout at
/// `$COSMIX/docs/mix` (also what cosmix.dev/mix/ serves). With a known
/// root that is the only candidate; without one, the documented default
/// clone location is tried.
fn man_dir_candidates(home: Option<&Path>, root: Option<&Path>) -> Vec<PathBuf> {
    match (root, home) {
        (Some(root), _) => vec![root.join("docs/mix")],
        (None, Some(home)) => vec![crate::cosmix_paths::default_root(home).join("docs/mix")],
        (None, None) => Vec::new(),
    }
}

fn local_man_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let root = crate::cosmix_paths::cosmix_root();
    man_dir_candidates(home.as_deref(), root.as_deref())
}

/// Locate the first existing local manual directory. The canonical online
/// source is handled by `cmd_man`; this is its checkout fallback.
fn resolve_man_dir() -> PathBuf {
    let dirs = local_man_dirs();
    dirs.iter()
        .find(|dir| dir.is_dir())
        .cloned()
        .unwrap_or_else(|| {
            dirs.into_iter()
                .next()
                .unwrap_or_else(|| PathBuf::from(".cosmix/mix"))
        })
}

fn read_local_man_page(filename: &str) -> Option<String> {
    local_man_dirs()
        .into_iter()
        .find_map(|dir| std::fs::read_to_string(dir.join(filename)).ok())
}

fn man_cache_root(xdg_cache_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    xdg_cache_home
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".cache")))
        .map(|root| root.join("cosmix/man"))
}

fn man_url_fingerprint(value: &str) -> u64 {
    // Stable FNV-1a: unlike DefaultHasher this keeps override cache paths
    // consistent across Rust releases and processes.
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn man_cache_path(root: &Path, base_url: &str, filename: &str) -> PathBuf {
    if base_url == DEFAULT_MAN_URL {
        root.join(filename)
    } else {
        root.join("sources")
            .join(format!("{:016x}", man_url_fingerprint(base_url)))
            .join(filename)
    }
}

fn configured_man_cache_path(base_url: &str, filename: &str) -> Option<PathBuf> {
    let xdg_cache_home = env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
    let home = dirs::home_dir();
    man_cache_root(xdg_cache_home.as_deref(), home.as_deref())
        .map(|root| man_cache_path(&root, base_url, filename))
}

fn cache_is_fresh(modified: SystemTime, now: SystemTime, ttl: Duration) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age <= ttl,
        Err(clock_skew) => clock_skew.duration() <= MAN_CLOCK_SKEW_ALLOWANCE,
    }
}

fn read_cached_man_page(path: &Path) -> Option<CachedManPage> {
    let content = std::fs::read_to_string(path).ok()?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(CachedManPage {
        content,
        fresh: cache_is_fresh(modified, SystemTime::now(), MAN_CACHE_TTL),
    })
}

fn write_cached_man_page(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;

    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("page.md");
    let temp = parent.join(format!(".{filename}.{}.{}.tmp", std::process::id(), nonce));

    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn valid_man_page_response(content_type: Option<&str>, content: &str) -> bool {
    if content.trim().is_empty()
        || content_type.is_some_and(|value| value.to_ascii_lowercase().contains("html"))
    {
        return false;
    }

    let trimmed = content.trim_start();
    !["<!doctype", "<html"].iter().any(|prefix| {
        trimmed
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    })
}

fn fetch_man_page_blocking(url: &str) -> Result<String, ()> {
    let agent = ureq::AgentBuilder::new()
        .https_only(true)
        .redirects(2)
        .timeout_connect(MAN_CONNECT_TIMEOUT)
        .timeout_read(MAN_READ_TIMEOUT)
        .timeout(MAN_REQUEST_TIMEOUT)
        .build();
    let response = agent.get(url).call().map_err(|_| ())?;
    if response.status() != 200 {
        return Err(());
    }
    let content_type = response.header("Content-Type").map(str::to_owned);
    let mut reader = response.into_reader().take(MAX_MAN_PAGE_BYTES + 1);
    let mut content = String::new();
    reader.read_to_string(&mut content).map_err(|_| ())?;
    if content.len() as u64 > MAX_MAN_PAGE_BYTES
        || !valid_man_page_response(content_type.as_deref(), &content)
    {
        return Err(());
    }
    Ok(content)
}

/// Guards against unbounded worker accumulation: only one man-fetch thread may
/// be in flight at a time. In a one-shot CLI this is moot (process exit reaps
/// the thread), but in a long-lived REPL a permanently wedged resolver would
/// otherwise let repeated `mix man` calls pile up detached, still-blocked
/// threads. While a prior worker is stuck, new calls skip the network and fall
/// straight through to the local/stale fallback.
static MAN_FETCH_INFLIGHT: AtomicBool = AtomicBool::new(false);

fn run_man_fetch_with_timeout<F>(fetch: F, timeout: Duration) -> Result<String, ()>
where
    F: FnOnce() -> Result<String, ()> + Send + 'static,
{
    if MAN_FETCH_INFLIGHT
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // A previous worker is still running (possibly wedged) — don't spawn
        // another; let resolution fall through to the local fallback.
        return Err(());
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    let spawn = thread::Builder::new()
        .name("mix-man-fetch".to_string())
        .spawn(move || {
            let result = fetch();
            MAN_FETCH_INFLIGHT.store(false, Ordering::Release);
            let _ = sender.send(result);
        });
    if spawn.is_err() {
        MAN_FETCH_INFLIGHT.store(false, Ordering::Release);
        return Err(());
    }

    receiver.recv_timeout(timeout).map_err(|_| ())?
}

fn fetch_man_page(url: &str) -> Result<String, ()> {
    let url = url.to_owned();
    run_man_fetch_with_timeout(move || fetch_man_page_blocking(&url), MAN_REQUEST_TIMEOUT)
}

fn available_man_topics(man_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(man_dir) else {
        return Vec::new();
    };
    let mut topics: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") && name != "README.md" {
                Some(name.trim_end_matches(".md").to_string())
            } else {
                None
            }
        })
        .collect();
    topics.sort();
    topics
}

fn print_man_not_found(topic: Option<&str>, base_url: &str) {
    eprintln!("mix man: no manual page for '{}'", topic.unwrap_or("index"));
    eprintln!();

    let topics = available_man_topics(&resolve_man_dir());
    if topics.is_empty() {
        eprintln!("Browse the manual at {base_url}/");
        eprintln!("Local fallback looked in $COSMIX/docs/mix (default root ~/Projects/cosmix).");
        eprintln!(
            "Use COSMIX_MAN_SOURCE=local, or clone: git clone https://github.com/markc/cosmix ~/Projects/cosmix"
        );
    } else {
        eprintln!("Available topics: {}", topics.join(", "));
    }
}

/// `mix man [TOPIC]`: resolve the canonical online manual with a local fallback.
fn cmd_man(topic: Option<&str>) {
    let source = match parse_man_source(env::var("COSMIX_MAN_SOURCE").ok().as_deref()) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("mix man: {error}");
            return;
        }
    };
    let filename = match man_page_filename(topic) {
        Ok(filename) => filename,
        Err(error) => {
            eprintln!("mix man: {error}");
            return;
        }
    };
    let base_url = man_base_url(env::var("COSMIX_MAN_URL").ok().as_deref());
    let cache_path = configured_man_cache_path(&base_url, &filename);

    if source == ManSource::Local {
        if let Some(content) = read_local_man_page(&filename) {
            print!("{content}");
            return;
        }
        if let Some(cached) = cache_path.as_deref().and_then(read_cached_man_page) {
            print!("{}", cached.content);
            return;
        }
        print_man_not_found(topic, &base_url);
        return;
    }

    let cached = cache_path.as_deref().and_then(read_cached_man_page);
    if let Some(cached) = cached.as_ref().filter(|cached| cached.fresh) {
        print!("{}", cached.content);
        return;
    }

    let url = man_page_url(&base_url, &filename);
    if let Ok(content) = fetch_man_page(&url) {
        if let Some(path) = cache_path.as_deref() {
            let _ = write_cached_man_page(path, &content);
        }
        print!("{content}");
        return;
    }

    if let Some(content) = read_local_man_page(&filename) {
        print!("{content}");
        return;
    }
    if let Some(cached) = cached {
        print!("{}", cached.content);
        return;
    }

    print_man_not_found(topic, &base_url);
}

/// `mix help` (and `mix --help`): the markdown-friendly capability-discovery
/// overview. The builtin *function* list lives in `mix builtins` — this
/// surface signposts it rather than duplicating it, so it stays a fast
/// "what can this version do" reference for humans and agents alike.
fn cmd_help_full(version: &str) {
    print!("{}", help_overview_string(version));
}

/// `mix keywords`: list all reserved words
fn cmd_keywords() {
    println!("Mix reserved words:");
    println!();
    for kw in MIX_KEYWORDS {
        println!("  {}", kw);
    }
}

/// `mix builtins [--json | --names | <name> | <category>]`: the full builtin
/// remit for capability discovery. No argument = every builtin (the default
/// an agent should reach for — no category steering required); `--json` = the
/// machine-readable table; `--names` = the bare name list; a `<name>` = a
/// detail block; a `<category>` = that group. Also reached via `mix --builtins`.
pub fn cmd_builtins(args: &[&str], version: &str) {
    // At most one argument: a flag, a name, or a category. Reject trailing
    // junk (`mix builtins --json oops`) rather than silently ignoring it.
    if args.len() > 1 {
        eprintln!(
            "mix builtins: too many arguments (expected at most one: --json, --data, --names, a name, or a category)"
        );
        return;
    }
    match args.first().copied() {
        Some("--json") => println!("{}", builtins_json_string()),
        Some("--data") => println!("{}", builtins_data_string()),
        Some("--names") => print!("{}", builtins_names_string()),
        Some(token) => {
            // Name detail is tried before a category filter.
            if let Some(detail) = builtin_detail_string(token) {
                print!("{}", detail);
            } else if builtin_categories().contains(&token) {
                print!("{}", builtins_report_string(Some(token)));
            } else {
                eprintln!("mix builtins: unknown builtin or category '{}'", token);
                eprintln!("Categories: {}", builtin_categories().join(", "));
                eprintln!("Run 'mix builtins' for the full list, or 'mix builtins --names'.");
            }
        }
        None => print!(
            "# mix {} — {} builtins  (`mix builtins <name>` for one · `--json`/`--data` machine-readable)\n\n{}",
            version,
            builtin_count(),
            builtins_report_string(None)
        ),
    }
}

/// `mix what NAME`: one-line description of a builtin or keyword
fn cmd_what(name: &str) {
    // Check builtins (pure + HOF + extras) via the unified iterator.
    for (n, _, desc) in all_builtin_entries() {
        if n == name {
            println!("{}: {}", name, desc);
            return;
        }
    }

    // Check keywords
    for (n, desc) in KEYWORD_DESCRIPTIONS {
        if *n == name {
            println!("{}: {}", name, desc);
            return;
        }
    }

    println!("{}: unknown", name);
}

/// `mix tutorial`: guided walkthrough of Mix basics
fn cmd_tutorial() {
    print!(
        r#"Mix Tutorial — Learn the Basics
================================

1. VARIABLES AND STRINGS
   Variables start with $. Double-quoted strings and heredocs support
   interpolation via ${{name}}. Lookup walks scope first, then the
   process environment, then prints "nil" when the name is unbound
   in both. Bare $X outside ${{...}} stays literal (awk-style $5
   is safe to print).

   $name = "world"
   print "Hello, ${{name}}!"          -- Hello, world!
   $greeting = "Hi ${{name}}"         -- interpolation in assignment
   print "PATH=${{PATH}}"             -- env-fallback: prints $PATH

2. ARITHMETIC
   + does math when both sides are numbers; .. always concatenates.

   print 2 + 3                        -- 5
   print "ab" .. "cd"                 -- abcd
   print "2" + "3"                    -- 5 (both parse as numbers)

3. LISTS AND MAPS
   Lists use [], maps use {{}}. Dot access for map fields.

   $colors = ["red", "green", "blue"]
   print $colors[1]                   -- green (0-based)
   $user = {{name: "Alice", age: 30}}
   print $user.name                   -- Alice

4. CONTROL FLOW
   Block keywords end with end, next, or done.

   if $x > 0 then print "positive" end
   for $i = 1 to 3 do print $i next  -- 1 2 3
   for each $c in $colors do print $c next
   while $n > 0 do $n = $n - 1 done

5. FUNCTIONS
   Defined with function/end. Scope is isolated (no closures).

   function greet($who)
     return "Hello, " .. $who
   end
   print greet("Mix")                 -- Hello, Mix

6. PARSE INSTRUCTION
   ARexx-style string decomposition.

   $line = "John 42 Melbourne"
   parse $line with $name " " $rest
   print $name                        -- John
   parse $rest with $age " " $city
   print $city                        -- Melbourne

7. SHELL INTEGRATION
   Run external commands directly or capture their output.

   ls -la                             -- bare command, streams output
   $files = $(ls *.mix)               -- capture into variable
   sh "echo done"                     -- explicit shell execution

8. FILE I/O
   Read, write, and find files with builtins.

   $text = read_file("notes.txt")
   write_file("out.txt", $text)
   $mixes = glob("*.mix")            -- list of matching paths

9. THE MIX META COMMAND
   Introspect your session from the REPL.

   mix vars                           -- list all variables
   mix help                           -- full language reference
   mix man functions                  -- read a manual page
   mix examples                       -- runnable code snippets

Tip: paste any example above into the Mix REPL to try it out.
"#
    );
}

/// `mix examples`: runnable code snippets organized by category
fn cmd_examples() {
    print!(
        r#"Mix Examples — Copy-paste into the REPL
========================================

--- Strings ---
print split("one,two,three", ",")          -- [one, two, three]
print join(["a", "b", "c"], "-")           -- a-b-c
print replace("hello world", "world", "Mix")  -- hello Mix
$who = "Mix"; print "Hello, ${{who}}!"     -- Hello, Mix!

--- Lists ---
$nums = [3, 1, 4, 1, 5]
print sort($nums)                          -- [1, 1, 3, 4, 5]
push $nums, 9; print pop($nums)            -- 9
for each $n in $nums do print $n next

--- Maps ---
$m = {{host: "localhost", port: 8080}}
print $m.host                              -- localhost
print keys($m)                             -- [host, port]
print values($m)                           -- [localhost, 8080]
for each $k in keys($m) do print $k .. " = " .. $m[$k] next

--- Files ---
write_file("/tmp/mix-test.txt", "hello\n")
print read_file("/tmp/mix-test.txt")       -- hello
$files = glob("/tmp/mix-*"); print $files

--- System ---
print env("HOME")                          -- /home/user
$out = exec("uname -r"); print $out
print pid()                                -- current process id
$a = args(); print $a                      -- script arguments list

--- PARSE ---
$line = "Alice 30 Engineer"
parse $line with $name " " $rest
print $name                                -- Alice
$csv = "red,green,blue"
parse $csv with $a "," $b "," $c
print $b                                   -- green
$path = "/usr/local/bin"
parse $path with "/" $a "/" $b "/" $c
print $b                                   -- local
"#
    );
}

/// Read OS ID from /etc/os-release
fn read_os_id() -> String {
    if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
        for line in contents.lines() {
            if let Some(id) = line.strip_prefix("ID=") {
                return id.trim_matches('"').to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Look up a command on PATH
fn which_command(name: &str) -> Option<String> {
    let path_var = env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let full = format!("{}/{}", dir, name);
        if std::path::Path::new(&full).is_file() {
            return Some(full);
        }
    }
    None
}

#[cfg(test)]
mod man_resolver_tests {
    use super::*;

    #[test]
    fn source_override_accepts_auto_and_local_only() {
        assert_eq!(parse_man_source(None), Ok(ManSource::Auto));
        assert_eq!(parse_man_source(Some("")), Ok(ManSource::Auto));
        assert_eq!(parse_man_source(Some(" AUTO ")), Ok(ManSource::Auto));
        assert_eq!(parse_man_source(Some("Local")), Ok(ManSource::Local));
        assert!(parse_man_source(Some("online")).is_err());
    }

    #[test]
    fn base_url_and_page_url_have_one_separator() {
        assert_eq!(man_base_url(None), DEFAULT_MAN_URL);
        assert_eq!(
            man_base_url(Some(" https://manual.example.test/mix/// ")),
            "https://manual.example.test/mix"
        );
        assert_eq!(
            man_page_url("https://manual.example.test/mix/", "README.md"),
            "https://manual.example.test/mix/README.md"
        );
    }

    #[test]
    fn topic_filename_is_path_safe() {
        assert_eq!(man_page_filename(None).as_deref(), Ok("README.md"));
        assert_eq!(
            man_page_filename(Some("shell-mode")).as_deref(),
            Ok("shell-mode.md")
        );
        assert!(man_page_filename(Some("../README")).is_err());
        assert!(man_page_filename(Some("a/b")).is_err());
        assert!(man_page_filename(Some("")).is_err());
    }

    #[test]
    fn local_candidates_are_keyed_off_the_root() {
        // a known root is the only candidate — no dot-dir fallbacks
        assert_eq!(
            man_dir_candidates(Some(Path::new("/home/tester")), Some(Path::new("/srv/cosmix"))),
            vec![PathBuf::from("/srv/cosmix/docs/mix")]
        );
        // no root: the documented default clone location
        assert_eq!(
            man_dir_candidates(Some(Path::new("/home/tester")), None),
            vec![PathBuf::from("/home/tester/Projects/cosmix/docs/mix")]
        );
        assert!(man_dir_candidates(None, None).is_empty());
    }

    #[test]
    fn cache_root_is_xdg_aware() {
        assert_eq!(
            man_cache_root(
                Some(Path::new("/var/cache/tester")),
                Some(Path::new("/home/tester"))
            ),
            Some(PathBuf::from("/var/cache/tester/cosmix/man"))
        );
        assert_eq!(
            man_cache_root(None, Some(Path::new("/home/tester"))),
            Some(PathBuf::from("/home/tester/.cache/cosmix/man"))
        );
        assert_eq!(
            man_cache_root(
                Some(Path::new("relative/cache")),
                Some(Path::new("/home/tester"))
            ),
            Some(PathBuf::from("/home/tester/.cache/cosmix/man"))
        );
        assert_eq!(man_cache_root(None, None), None);
    }

    #[test]
    fn url_override_has_an_isolated_cache_namespace() {
        let root = Path::new("/cache/cosmix/man");
        assert_eq!(
            man_cache_path(root, DEFAULT_MAN_URL, "syntax.md"),
            PathBuf::from("/cache/cosmix/man/syntax.md")
        );
        let override_path = man_cache_path(root, "http://127.0.0.1:8080/mix", "syntax.md");
        assert!(override_path.starts_with("/cache/cosmix/man/sources"));
        assert!(override_path.ends_with("syntax.md"));
        assert_ne!(
            override_path,
            man_cache_path(root, DEFAULT_MAN_URL, "syntax.md")
        );
        assert_eq!(
            override_path,
            man_cache_path(root, "http://127.0.0.1:8080/mix", "syntax.md")
        );
    }

    #[test]
    fn cache_freshness_handles_boundary_and_clock_skew() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        assert!(cache_is_fresh(now - MAN_CACHE_TTL, now, MAN_CACHE_TTL));
        assert!(!cache_is_fresh(
            now - MAN_CACHE_TTL - Duration::from_secs(1),
            now,
            MAN_CACHE_TTL
        ));
        assert!(cache_is_fresh(
            now + Duration::from_secs(1),
            now,
            MAN_CACHE_TTL
        ));
        assert!(!cache_is_fresh(
            now + MAN_CLOCK_SKEW_ALLOWANCE + Duration::from_secs(1),
            now,
            MAN_CACHE_TTL
        ));
    }

    #[test]
    fn fetched_page_validation_rejects_empty_and_html_responses() {
        assert!(valid_man_page_response(
            Some("text/markdown; charset=utf-8"),
            "# syntax\n"
        ));
        assert!(!valid_man_page_response(Some("text/markdown"), " \n\t"));
        assert!(!valid_man_page_response(
            Some("text/html; charset=utf-8"),
            "# disguised error\n"
        ));
        assert!(!valid_man_page_response(
            Some("text/plain"),
            " \n<!DOCTYPE html><html></html>"
        ));
        assert!(!valid_man_page_response(
            None,
            "<HtMl><body>error</body></hTmL>"
        ));
    }

    #[test]
    fn fetch_wall_clock_timeout_does_not_wait_for_worker() {
        let (release_sender, release_receiver) = mpsc::channel::<()>();
        let result = run_man_fetch_with_timeout(
            move || {
                let _ = release_receiver.recv();
                Ok("late response".to_string())
            },
            Duration::from_millis(10),
        );
        assert!(result.is_err());
        drop(release_sender);
    }
}

#[cfg(test)]
mod builtins_introspection_tests {
    use super::*;

    #[test]
    fn json_is_valid_and_complete() {
        let s = builtins_json_string();
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        let arr = v.as_array().expect("top-level array");
        assert_eq!(arr.len(), builtin_count());
        for obj in arr {
            for k in ["name", "category", "capability", "signature", "description"] {
                assert!(
                    obj.get(k).and_then(|x| x.as_str()).is_some(),
                    "entry missing string key '{k}'"
                );
            }
        }
    }

    #[test]
    fn every_capability_is_a_known_kebab() {
        let known = [
            "pure",
            "fs-read",
            "fs-write",
            "network",
            "process",
            "env",
            "db",
            "jmap",
            "bus",
            "statement",
        ];
        for row in all_builtin_rows() {
            assert!(
                known.contains(&row.capability),
                "builtin '{}' has unknown capability '{}'",
                row.name,
                row.capability
            );
        }
    }

    #[test]
    fn names_count_matches_registry() {
        assert_eq!(builtins_names_string().lines().count(), builtin_count());
    }

    /// The default `mix builtins` must show the FULL remit — every builtin,
    /// no category silently hidden (regression guard for the hardcoded-
    /// category list that dropped db/jmap/bus/datastar).
    #[test]
    fn default_report_shows_every_builtin() {
        let rows = builtins_report_string(None)
            .lines()
            .filter(|l| l.starts_with("  "))
            .count();
        assert_eq!(
            rows,
            builtin_count(),
            "default `mix builtins` hid some builtins"
        );
        // and the derived category list covers every entry's category.
        let cats = builtin_categories();
        for row in all_builtin_rows() {
            assert!(
                cats.contains(&row.category),
                "category '{}' of '{}' missing from the report",
                row.category,
                row.name
            );
        }
    }

    #[test]
    fn detail_present_for_known_absent_for_unknown() {
        assert!(builtin_detail_string("length").is_some());
        assert!(builtin_detail_string("definitely_not_a_builtin").is_none());
    }

    /// Drift guard for the increment-2 signature fan-out: any *populated*
    /// signature must lead with its own builtin name (catches copy-paste).
    /// Vacuously true today (all signatures empty), enforced once populated.
    #[test]
    fn populated_signatures_lead_with_the_builtin_name() {
        for row in all_builtin_rows() {
            let sig = row.signature();
            let lead: String = sig
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '(')
                .collect();
            assert_eq!(
                lead, row.name,
                "signature/name drift on '{}': {sig}",
                row.name
            );
        }
    }

    /// The metadata_schema-1 JSON surface: spot-check a known entry for
    /// the D2 contract fields, and require every entry to carry them.
    #[test]
    fn builtins_json_carries_structured_metadata() {
        let parsed: serde_json::Value =
            serde_json::from_str(&builtins_json_string()).expect("valid JSON");
        let arr = parsed.as_array().expect("top-level array");
        assert_eq!(arr.len(), builtin_count());
        for entry in arr {
            for key in [
                "metadata_schema",
                "kind",
                "name",
                "category",
                "capability",
                "conditional_capabilities",
                "description",
                "signature",
                "arity",
                "args",
                "returns",
                "effects",
                "operational_failure",
            ] {
                assert!(
                    entry.get(key).is_some(),
                    "entry {} missing key '{key}'",
                    entry["name"]
                );
            }
            assert_eq!(entry["metadata_schema"], 1);
        }
        let substr = arr
            .iter()
            .find(|e| e["name"] == "substr")
            .expect("substr present");
        assert_eq!(substr["kind"], "builtin");
        assert_eq!(substr["arity"]["min"], 2);
        assert_eq!(substr["arity"]["max"], 3);
        assert_eq!(substr["args"][2]["required"], false);
        let print = arr
            .iter()
            .find(|e| e["name"] == "print")
            .expect("print present");
        assert_eq!(print["kind"], "statement");
        assert_eq!(print["capability"], "statement");
        assert_eq!(print["arity"]["max"], serde_json::Value::Null);
    }

    /// `--data` must be strict-data parseable and mirror the JSON count.
    #[test]
    fn builtins_data_round_trips_through_parse_data() {
        let data = builtins_data_string();
        let v = cosmix_mix::parse_data(&data).expect("strict-data parseable");
        match &v {
            Value::List(items) => assert_eq!(items.len(), builtin_count()),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn help_overview_signposts_builtins_and_omits_the_function_list() {
        let h = help_overview_string("9.9.9");
        assert!(h.contains("mix builtins"), "must signpost `mix builtins`");
        assert!(h.contains("9.9.9"), "must show the version");
        assert!(h.contains("cosmix.dev/mix"), "must link the manual");
        // The builtin *function* list is delegated to `mix builtins`, not
        // inlined here: a representative one-line description must be absent.
        assert!(
            !h.contains("Return length of string, list, or map"),
            "overview must not duplicate the builtin function listing"
        );
    }
}

#[cfg(test)]
mod version_line_tests {
    use super::*;

    /// The REPL `mix --version`, the CLI `--version` flag, and the status
    /// header all route through `version_line`; guard the exact `mix X.Y.Z`
    /// shape so a REPL user and a CLI user see the identical string. The REPL
    /// arm regressed before this (fell through to the meta-command help dump).
    #[test]
    fn version_line_matches_the_cli_mix_prefix_format() {
        assert_eq!(version_line("9.9.9"), "mix 9.9.9");
        assert_eq!(version_line("0.32.1"), "mix 0.32.1");
    }
}
