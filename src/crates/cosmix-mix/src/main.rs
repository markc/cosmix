// Crate-wide CLI output policy. Declared before the modules so every
// print!/println!/eprintln! in main, meta, the REPL, and shell surface
// resolves to these BrokenPipe-tolerant writers. Internal process-pipe I/O uses
// `Write` directly and remains under Rust's normal SIGPIPE-ignore policy.
macro_rules! print {
    ($($arg:tt)*) => {{
        crate::write_cli_output(crate::CliStream::Stdout, format_args!($($arg)*), false)
    }};
}

macro_rules! println {
    () => {{ crate::write_cli_output(crate::CliStream::Stdout, format_args!(""), true) }};
    ($($arg:tt)*) => {{
        crate::write_cli_output(crate::CliStream::Stdout, format_args!($($arg)*), true)
    }};
}

macro_rules! eprintln {
    () => {{ crate::write_cli_output(crate::CliStream::Stderr, format_args!(""), true) }};
    ($($arg:tt)*) => {{
        crate::write_cli_output(crate::CliStream::Stderr, format_args!($($arg)*), true)
    }};
}

mod bus;
mod completion;
mod cosmix_paths;
mod edit;
pub mod editor;
mod exec;
mod job_control;
mod jobs;
mod lint;
mod meta;
mod native_session;
mod node_config;
mod repl;
mod repl_editor;
mod serve_runtime;
mod result_fd;
mod session_task;
mod session_execute;
mod session_state;
mod session_status;
mod shell;
mod shell_handler;
mod stats_coverage;
mod stats_io;

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process;

use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::stats::{ExecutionMode, StatsContext, UsageStats};
use cosmix_mix::value::Value;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy)]
enum CliStream {
    Stdout,
    Stderr,
}

/// Write one CLI/meta fragment, treating a closed downstream pipe as normal.
/// Other output failures remain loud because they indicate a real local I/O
/// fault. Ignoring only BrokenPipe is portable and, unlike changing SIGPIPE
/// process-wide, cannot kill Mix's internal child-stdin writer threads.
fn write_cli_output(stream: CliStream, args: std::fmt::Arguments<'_>, newline: bool) {
    fn write_to(mut writer: impl Write, args: std::fmt::Arguments<'_>, newline: bool) {
        let result = writer.write_fmt(args).and_then(|()| {
            if newline {
                writer.write_all(b"\n")
            } else {
                Ok(())
            }
        });
        if let Err(error) = result
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            panic!("failed printing CLI output: {error}");
        }
    }

    match stream {
        CliStream::Stdout => write_to(io::stdout().lock(), args, newline),
        CliStream::Stderr => write_to(io::stderr().lock(), args, newline),
    }
}

/// Recursion-depth cap for scripts run by the `mix` binary.
///
/// The library default ([`cosmix_mix::DEFAULT_RECURSION_LIMIT`] = 16)
/// is sized conservatively for the smallest realistic embedder stack
/// (~2 MB — tokio worker / `spawn_blocking` / test threads, e.g.
/// maild's per-message filter). The binary runs scripts on the ~8 MB
/// main thread, where the async call path overflows around depth ~210,
/// so it raises the cap to 128 — ordinary deep recursion works while a
/// runaway still returns a clean error instead of a native stack
/// overflow.
const SCRIPT_RECURSION_LIMIT: usize = 128;

/// `--no-traceback` (0.29.0): restore the legacy single-line rendering
/// for uncaught errors. Default is the multi-line traceback
/// (`MixError::render_traceback`) when the error crossed a function or
/// builtin boundary; errors with no frames render single-line either
/// way, so shallow scripts are unaffected.
static NO_TRACEBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The source the current top-level run is executing, `(path, source)` —
/// `path` is the script filename (matching the frames' `file`) or `None` for
/// `-c`/stdin. Set once before execution by [`run_source`]/[`run_command_line`]
/// so [`print_uncaught`] can show the offending source line under an error.
/// Held as the exact bytes handed to the evaluator, so the printed line is
/// always what actually ran — no disk re-read, no staleness, no lie.
static ENTRY_SOURCE: std::sync::Mutex<Option<(Option<String>, String)>> =
    std::sync::Mutex::new(None);

/// Remember the top-level source for error-line rendering.
fn set_entry_source(path: Option<String>, source: &str) {
    if let Ok(mut guard) = ENTRY_SOURCE.lock() {
        *guard = Some((path, source.to_string()));
    }
}

/// The offending source line to print under an uncaught error, `(line_no,
/// text)` — but only when the failure site belongs to the top-level source
/// this process actually executed (a `-c` body, or the main script file).
/// An error raised inside an imported module (a different `file`) yields
/// `None` rather than a possibly-stale disk read: better silent than lying.
fn offending_source_line(e: &cosmix_mix::error::MixError) -> Option<(usize, String)> {
    let (site_file, line) = e.error_site()?;
    let guard = ENTRY_SOURCE.lock().ok()?;
    let (entry_path, source) = guard.as_ref()?;
    if &site_file != entry_path {
        return None;
    }
    let text = source.lines().nth(line.checked_sub(1)?)?;
    if text.trim().is_empty() {
        return None;
    }
    // Cap a pathological one-liner so the footer stays one terminal line.
    let shown = if text.chars().count() > 200 {
        let head: String = text.chars().take(197).collect();
        format!("{head}...")
    } else {
        text.to_string()
    };
    Some((line, shown))
}

/// Print an uncaught top-level error: traceback by default, legacy
/// single line under `--no-traceback`. Both forms gain a caret-style
/// source-line footer when the failure site is in the top-level source.
fn print_uncaught(e: &cosmix_mix::error::MixError) {
    if NO_TRACEBACK.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{e}");
    } else {
        eprintln!("{}", e.render_traceback());
    }
    // Statements carry line precision only (no column), so there is no caret
    // to point — show the offending line under a line-number gutter, rustc's
    // form minus the `^^^` row that a column span would fill.
    if let Some((line, text)) = offending_source_line(e) {
        eprintln!("  {line} | {text}");
    }
}

/// `--strict-arity` (0.29.0, decision D5): run the script/command/serve
/// evaluator in [`cosmix_mix::ArityMode::Strict`] — user-function calls
/// outside min..=max and builtin calls outside their contract arity
/// raise catchable ARITY_MISMATCH errors instead of the compatible
/// missing->nil / extra-ignored binding.
static STRICT_ARITY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Apply the global CLI arity flag to a freshly built evaluator.
fn apply_arity_mode(eval: &mut Evaluator) {
    if STRICT_ARITY.load(std::sync::atomic::Ordering::Relaxed) {
        eval.set_arity_mode(cosmix_mix::ArityMode::Strict);
    }
}

/// Limits applied to every script the binary runs (REPL, `-c`, file
/// runner, `--serve`). Only the recursion cap is raised above the lib
/// default; time/collection caps stay unset (the binary does not
/// sandbox its own operator).
pub(crate) fn script_limits() -> cosmix_mix::EvalLimits {
    cosmix_mix::EvalLimits {
        recursion_limit: SCRIPT_RECURSION_LIMIT,
        ..Default::default()
    }
}

/// Build the current-thread tokio runtime every binary mode runs on.
/// A failure here is a startup environment problem (fd exhaustion,
/// resource limits) — report it plainly on stderr and exit(1) rather
/// than panicking with a backtrace.
pub(crate) fn build_runtime() -> tokio::runtime::Runtime {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("mix: failed to create tokio runtime: {}", e);
            process::exit(1);
        }
    }
}

/// Wait for SIGTERM (systemd stop) or Ctrl-C, whichever fires first, and return
/// which one it was.
///
/// The number is load-bearing for `--result-fd`: a task supervisor ends its
/// child with SIGTERM, and the result frame has to say so rather than present a
/// killed evaluation as a successful nil.
///
/// Inlined here so mix has no dependency on the cos-side
/// `cosmix-lib-daemon` crate; behaviour-parity with that crate's
/// `shutdown_signal()`.
async fn shutdown_signal() -> i32 {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    // Registration failure leaves Ctrl-C as the available graceful path;
    // do not bypass the evaluator's final stats flush.
    let signal = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut sigterm) => tokio::select! {
            _ = ctrl_c => libc::SIGINT,
            _ = sigterm.recv() => libc::SIGTERM,
        },
        Err(e) => {
            eprintln!("mix: failed to register SIGTERM handler: {}", e);
            let _ = ctrl_c.await;
            libc::SIGINT
        }
    };

    #[cfg(not(unix))]
    let signal = {
        ctrl_c.await.ok();
        libc::SIGINT
    };

    tracing::info!(signal, "shutdown signal received");
    signal
}

fn print_help() {
    // Single source of truth shared with the `mix help` subcommand / REPL:
    // a markdown-friendly discovery overview that signposts `mix builtins`
    // for the full function remit rather than duplicating it here.
    print!("{}", meta::help_overview_string(VERSION));
    println!(
        "  --gui [args...]  Launch a fresh Term frontend (COSMIX_TERM_BIN overrides); Bus window reuse deferred until authenticated per-user identity (P0-I)."
    );
}

/// Resolve installed Term only; the injected lookup keeps tests independent of
/// the host installation and environment. An explicit override fails closed.
fn resolve_term(
    override_bin: Option<std::ffi::OsString>,
    cosmix: Option<std::ffi::OsString>,
    mut lookup: impl FnMut(&Path) -> Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf, String> {
    if let Some(path) = override_bin {
        return lookup(Path::new(&path)).ok_or_else(|| {
            "mix --gui: COSMIX_TERM_BIN must name an executable frontend file".to_string()
        });
    }
    // `bterm` first, then `term`, at each tier before moving to the next — a
    // $COSMIX dev build must win over an installed one whichever frontend it
    // is, or `mix --gui` silently runs the system terminal while you are
    // testing a local one.
    //
    // ⚠️ THE NAME ORDER IS A DECISION, NOT A DEFAULT (Mark, 2026-09-21,
    // TODO-term D10). `bterm` is the tabbed, verb-complete, daily-driven
    // frontend; `term` is the iced one, which since T2 is a skeleton that
    // draws a grid and serves no verbs. The moment `apps/term` became a
    // workspace member again, `setup.mix --desktop` began installing it as
    // `bin/term` — so a `term`-first list would have made the next routine
    // install silently replace the daily driver with the skeleton, which is
    // exactly what D6 forbids until the new frontend has full verb parity
    // (T6) plus a month of daily driving.
    //
    // FLIP THIS BACK AS PART OF T6, not before, and not as a drive-by: the
    // whole point of D6 is that the switch is a decision someone makes, not
    // one an installer makes for them. Until then `COSMIX_TERM_BIN=.../term`
    // is how you reach the iced one deliberately; it still overrides
    // everything and still fails closed.
    for name in TERM_FRONTENDS {
        if let Some(root) = &cosmix
            && let Some(path) = lookup(&Path::new(root).join("bin").join(name))
        {
            return Ok(path);
        }
    }
    for name in TERM_FRONTENDS {
        if let Some(path) = lookup(&Path::new("/opt/cosmix/bin").join(name)) {
            return Ok(path);
        }
    }
    TERM_FRONTENDS
        .into_iter()
        .find_map(|name| lookup(Path::new(name)))
        // Derived from TERM_FRONTENDS, never spelled out again. Two sibling
        // error strings in other crates drifted stale the moment this order
        // changed, which is what a hand-written copy of a list does.
        .ok_or_else(|| {
            format!(
                "mix --gui: no CosMix terminal frontend is installed (looked for {} in $COSMIX/bin, /opt/cosmix/bin, and on PATH). Install the desktop package.",
                TERM_FRONTENDS.join(" then ")
            )
        })
}

/// The frontend binary names, in resolution order. Shared with [`term_lookup`]
/// so the PATH tier cannot search for a name the lookup then refuses to treat
/// as a PATH name — which is exactly what happened to `bterm` between the T1
/// rename and this fix: the list gained a second name and the lookup's
/// bare-name exemption did not, so a `bterm` installed only on PATH was looked
/// for at `<cwd>/bterm` and never found.
const TERM_FRONTENDS: [&str; 2] = ["bterm", "term"];

/// Is `path` an unqualified frontend name — one that must reach `which`
/// untouched so PATH is actually searched?
fn is_bare_frontend_name(path: &Path) -> bool {
    TERM_FRONTENDS.iter().any(|name| path == Path::new(name))
}

/// Reuse the public `which` builtin's regular-file + kernel X_OK check,
/// including ACLs. Absolute candidates bypass PATH inside that same builtin,
/// so an unqualified frontend name must be handed over UNCHANGED or it becomes
/// a `<cwd>`-relative path and PATH is never consulted.
fn term_lookup(path: &Path) -> Option<std::path::PathBuf> {
    let path = if is_bare_frontend_name(path) {
        path.to_path_buf()
    } else {
        std::path::absolute(path).ok()?
    };
    match cosmix_mix::builtins::call_builtin(
        "which",
        vec![Value::String(path.to_str()?.to_string())],
    ) {
        Ok(Some(Value::String(ref found))) => std::path::absolute(found).ok(),
        _ => None,
    }
}

/// Refuse a resolved frontend that is not a launchable executable IMAGE — an
/// ELF binary or a `#!` script. This is what stops execvp's ENOEXEC→/bin/sh
/// fallback from running an arbitrary +x text file as a shell (empirically a
/// silent `exit 0`). Read errors are treated as "not launchable".
#[cfg(unix)]
fn require_executable_image(path: &Path) -> Result<(), String> {
    use std::io::Read;
    let mut head = [0u8; 4];
    let n = fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let is_elf = n >= 4 && head == *b"\x7fELF";
    let is_shebang = n >= 2 && head[..2] == *b"#!";
    if is_elf || is_shebang {
        Ok(())
    } else {
        Err(format!(
            "{} is not a launchable executable (expected an ELF binary or a #! script); \
             refusing to run it as a shell",
            path.display()
        ))
    }
}

fn run_gui() -> i32 {
    // P2.2c starts a fresh instance. Requesting a window from an existing
    // instance over Bus is deferred until authenticated per-user identity
    // (P0-I) exists; do not guess an instance or fake a Bus request here.
    let frontend = match resolve_term(
        env::var_os("COSMIX_TERM_BIN"),
        env::var_os("COSMIX"),
        term_lookup,
    ) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    // A resolved +x file that is neither an ELF image nor a `#!` script would
    // hit execvp's POSIX ENOEXEC fallback to /bin/sh: it would silently run the
    // file AS a shell script and exit 0 without ever launching a frontend
    // (spawn-and-wait has the same fallback). Refuse it so `--gui` can never
    // succeed by running a non-frontend as a shell.
    #[cfg(unix)]
    if let Err(error) = require_executable_image(&frontend) {
        eprintln!("mix --gui: {error}");
        return 1;
    }
    // Use Command's direct argv execution, as exec::command_for does for
    // external programs. Keep cwd, environment and stdio inherited. Forward
    // OS argv unchanged (any non-UTF-8 argv would already have aborted the
    // process at the initial env::args() collection — a pre-existing,
    // binary-wide limitation, not something --gui can widen).
    let forwarded = env::args_os().skip_while(|arg| arg != "--gui").skip(1);
    let mut command = process::Command::new(&frontend);
    command.args(forwarded);
    // "Open here": a terminal launched from a shell opens in that shell's
    // directory. Stamp the invoking cwd as TERM_CWD so the frontend's child
    // shell starts there, but never override an explicit TERM_CWD a caller
    // (a desktop launcher targeting a project) already set.
    if env::var_os("TERM_CWD").is_none()
        && let Ok(cwd) = env::current_dir()
    {
        command.env("TERM_CWD", cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!(
            "mix --gui: exec {} failed: {error}; trying spawn-and-wait",
            frontend.display()
        );
    }
    match command.status() {
        Ok(status) => exec::exit_code(status),
        Err(error) => {
            eprintln!(
                "mix --gui: could not launch {}: {error}",
                frontend.display()
            );
            1
        }
    }
}

/// One-shot stats subcommand: load stats from disk, dispatch, exit.
/// Separate from the REPL meta-command path so scripts and cron jobs
/// can query stats without starting an interactive session. The REPL
/// path still works identically — both call `cmd_stats_dispatch`
/// against the same on-disk `current.json` under
/// `$XDG_STATE_HOME/mix/` (default `~/.local/state/mix/`).
fn run_stats_subcommand(sub_args: &[String]) -> i32 {
    let args_slice: Vec<&str> = sub_args.iter().map(String::as_str).collect();
    stats_io::cmd_stats_dispatch(&args_slice, None)
}

/// One-shot meta-command subcommand: build a minimal Evaluator, load
/// prelude so builtin inspection works, dispatch to `meta::dispatch`.
/// Counterpart to `run_stats_subcommand` for every other REPL meta
/// command (`help`, `builtins`, `keywords`, `man`, `mesh`, `ports`,
/// `build`, `test`, etc.).
///
/// The Evaluator is mostly empty — no user vars, no aliases, no
/// user-defined functions — because a one-shot CLI doesn't have
/// REPL session state. Commands that inspect session state (`vars`,
/// `aliases`, `functions`, `type`, `context`) will show empty
/// results, which is the correct behaviour: there IS no session.
fn run_meta_subcommand(sub_args: &[String]) -> i32 {
    let rt = build_runtime();

    rt.block_on(async {
        let mut eval = Evaluator::new();
        eval.set_bus_handler(std::rc::Rc::new(bus::MixBusHandler::new()));
        cosmix_mix::interrupt::init(eval.interrupt_flag());
        eval.load_prelude().await;

        // `mix doctor` one-shot: return its health exit code (0/1) so it can
        // gate `mix doctor && …`. Handled here rather than in `dispatch`, which
        // is shared with the REPL where a process::exit would kill the session.
        if sub_args.first().map(String::as_str) == Some("doctor") {
            return meta::run_doctor(&eval, VERSION);
        }
        let args_slice: Vec<&str> = sub_args.iter().map(String::as_str).collect();
        let _exec_hint = meta::dispatch(&args_slice, &eval, VERSION);
        // `dispatch` returns Some(path) only for REPL-exec-chain
        // commands like `build` that restart the REPL into a new
        // binary. In one-shot mode there's no REPL to exec back
        // into, so we just exit after the command returns.
        0i32
    })
}

/// Names the one-shot CLI accepts as meta commands (before trying
/// the argument as a script filename). Matching one of these sends
/// the remaining args to `run_meta_subcommand`. Scripts with these
/// names in the CWD are shadowed — users who need to run a script
/// named e.g. `build` should invoke it as `./build` or `mix ./build`.
///
/// Kept as an explicit allowlist rather than "try meta first, fall
/// back to script" so dispatch is deterministic and future meta
/// commands are opt-in visible at this layer.
const META_CLI_COMMANDS: &[&str] = &[
    "vars",
    "aliases",
    "functions",
    "all",
    "type",
    "config",
    "build",
    "clean",
    "update",
    "test",
    "self",
    "status",
    "doctor",
    // `version` matches the REPL meta-command spelling (`mix version`), so a
    // plumbed REPL line falling through to external execution — and any
    // script/CLI caller — gets the version line instead of "Error reading
    // 'version'". `--version`/`-V` are handled by flag parsing before this.
    "version",
    "check",
    "diff",
    "mesh",
    "ports",
    "ping",
    "tutorial",
    "examples",
    "man",
    "help",
    "keywords",
    "builtins",
    "what",
    "apropos",
    "syntax",
    "operators",
    "fix",
    "extend",
    "review",
    "explain",
    "evolve",
    "dogfood",
    "fuzz",
    "teach",
    "context",
    "snapshot",
    "ask",
    "chat",
    "deploy",
    "health",
    "logs",
    "claude-start",
    "claude-stop",
    "claude-status",
    "watch",
];

fn run_source(
    source: &str,
    filename: Option<&str>,
    script_args: &[String],
    no_prelude: bool,
) -> i32 {
    // `args()` reads this. It must be told, not left to guess from the
    // process argv — a flag before the script name shifts that by one.
    cosmix_mix::set_script_argv(script_args.to_vec());
    // Remember the source (keyed by the same filename the frames carry) so an
    // uncaught error can show its offending line.
    set_entry_source(filename.map(str::to_string), source);
    let rt = build_runtime();

    // SPEC 18 Phase 2 WS3-C.7d — wrap the whole `block_on` body in a
    // `LocalSet`. The Class C dispatch path spawns each
    // async-handler chain on the ambient LocalSet via
    // `tokio::task::spawn_local`; without one in scope, the first
    // Class C dispatch would panic (`spawn_local called from outside
    // of a task::LocalSet`). Pure Class S scripts run unchanged inside
    // the LocalSet — `spawn_local` is only called when at least one
    // matching `on <cmd>` handler is `async`.
    let local = tokio::task::LocalSet::new();
    let (outcome, stats) = rt.block_on(local.run_until(async {
        let mut eval = Evaluator::new();
        eval.set_limits(script_limits());
        apply_arity_mode(&mut eval);
        eval.set_bus_handler(std::rc::Rc::new(bus::MixBusHandler::new()));
        // Make `source x` fall back to the REPL-style shell
        // classifier when `x` contains bareword shell lines (matches
        // .mixrc semantics). Pure-Mix files still hit the whole-file
        // parse path and never invoke the handler.
        eval.set_shell_handler(std::rc::Rc::new(shell_handler::ReplShellHandler::new()));
        cosmix_mix::interrupt::init(eval.interrupt_flag());

        // Register AI extension functions
        repl::register_ai_extensions(&mut eval);

        // Load prelude
        if !no_prelude {
            eval.load_prelude().await;
        }

        if stats_io::stats_enabled() {
            let context = if filename == Some("-") {
                StatsContext::new(ExecutionMode::Stdin, None)
            } else {
                StatsContext::new(ExecutionMode::Script, filename.map(Path::new))
            };
            eval.attach_stats(UsageStats::for_execution(context));
            if let Some(mut stats) = eval.stats_mut() {
                stats.increment_commands();
            }
        }

        // Set positional arguments
        if let Some(name) = filename {
            eval.set_global("0", Value::String(name.to_string()));
            // Record the script path so `include` can resolve relative to
            // the running file's directory (and so top-level diagnostics
            // attribute to the script, not "<unknown>").
            eval.set_file(name);
        }
        for (i, arg) in script_args.iter().enumerate() {
            eval.set_global(&(i + 1).to_string(), Value::String(arg.clone()));
        }

        // Race script execution against Ctrl-C directly.
        // tokio::signal::ctrl_c() in the select ensures the IO driver
        // is polled even during pure timer sleeps, which a spawned task
        // approach cannot guarantee on current_thread.
        let outcome = tokio::select! {
            biased;
            _ = shutdown_signal() => {
                // First Ctrl-C — exit cleanly
                Ok(())
            }
            res = async {
                eval.execute_script_source(source).await?;
                if eval.handler_count() > 0 {
                    eval.run_event_pump().await?;
                }
                Ok::<_, cosmix_mix::error::MixError>(())
            } => res,
        };
        (outcome, eval.take_stats())
    }));
    if let Some(stats) = stats {
        stats_io::flush_batch(stats);
    }
    match outcome {
        Ok(_) => 0,
        Err(cosmix_mix::error::MixError::ExitRequest { code }) => code,
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("interrupted") {
                // Clean exit on interrupt
                0
            } else {
                print_uncaught(&e);
                1
            }
        }
    }
}

/// Run a `-c` (and `-i -c`) one-shot command line through the SAME
/// classifier the interactive REPL uses, so a mix-login-shell honours the
/// universal shell `-c` contract: `ssh host hostname` / `mix -c 'mix status'`
/// dispatch as commands, while `mix -c 'print(1 + 1)'` and every other Mix
/// statement still evaluate as Mix.
///
/// - `load_rc`: `-i` was given (≈ `bash -ci`) — source ~/.mixrc first so
///   aliases + the toolkit's PATH are in scope before classifying. Without
///   it, aliases stay empty (≈ `bash -c`).
/// - Classification (`shell::classify_input`) is shell-first: a first word on
///   PATH (or `mix`, a SHELL_BUILTIN) → external command; `print`/`if`/`$…`
///   and anything that parses as a real Mix statement → Mix.
/// - A dispatched command's exit status becomes mix's exit code.
/// - A leading `time` is a MODIFIER, resolved before both (see
///   `shell::strip_time_prefix`), so `ssh host 'time shwho'` times the command
///   instead of hunting PATH for a `time` binary that does not exist.
fn run_command_line(
    code: &str,
    load_rc: bool,
    script_args: &[String],
    no_prelude: bool,
    result_fd: Option<crate::result_fd::ResultFd>,
) -> i32 {
    cosmix_mix::set_script_argv(script_args.to_vec());
    // `-c`/stdin has no file, so its frames carry `file: None`; store under
    // `None` to match, enabling the offending-line footer for `-c` too.
    set_entry_source(None, code);
    let rt = build_runtime();
    let local = tokio::task::LocalSet::new();
    let (exit_code, stats) = rt.block_on(local.run_until(async {
        let mut eval = Evaluator::new();
        eval.set_limits(script_limits());
        apply_arity_mode(&mut eval);
        eval.set_bus_handler(std::rc::Rc::new(bus::MixBusHandler::new()));
        // Same per-line shell fallback the REPL/.mixrc rely on.
        eval.set_shell_handler(std::rc::Rc::new(shell_handler::ReplShellHandler::new()));
        cosmix_mix::interrupt::init(eval.interrupt_flag());
        repl::register_ai_extensions(&mut eval);
        if !no_prelude {
            eval.load_prelude().await;
        }
        for (idx, arg) in script_args.iter().enumerate() {
            eval.set_global(&(idx + 1).to_string(), Value::String(arg.clone()));
        }
        if load_rc && let Some(code) = repl::load_mixrc_async(&mut eval).await {
            return (code, eval.take_stats());
        }
        if stats_io::stats_enabled() {
            eval.attach_stats(UsageStats::for_execution(StatsContext::new(
                ExecutionMode::C,
                None,
            )));
            if let Some(mut stats) = eval.stats_mut() {
                stats.increment_commands();
            }
        }

        let exit_code = async {

        // Genuinely-empty input (blank line / `#` or `--` comment) is a clean
        // no-op. Any OTHER input the classifier collapses to Empty is a parse
        // error it has already printed to stderr — that must NOT exit 0.
        //
        // EVERY line must be blank-or-comment, not just the first. The original
        // `trimmed.starts_with("--")` is correct for a REPL line, where the
        // input IS one line — but `-c` carries whole programs, and a program
        // whose FIRST line is a comment was silently discarded and reported
        // success:
        //
        //     mix -c '-- set up
        //     print("RAN")'      ->  no output, exit 0
        //
        // Silent discard with exit 0 is the worst available failure mode: a
        // script that never ran is indistinguishable from one that did nothing.
        // Comments are the normal way to open a generated script, so this hit
        // exactly the machine-authored case.
        let trimmed = code.trim();
        let all_comment_or_blank = trimmed.lines().all(|l| {
            let t = l.trim();
            t.is_empty() || t.starts_with('#') || t.starts_with("--")
        });
        if trimmed.is_empty() || all_comment_or_blank {
            return 0;
        }

        // `time <line>` — a modifier, not a command (bash's `time` is a keyword;
        // there is no `time` binary to exec). Resolved BEFORE alias expansion so
        // the wrapped head still expands (`time ll` → `time ls -l`), and before
        // classification so it wraps whatever the rest turns out to be: external
        // command, pipeline, chain, bareword function, or Mix code.
        let (timed, code) = match shell::strip_time_prefix(trimmed) {
            Some("") => {
                eprintln!("mix: time: usage: time <command | mix expression>");
                return 2;
            }
            Some(rest) => (true, rest),
            None => (false, code),
        };

        // Expand aliases up front (mirrors the REPL at repl.rs:185), then
        // classify + dispatch the EXPANDED line — so `-i -c '<alias>'` runs
        // the alias's expansion, not the bare alias name.
        let (line, alias_name) = {
            let aliases = eval.aliases();
            let alias_name = code
                .split_whitespace()
                .next()
                .filter(|name| aliases.contains_key(*name))
                .map(str::to_string);
            (shell::expand_alias(code, &aliases), alias_name)
        };
        if let Some(alias_name) = alias_name
            && let Some(mut stats) = eval.stats_mut()
        {
            stats.track_alias(&alias_name);
        }
        let kind = {
            let aliases = eval.aliases();
            let functions = eval.function_names();
            shell::classify_input_fns(&line, &aliases, &functions)
        };
        // Report elapsed on every exit path — the arms below return early (see
        // shell::TimeGuard). Armed only for the arms that RUN something, mirroring
        // the REPL: a line that never executed (`time # comment`, `time print(`)
        // has no duration to report, and an elapsed there would time nothing but
        // the error path.
        let executing = !matches!(
            kind,
            shell::InputKind::Empty
                | shell::InputKind::Incomplete
                | shell::InputKind::ParseError(_)
        );
        let mut timer = shell::TimeGuard::armed(timed && executing);
        match kind {
            // Only genuinely-empty input (a blank line, a `#`/`--` comment, or
            // an alias that expands to one) reaches here as Empty — a clean
            // no-op, exit 0. A definitive Mix lex/parse error is ParseError now,
            // not Empty, so it is no longer silently collapsed to a 0-or-1 guess.
            shell::InputKind::Empty => 0,
            shell::InputKind::ParseError(msg) => {
                eprintln!("{}", msg);
                1
            }
            shell::InputKind::Incomplete => {
                eprintln!("mix: -c: incomplete input (unterminated block, string, or expression)");
                1
            }
            shell::InputKind::MixCode(stmts) => {
                if stmts.is_empty() {
                    return 0;
                }
                // Race execution (+ the event pump, for any `on` handlers)
                // against Ctrl-C, exactly as run_source does, so a `-c` body
                // that registers handlers can still be interrupted.
                // The VALUE is kept, not discarded, when a result fd is
                // present. `-c` has never echoed it to stdout, so nothing is
                // being suppressed here — the property the task contract wants
                // (stdout is the program's text, the value travels the fd)
                // already held, and this preserves it rather than creating it.
                // Err(signal) is the shutdown arm. It has to stay distinguishable
                // all the way to the frame: a task's supervisor ends it with
                // SIGTERM, so folding that into `Ok(Value::Nil)` wrote a
                // SUCCESSFUL result for a killed evaluation — a cancelled task
                // and a task that genuinely returned nil became the same report.
                let res: Result<Result<Value, cosmix_mix::error::MixError>, i32> = tokio::select! {
                    biased;
                    signal = shutdown_signal() => Err(signal),
                    r = async {
                        let value = eval.execute(&stmts).await?;
                        if eval.handler_count() > 0 {
                            eval.run_event_pump().await?;
                        }
                        Ok(value)
                    } => Ok(r),
                };
                let framed = result_fd.is_some();
                if let Some(result_fd) = result_fd {
                    let payload = match &res {
                        Err(signal) => crate::result_fd::Payload::Error(format!(
                            "interrupted by signal {signal} before the evaluation finished"
                        )),
                        Ok(Ok(value)) => crate::result_fd::Payload::Value(value.clone()),
                        Ok(Err(error)) => crate::result_fd::Payload::Error(format!("{error}")),
                    };
                    if let Err(error) = result_fd.write(&payload) {
                        // Loud, because a missing frame is reported by the
                        // supervisor as `result_missing` and the operator would
                        // otherwise have no way to learn why.
                        eprintln!("mix: --result-fd: could not write the result frame: {error}");
                    }
                }
                match res {
                    // Only the framed caller's exit code changes. A plain
                    // `mix -c` stopped by Ctrl-C or systemd has always been a
                    // clean 0 and scripts depend on it; a task's shell is the
                    // one that must not exit 0 after being killed.
                    Err(signal) => {
                        if framed {
                            128 + signal
                        } else {
                            0
                        }
                    }
                    Ok(Ok(_)) => 0,
                    Ok(Err(cosmix_mix::error::MixError::ExitRequest { code })) => code,
                    // Match run_source: a Ctrl-C interrupt is a clean exit.
                    Ok(Err(e)) if format!("{e}").contains("interrupted") => 0,
                    Ok(Err(e)) => {
                        print_uncaught(&e);
                        1
                    }
                }
            }
            shell::InputKind::FunctionCommand { name, args } => {
                // Bareword call of a defined function under `-c` (`mix -i -c
                // 'sc restart nginx'`) — dispatch as `sc("restart", "nginx")`.
                // Race against Ctrl-C like the MixCode arm; exit 0 on success,
                // 1 on a Mix runtime error (the function's own `$rc`/side effects
                // carry the real command status, exactly as a paren call would).
                let res: Result<(), cosmix_mix::error::MixError> = tokio::select! {
                    biased;
                    _ = shutdown_signal() => Ok(()),
                    r = async {
                        eval.call_function_by_name_with_args(&name, &args).await?;
                        if eval.handler_count() > 0 {
                            eval.run_event_pump().await?;
                        }
                        Ok(())
                    } => r,
                };
                match res {
                    Ok(_) => 0,
                    Err(cosmix_mix::error::MixError::ExitRequest { code }) => code,
                    Err(e) if format!("{e}").contains("interrupted") => 0,
                    Err(e) => {
                        print_uncaught(&e);
                        1
                    }
                }
            }
            shell::InputKind::ExternalCommand(command) => {
                // Split control ops (&&/||/;) on the LITERAL line STRUCTURALLY
                // (no resolver, no expansion) — so a variable's value can never
                // inject a control operator (no command injection), and a
                // `$(...)` is NOT run until its piece is selected for execution.
                let items = match exec::split_command_list(&command) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("{}", e);
                        return 1;
                    }
                };
                // A LONE command is parsed+expanded once (running any `$(...)`
                // exactly once) so the in-process builtins below can intercept
                // it: `exit`/`cd` and the REPL-launching bare `mix`. (REPL-only
                // builtins jobs/fg/bg/pushd/popd/history/unalias have no meaning
                // under `-c` and spawn-and-fail — acceptable.)
                if items.len() == 1 {
                    let pipeline = match exec::parse_pipeline(items[0].1, &eval) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("{}", e);
                            return 1;
                        }
                    };
                    if let Some(seg) = pipeline.segments.first()
                        && let Some(mut stats) = eval.stats_mut()
                    {
                        stats.track_command(&seg.program);
                    }
                    if pipeline.segments.len() == 1 {
                        let seg = &pipeline.segments[0];
                        match seg.program.as_str() {
                            "exit" => {
                                return seg
                                    .args
                                    .first()
                                    .and_then(|s| s.parse::<i32>().ok())
                                    .unwrap_or(0);
                            }
                            // The canonical in-process cd (exec::builtin_cd) —
                            // `-c` gains `cd` (→HOME), `cd -`, and `~`-expansion,
                            // matching the REPL and sourced files.
                            "cd" => return exec::builtin_cd(&seg.args),
                            "mix" if seg.args.is_empty() => {
                                eprintln!(
                                    "mix: -c: bare 'mix' would start an interactive REPL — ignored"
                                );
                                return 0;
                            }
                            _ => {}
                        }
                    }
                    return match exec::execute_pipeline(&pipeline) {
                        Ok(exec::PipelineResult::Managed(_)) => unreachable!("noninteractive policy"),
                        Ok(exec::PipelineResult::Done(status)) => exec::exit_code(status),
                        // One-shot `-c`: the process exits immediately, so the
                        // `&` child is re-parented to init and reaped there —
                        // leaking it here is correct (no JobTable exists).
                        // Backgrounded: the child was SPAWNED and we return now, so
                        // the only thing a timer could report is spawn latency —
                        // which would read as the command's runtime. Cancel it.
                        Ok(exec::PipelineResult::Background(_)) => {
                            timer.disarm();
                            if timed {
                                eprintln!("mix: time: backgrounded (&) — not timed");
                            }
                            0
                        }
                        Err(e) => {
                            let prog = pipeline
                                .segments
                                .first()
                                .map(|s| s.program.as_str())
                                .unwrap_or("command");
                            eprintln!("mix: {}: {}", prog, e);
                            127
                        }
                    };
                }
                // Run a chain with &&/||/; short-circuit, expanding (and running
                // any `$(...)` in) each piece only when its connector selects it;
                // returns the last executed command's exit code (signal-killed ->
                // 128+sig; spawn error -> 127, control flow continues). No
                // JobTable in a one-shot `-c` — a `&` piece is reaped detached.
                let outcome = exec::execute_command_list_outcome(&items, &eval, None);
                if let Some(mut stats) = eval.stats_mut() {
                    for command in &outcome.commands {
                        stats.track_command(command);
                    }
                }
                // Background is read from what the chain actually RAN, not from a
                // scan of the pieces: a `&` in a branch the connectors skip
                // (`false && sleep 5 &`) spawns nothing, so that line still has a
                // real foreground duration and stays timeable.
                if outcome.backgrounded {
                    timer.disarm();
                    if timed {
                        eprintln!("mix: time: backgrounded (&) — not timed");
                    }
                }
                outcome.code
            }
        }
        }
        .await;
        (exit_code, eval.take_stats())
    }));
    if let Some(stats) = stats {
        stats_io::flush_batch(stats);
    }
    exit_code
}

/// Strip a leading `cosmix-` so a POSIX user name (`cosmix-<d>`) never
/// leaks into the Bus namespace as a service name. The Bus namespace
/// uses the bare `<d>` token (SPEC-10 / SPEC 18 §3.1); `cosmix-` is the
/// system-user prefix only.
fn strip_cosmix_prefix(s: &str) -> String {
    s.strip_prefix("cosmix-").unwrap_or(s).to_string()
}

/// Derive the Bus service name for `mix --serve`.
///
/// SPEC 18 §3.1 / SPEC-10: the Bus service name is the `<d>` token (the
/// POSIX user `cosmix-<d>` minus the `cosmix-` prefix). `--name` wins;
/// otherwise the default is the script's file stem — **never** the
/// `cosmix-*` POSIX form (a leading `cosmix-` is stripped from either
/// source so an install that names the script/flag after the system
/// user still yields the canonical Bus identity:
/// `/usr/local/lib/cosmix/statecache.mix` → `statecache`). An
/// empty derivation is rejected: an anonymous `--serve` is a launch
/// error (the caller exits non-zero), never a nameless citizen.
fn derive_serve_name(explicit: Option<&str>, script_path: &str) -> Result<String, String> {
    if let Some(n) = explicit {
        let n = strip_cosmix_prefix(n.trim());
        if !is_valid_bus_name(&n) {
            return Err(format!(
                "--name resolved to '{n}', which is not a valid Bus service name \
                 (must be non-empty and not start with '.')"
            ));
        }
        return Ok(n);
    }
    let stem = std::path::Path::new(script_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim();
    let name = strip_cosmix_prefix(stem);
    if !is_valid_bus_name(&name) {
        return Err(format!(
            "cannot derive a Bus service name from '{script_path}'; \
             pass --name <svc> (anonymous --serve is not permitted)"
        ));
    }
    Ok(name)
}

/// A Bus service name is the bare SPEC-10 `<d>` token: non-empty and
/// not a leading-dot hidden-file artefact. A dotfile-only script path
/// (`/x/.mix`, `.foo`) has `file_stem()` == the whole `.`-led name, so
/// the empty check alone would let a non-Bus identity (`.foo`) register
/// — §3.1 requires the default be the Bus form or a launch error. The
/// `cosmix-` POSIX prefix is stripped by the caller before this check;
/// full token-charset validation is the broker's job, not the
/// launcher's (kept narrow to avoid WS3 scope creep).
fn is_valid_bus_name(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.')
}

/// SPEC 18 §3.5 (WS5) bounded grace for the deregister RPC: a wedged
/// or unreachable broker must not hang process exit. Exceeding it →
/// best-effort supervisor stop + non-zero exit (operator/systemd
/// signal). 5 s is generous for a single intra-mesh `noded.deregister`
/// round-trip; Phase-1 default, no config surface yet.
const DEREGISTER_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How `run_serve`'s pump/init future terminated, mapped to an exit
/// code after the supervisor is stopped.
enum ServeOutcome {
    /// Ctrl-C, or a `MixError` whose message indicates interruption.
    Interrupted,
    /// The pump returned `Ok` — a genuine shutdown, not a transient
    /// drop. Covers BOTH the Ch02 `QUIT` universal (WS4 pump break)
    /// and the supervised-receiver `None` fatal terminal. Transport
    /// liveness is NOT implied here; it is determined by the
    /// post-select deregister outcome (a clean `QUIT` typically leaves
    /// the socket live; a `None` terminal typically means it is gone).
    PumpEnded,
    /// The init body or pump raised a non-interrupt `MixError`.
    Error,
    /// Script control flow requested an exact process status. Language-level
    /// `finally` blocks have already run; serve still performs its bounded
    /// deregister/drain path before returning this code to the caller.
    ExitRequested(i32),
}

/// Initialize serve-mode logging via the shared `cosmix_log` core
/// (the bus logging crate every cosmix daemon now uses): native
/// **journald** with correct PRIORITY + structured fields, and an
/// automatic **stderr fallback** when no journal socket is present
/// (dev / non-systemd). `RUST_LOG`-overridable. Replaces the old
/// bespoke stderr-only subscriber — journald is strictly better than
/// "stderr captured by systemd" (priority mapping + queryable fields).
///
/// The file sink stays off (serve preset `log_file = None`): a SPEC-10
/// system citizen (`cosmix-statecache`) has no usable `HOME`
/// (SPEC 18 §3.6 / WS3 consult MAJOR 3), and journald is the durable
/// channel. Per-service identity is still carried as a structured
/// `service = <bus-name>` field on every serve/supervisor log line,
/// never the process name `cosmix-mix`.
///
/// Returns the `LogHandle`, which the caller MUST hold for the serve
/// lifetime — it owns the subscriber guards and the live-reload handle;
/// dropping it flushes. (Mix has no `cosmix-lib-props-store`, so the
/// live SPEC-12 `<svc>.log` swap that webd/maild get is not wired here;
/// `RUST_LOG` + restart is the control surface until a Mix-native Bus
/// verb drives the reload handle via props-core.)
fn init_serve_tracing() -> cosmix_log::LogHandle {
    // EnvFilter directives match the runtime tracing *target* = each
    // crate's compiled name, NOT its Cargo package name. Getting this
    // wrong silently drops the SPEC 18 §9 observability markers (a
    // mismatched directive is not an error — it just never matches):
    //   * binary crate `cosmix-mix` has `[[bin]] name = "mix"` → the
    //     §3.5 shutdown markers in this file log under target `mix`;
    //   * `cosmix-lib-mix` has `[lib] name = "cosmix_mix"` → the WS6
    //     §3.4 panic marker + pump lines log under `cosmix_mix::…`;
    //   * `cosmix-lib-client` has `[lib] name = "cosmix_client"` → the
    //     §3.3 `supervised_reconnect` replay marker;
    //   * `cosmix-lib-bus` has `[lib] name = "cosmix_bus"`.
    // Verified end-to-end against the WS8 acceptance harness — do not
    // "tidy" these back to package names. `RUST_LOG` overrides it.
    //
    // stderr sink: the `serve` defaults are journald-primary, and the
    // library's `Auto` stderr rule turns stderr OFF whenever the journald
    // socket is present (true on any systemd box) so a supervised citizen
    // doesn't double-log. That is right under systemd, but it means an
    // INTERACTIVE `mix --serve foo.mix` in a terminal shows NOTHING — a
    // handler fault answers the caller the fixed `internal handler error`
    // (the §3.4 wire-masking is deliberate; the real error is a
    // `tracing::error!`) and the developer never sees the real error. So
    // when stderr is a TTY (a foreground dev run, never a systemd unit),
    // force the stderr sink ON — the fault detail lands right in the
    // terminal. A systemd/redirected run (stderr not a TTY) keeps the
    // journald-primary default unchanged; `journalctl -t cosmix-mix` is the
    // channel there.
    use std::io::IsTerminal;
    let opts = cosmix_log::LogOpts {
        log_stderr: if std::io::stderr().is_terminal() {
            Some(cosmix_log::TriState::Always)
        } else {
            None
        },
        ..Default::default()
    };
    match cosmix_log::init(
        &opts,
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::serve("cosmix-mix")
            .with_filter("mix=info,cosmix_mix=info,cosmix_client=info,cosmix_bus=info"),
    ) {
        Ok(handle) => handle,
        Err(e) => {
            // Startup failure: a serve citizen without its logging channel
            // is misconfigured — fail fast under systemd with a plain
            // stderr message, not a panic backtrace.
            eprintln!("mix: --serve: logging init failed: {}", e);
            process::exit(1);
        }
    }
}

/// `mix --serve <script> [--name <svc>]`: run a Mix script as a
/// long-lived **supervised** Bus daemon citizen (SPEC 18 Phase 1 WS3).
///
/// Differs from [`run_source`] in three load-bearing ways:
///
/// 1. The transport is the WS1
///    [`SupervisedClient`](cosmix_lib_client::SupervisedClient), not a
///    one-shot anonymous `NodedClient`: a `cosmix-noded` bounce is a
///    transient drop the citizen reconnects/re-registers/replays
///    through (§3.3), not a process death.
/// 2. The pump is **unconditional and non-terminating** — a resident
///    daemon, not a script with an optional `handler_count()>0` event
///    tail. It exits only on a fatal terminal (supervised receiver
///    `None`) or interrupt.
/// 3. An exhausted *initial* connect budget is a typed fatal → exit
///    non-zero (§3.1), so a misconfigured citizen fails fast under
///    systemd rather than spinning silently.
///
/// Returns a process exit code (the caller `process::exit`s it).
fn run_serve(script_path: &str, service_name: &str, no_prelude: bool) -> i32 {
    // Held for the entire serve lifetime: owns the journald/stderr
    // subscriber guards + the live-reload handle. Dropping it on
    // `run_serve` return flushes pending log writes.
    let _log = init_serve_tracing();

    // Anchor the script path to an absolute one BEFORE any script code runs:
    // a citizen's init body may `chdir`, and RELOAD re-reads this path — a
    // relative path would then resolve against the changed directory and
    // reload a different file (or none). Fall back to the given path if the
    // file does not exist yet (the read below reports it).
    let script_path_abs = std::fs::canonicalize(script_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| script_path.to_string());
    let script_path = script_path_abs.as_str();

    let source = match fs::read_to_string(script_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                service = %service_name,
                script = %script_path,
                error = %e,
                "serve: cannot read script"
            );
            return 1;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(service = %service_name, error = %e, "serve: cannot build runtime");
            return 1;
        }
    };

    // SPEC 18 Phase 2 WS3-C.7d — serve mode runs the event pump and
    // dispatches Class C chains via `tokio::task::spawn_local`. The
    // pump body therefore MUST execute inside a `LocalSet`; otherwise
    // the first async-handler arrival panics at the spawn site.
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async {
        let stmts = {
            let mut lexer = cosmix_mix::lexer::Lexer::new(&source);
            let tokens = match lexer.tokenize() {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(
                        service = %service_name,
                        error = %format!("{e}"),
                        "serve: lex error"
                    );
                    return 1;
                }
            };
            let mut parser = cosmix_mix::parser::Parser::new(tokens, &source);
            match parser.parse_program() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        service = %service_name,
                        error = %format!("{e}"),
                        "serve: parse error"
                    );
                    return 1;
                }
            }
        };

        let noded_url = node_config::resolve_noded_url();
        tracing::info!(
            service = %service_name,
            noded_url = %noded_url,
            "serve: connecting (supervised)"
        );
        // A mix --serve citizen (e.g. statecache) has no binary of its
        // own — its provenance IS the mix binary's build, so noded.list
        // reports which mix runs it (version-discovery contract). Built
        // once here; the supervisor re-sends it on every reconnect.
        let bi = cosmix_buildinfo::build_info!();
        let provenance = cosmix_lib_bus::RegisterProvenance::from_parts(
            bi.pkg,
            bi.version,
            bi.git_sha,
            bi.git_dirty,
            bi.build_time,
            cosmix_buildinfo::now_rfc3339(),
        );
        let supervised = match cosmix_lib_client::SupervisedClient::connect_supervised_with_provenance(
            service_name,
            &noded_url,
            Some(provenance),
        )
        .await
        {
            Ok(s) => std::sync::Arc::new(s),
            Err(e) => {
                // Typed fatal (SPEC 18 §3.1): the initial connect+register
                // budget is exhausted. Fail fast, exit non-zero — do NOT
                // spin against a broker that will never answer.
                tracing::error!(
                    service = %service_name,
                    error = %e,
                    "serve: initial broker connect failed; exiting non-zero (SPEC 18 §3.1)"
                );
                return 1;
            }
        };
        tracing::info!(service = %service_name, "serve: connected and registered");

        // Evaluator construction, shared by first boot and every hot-reload
        // (SPEC 18 RELOAD, `_plan/2026-09-13-mix-citizen-hot-reload.md`):
        // identical wiring, so a reloaded citizen is indistinguishable from
        // a freshly started one — same runtime-reserved Ch07 L0+ surface
        // (HELP/INFO/QUIT/RELOAD + <svc>.props.{get,list,describe},
        // consulted pre-dispatch so an author `on` handler naming a
        // reserved verb cannot shadow it, DECIDED §7-Q4), same prelude,
        // limits, stats. The SupervisedClient is shared — the broker
        // connection and registration outlive any single evaluator.
        // The bus handler owns the single incoming-message receiver (taken
        // once from the supervised client), so it MUST be shared across
        // reloads — a fresh handler would find the receiver already taken
        // and its pump would exit immediately. Built once, cloned into every
        // evaluator.
        let bus_handler = std::rc::Rc::new(bus::MixServeHandler::new(supervised.clone()));
        // Process identity, shared across every generation so uptime/started_at
        // survive a reload and lifecycle.generation confirms a swap took.
        let identity = std::rc::Rc::new(serve_runtime::ReloadIdentity::new());
        async fn build_serve_eval(
            bus_handler: &std::rc::Rc<bus::MixServeHandler>,
            identity: &std::rc::Rc<serve_runtime::ReloadIdentity>,
            service_name: &str,
            script_path: &str,
            no_prelude: bool,
        ) -> Evaluator {
            let mut eval = Evaluator::new();
            eval.set_limits(script_limits());
            apply_arity_mode(&mut eval);
            eval.set_bus_handler(bus_handler.clone());
            eval.set_serve_runtime(std::rc::Rc::new(
                serve_runtime::MixServeRuntime::with_script_path(
                    service_name,
                    script_path,
                    identity.clone(),
                ),
            ));
            repl::register_ai_extensions(&mut eval);
            if !no_prelude {
                eval.load_prelude().await;
            }
            if stats_io::stats_enabled() {
                eval.attach_stats(UsageStats::for_execution(StatsContext::new(
                    ExecutionMode::Serve,
                    Some(Path::new(script_path)),
                )));
                if let Some(mut stats) = eval.stats_mut() {
                    stats.increment_commands();
                }
            }
            eval.set_global("0", Value::String(script_path.to_string()));
            // `include` resolves relative to the serve script's directory.
            eval.set_file(script_path);
            eval
        }

        let mut eval = build_serve_eval(&bus_handler, &identity, service_name, script_path, no_prelude).await;
        cosmix_mix::interrupt::init(eval.interrupt_flag());

        // Serve mode ALWAYS enters the pump after the init body — it is
        // a resident daemon, not a script with an optional event tail.
        //
        // SPEC 18 §3.5 (WS5) — the THREE shutdown triggers converge on
        // ONE graceful path:
        //   * SIGTERM (the systemd stop signal) and Ctrl-C, both via
        //     the inlined `shutdown_signal()`;
        //   * the Ch02 `QUIT` universal, via the WS4 `"quit"` pump break
        //     (`run_event_pump` returns `Ok` → `PumpEnded`).
        // All three fall through to a deterministic three-step
        // sequence: (1) the select! below stops accepting new inbound
        // by completing the pump future; (2) the post-select deregister
        // bounded by `DEREGISTER_GRACE` cleans the broker registry
        // before any local cancellation; (3) Phase 2 WS3-C.7f
        // `Evaluator::drain_class_c_for_shutdown` joins/aborts
        // in-flight Class C tasks and synthesizes §3.4 shutdown
        // replies for any pending-request handles (when the socket is
        // still live — see `allow_synth_replies` derivation below).
        // Class S chains never spawn — they run inline on the pump
        // future and finish/cancel with it.
        // The pump runs inside a loop so a reserved `RELOAD` (a clean pump
        // end with the reload flag set) can hot-swap the evaluator without
        // ever leaving this function — the broker connection, registration,
        // and signal wiring all survive. Load-beside-swap: the OLD
        // evaluator stays alive until the new script's top-level has
        // executed successfully; any failure resumes the old one, state
        // intact. Every other pump end falls through to the shutdown path
        // exactly as before.
        // ONE shutdown future for the whole serve lifetime — pinned so it is
        // never re-registered (a per-iteration re-register would leave a
        // signal-delivery gap across a reload), and raced against BOTH the
        // pump AND the reload's init body so a hanging/sleeping new script
        // is still killable by SIGTERM/Ctrl-C (codex arm MAJOR-3).
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        // Grace for draining a generation's in-flight Class C work at a swap
        // point (not process shutdown; the connection lives on).
        let reload_drain = std::time::Duration::from_secs(2);
        // `stmts` is the FIRST generation's init body, executed once on entry;
        // a reload executes its own `new_stmts` inline in the reload branch,
        // so this is never re-executed after iteration one.
        let mut needs_exec = true;
        let outcome = loop {
            let do_exec = needs_exec;
            needs_exec = false;
            let iter_outcome = tokio::select! {
                biased;
                _ = &mut shutdown => {
                    tracing::info!(
                        service = %service_name,
                        "serve: SIGTERM/Ctrl-C received; graceful shutdown (SPEC 18 §3.5)"
                    );
                    ServeOutcome::Interrupted
                }
                res = async {
                    if do_exec {
                        eval.execute(&stmts).await?;
                    }
                    eval.run_event_pump().await?;
                    Ok::<_, cosmix_mix::error::MixError>(())
                } => match res {
                    Ok(()) => ServeOutcome::PumpEnded,
                    Err(cosmix_mix::error::MixError::ExitRequest { code }) => {
                        ServeOutcome::ExitRequested(code)
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        if msg.contains("interrupted") {
                            ServeOutcome::Interrupted
                        } else {
                            tracing::error!(
                                service = %service_name,
                                error = %msg,
                                "serve: script error"
                            );
                            ServeOutcome::Error
                        }
                    }
                }
            };

            if !(matches!(iter_outcome, ServeOutcome::PumpEnded) && eval.take_reload_request()) {
                break iter_outcome;
            }

            // ── hot-reload (SPEC 18 RELOAD) ─────────────────────────────
            // The runtime already parse-validated and replied rc:0; a race
            // (file changed since) or a runtime failure in the new init
            // body reverts to the old evaluator — loudly, never silently.
            tracing::info!(service = %service_name, "serve: RELOAD accepted; building replacement evaluator");
            let new_source = match std::fs::read_to_string(script_path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(service = %service_name, error = %e,
                        "serve: reload REVERTED — script re-read failed; old script resumes");
                    continue;
                }
            };
            let new_stmts = match cosmix_mix::lexer::Lexer::new(&new_source)
                .tokenize()
                .and_then(|t| cosmix_mix::parser::Parser::new(t, &new_source).parse_program())
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(service = %service_name, error = %format!("{e}"),
                        "serve: reload REVERTED — source no longer parses (changed since validation?); old script resumes");
                    continue;
                }
            };
            let mut new_eval =
                build_serve_eval(&bus_handler, &identity, service_name, script_path, no_prelude).await;
            // interrupt::init is once-only, bound to the FIRST evaluator's
            // flag — share that flag so the evaluator-internal interrupt path
            // keeps working after any number of reloads.
            new_eval.set_interrupt_flag(eval.interrupt_flag());

            // End the OLD generation's spawn(argv, {die_with_parent: true})
            // children BEFORE the new init runs (review MINOR-6): an init that
            // spawns its helper again would otherwise get a second one beside
            // the old (the goose case — two servers, one port). The registry
            // is process-wide, so sweeping after the commit would also end
            // the helper the new init just started. Cost, documented: a
            // reload that then REVERTS resumes the old script without them.
            let swept = owned_spawns_sweep_count();
            if swept > 0 {
                tracing::info!(service = %service_name, swept,
                    "serve: RELOAD ended the old generation's owned children");
            }

            // Execute the new init body RACED against shutdown: a new script
            // whose top-level sleeps/hangs must still yield to SIGTERM. A
            // top-level `sleep()` dispatches events, so the new (not-yet-
            // committed) evaluator can spawn Class C tasks during init — on
            // ANY exit from here that does not commit the swap, its tasks
            // MUST be cancelled or they outlive the discarded evaluator and
            // reply on the shared connection after the old one resumes
            // (codex arm MAJOR-6). `reload_drain` with synth replies answers
            // anyone who got in rather than stranding them (MAJOR-5).
            let exec_res = tokio::select! {
                biased;
                _ = &mut shutdown => {
                    let drained = new_eval.drain_class_c_for_shutdown(reload_drain, true).await;
                    tracing::info!(service = %service_name,
                        aborted = drained.aborted, synth_sent = drained.synth_sent,
                        "serve: SIGTERM/Ctrl-C during reload init; cancelled replacement, shutting down");
                    break ServeOutcome::Interrupted;
                }
                r = new_eval.execute(&new_stmts) => r,
            };
            match exec_res {
                Ok(_) => {
                    // Old evaluator's in-flight Class C work is drained with
                    // synth replies BEFORE it drops — the connection is live,
                    // so a stranded caller gets a terminal reply, not silence.
                    let drained = eval.drain_class_c_for_shutdown(reload_drain, true).await;
                    // Flush the retiring generation's usage stats before it
                    // drops — otherwise a reloaded citizen discards every
                    // bucket since the last reload (only the FINAL evaluator
                    // is flushed at exit).
                    if let Some(stats) = eval.take_stats() {
                        stats_io::flush_batch(stats);
                    }
                    eval = new_eval;
                    // Bump the shared identity ONLY here, once the swap has
                    // committed — so a caller polling lifecycle.generation
                    // sees it advance exactly once per live swap, never on a
                    // refused or reverted reload.
                    identity.committed_reload();
                    tracing::info!(
                        service = %service_name,
                        handler_count = eval.handler_count(),
                        aborted = drained.aborted,
                        synth_sent = drained.synth_sent,
                        "serve: hot-reload complete; new script live"
                    );
                }
                Err(e) => {
                    // Cancel any Class C tasks the failed init admitted, so
                    // the discarded evaluator leaves nothing replying behind
                    // the resumed old one.
                    let drained = new_eval.drain_class_c_for_shutdown(reload_drain, true).await;
                    // The old generation's owned children were swept before
                    // this init ran, so the registry now holds ONLY what the
                    // failed init spawned: sweeping here is exact (review R1).
                    let swept = owned_spawns_sweep_count();
                    tracing::error!(service = %service_name, error = %format!("{e}"),
                        aborted = drained.aborted, synth_sent = drained.synth_sent, swept,
                        "serve: reload REVERTED — new init body failed; old script resumes with state intact");
                }
            }
        };

        // §3.5 single shutdown path: deregister BEFORE exit so the
        // broker registry never retains a dead name. `deregister()`
        // (WS1 → WS0 `noded.deregister`) marks ShuttingDown and joins
        // the reconnect supervisor FIRST (race-free — it cannot swap a
        // freshly-reconnected live client after the liveness check),
        // then issues the RPC on the live connection. This supersedes
        // WS3's defensive bare `shutdown()`. Bounded so a wedged or
        // unreachable broker cannot hang process exit.
        //
        // The match yields THREE values that drive the next two steps:
        //   * `deregistered` — is the broker registry clean? Drives the
        //     exit-code branch below. `Disconnected` counts as clean
        //     because WS-close already evicted the name.
        //   * `allow_synth_replies` — is the caller transport channel
        //     live enough to deliver synth shutdown replies? Drives
        //     C.7f drain's Phase 3. `deregister()` itself only
        //     returns `Ok(())`, `Err(Disconnected)`, or
        //     `Err(Transport(_))`; permit synth on `Ok(())` and the
        //     `Transport(_)` arm (the call reached a live connection
        //     per the `Transport(_)` doc — the socket may still be
        //     usable, and any miss surfaces as `synth_failed`).
        //     Suppress on `Disconnected` (confirmed no socket) and
        //     on the `_elapsed` timeout (supervisor stopped, transport
        //     ambiguous). The catch-all `Ok(Err(_))` defensively
        //     permits synth for any non-Disconnected variant that may
        //     be added later — a fresh variant is most plausibly a
        //     wire-level error, not a hard "socket is gone."
        //     IMPORTANT: the synth path bypasses the supervised
        //     `ShuttingDown` gate via the dedicated
        //     `respond_parts_shutdown_synth` primitive (the C.7f
        //     drain is the ONE legitimate post-deregister outbound
        //     path; script-side `reply()` continues to be gated).
        //   * `drain_grace` — how long to wait for in-flight handle
        //     completion. `CLASSC_DRAIN_GRACE` for live/ambiguous
        //     transports (handlers may legitimately finish work that
        //     does not need the wire); `ZERO` for confirmed no-socket
        //     paths so survivors are aborted immediately ("transport-
        //     drop = immediate cancel" per task #60 / C.7f semantics).
        let (deregistered, allow_synth_replies, drain_grace) = match tokio::time::timeout(
            DEREGISTER_GRACE,
            supervised.deregister(),
        )
        .await
        {
            Ok(Ok(())) => {
                tracing::info!(service = %service_name, "serve: deregistered cleanly");
                (true, true, cosmix_mix::evaluator::CLASSC_DRAIN_GRACE)
            }
            Ok(Err(cosmix_lib_client::SupervisedError::Disconnected)) => {
                // No live socket: the broker already dropped this name
                // on WS-close (fatal terminal, or a noded bounce mid
                // shutdown). The registry is already clean — nothing to
                // deregister, so this is a clean stop, not a failure.
                // Drain with ZERO grace: any pending Class C tasks
                // cannot reach the wire, so waiting helps no caller.
                tracing::info!(
                    service = %service_name,
                    "serve: connection already gone; broker dropped name on WS-close"
                );
                (true, false, std::time::Duration::ZERO)
            }
            Ok(Err(e)) => {
                // RPC-level error on a call that did reach a live
                // connection (per `SupervisedError::Transport` doc) or
                // another non-Disconnected failure variant. The broker
                // registry may retain the name (`deregistered=false` →
                // non-zero exit), but the WS may still be live enough
                // to deliver synth replies; attempt them and let
                // `synth_failed` count any misses (cheaper than blanket
                // suppression that would hide live-socket-in-error
                // cases). Keep full drain grace — handlers may have
                // local cleanup that doesn't depend on the
                // (already-failed) deregister.
                tracing::warn!(
                    service = %service_name,
                    error = %e,
                    "serve: deregister RPC failed; broker registry may retain the name"
                );
                (false, true, cosmix_mix::evaluator::CLASSC_DRAIN_GRACE)
            }
            Err(_elapsed) => {
                tracing::warn!(
                    service = %service_name,
                    grace_s = DEREGISTER_GRACE.as_secs(),
                    "serve: deregister grace exceeded; best-effort supervisor stop"
                );
                // `deregister()` was dropped mid-flight by the timeout.
                // It sends the supervisor stop signal as its FIRST
                // action (before any await), so the supervisor is
                // already winding down regardless of where the cancel
                // landed; the supervisor observes that signal at its
                // explicit select checkpoints (idle wait, backoff
                // sleep, post-connect) and stops promptly — though an
                // in-progress `NodedClient::connect`/RPC inside the
                // reconnect loop runs to its own completion before the
                // next check. This `shutdown()` is the meaningful join
                // for the sub-case where the cancel landed *before* the
                // handle was taken (handle still in the Mutex); for the
                // other sub-cases it is an idempotent no-op. The hard
                // backstop that nothing (Tokio task or in-progress
                // connect/RPC) outlives process intent is the
                // unconditional `process::exit()` of run_serve's return;
                // a possibly-stale broker name is the §3.5
                // grace-exceeded case (non-zero exit below + broker
                // WS-close peer teardown), not a leak.
                //
                // Post-shutdown: any synth reply would land on a
                // `SupervisedError::ShuttingDown` arm; suppress and
                // use ZERO drain — supervisor is gone, no reason to
                // wait further.
                supervised.shutdown().await;
                (false, false, std::time::Duration::ZERO)
            }
        };

        // SPEC 18 Phase 2 WS3-C.7f.2 — drain Class C in-flight chains
        // AFTER deregister so the broker registry is clean before any
        // local cancellation. Per the C.7f slicing consult: deregister
        // first guarantees no new request is routed at us mid-drain,
        // which would otherwise race the task-registry snapshot.
        //
        // Order matters: drain BEFORE `process::exit` (handed off by
        // run_serve's return) so survivor tasks are aborted with a
        // bounded join, and pending-request handles get a §3.4
        // shutdown synth reply when the socket is still live. The
        // synth path goes through the SAME `BusHandler::reply` wire
        // primitive author code uses, so a synth reply IS visible to
        // an in-flight caller — when `allow_synth_replies` is true.
        //
        // The drain is a fixed bounded operation (`drain_grace` for
        // handle wait + 100 ms for abort wind-down). The grace is
        // either `CLASSC_DRAIN_GRACE` or `ZERO` per the deregister
        // outcome (see above). It cannot hang process exit; the
        // unconditional `process::exit()` in the caller is the hard
        // backstop for any tokio-task that does not observe abort.
        let drain_outcome = eval
            .drain_class_c_for_shutdown(drain_grace, allow_synth_replies)
            .await;
        let drain_unclean = drain_outcome.aborted > 0 || drain_outcome.synth_failed > 0;
        if drain_unclean {
            tracing::warn!(
                service = %service_name,
                initial_tasks = drain_outcome.initial_tasks,
                initial_pending = drain_outcome.initial_pending,
                drained_clean = drain_outcome.drained_clean,
                aborted = drain_outcome.aborted,
                synth_sent = drain_outcome.synth_sent,
                synth_failed = drain_outcome.synth_failed,
                synth_skipped_no_socket = drain_outcome.synth_skipped_no_socket,
                allow_synth_replies,
                "serve: Class C drain completed with survivors or synth failures (SPEC 18 §3.5/C.7f)"
            );
        } else {
            tracing::info!(
                service = %service_name,
                initial_tasks = drain_outcome.initial_tasks,
                initial_pending = drain_outcome.initial_pending,
                drained_clean = drain_outcome.drained_clean,
                aborted = drain_outcome.aborted,
                synth_sent = drain_outcome.synth_sent,
                synth_failed = drain_outcome.synth_failed,
                synth_skipped_no_socket = drain_outcome.synth_skipped_no_socket,
                allow_synth_replies,
                "serve: Class C drain completed (SPEC 18 §3.5/C.7f)"
            );
        }

        let exit_code = match outcome {
            ServeOutcome::Error => 1,
            // The script's explicit status is exact even if best-effort serve
            // teardown logged a deregister/drain failure above.
            ServeOutcome::ExitRequested(code) => code,
            ServeOutcome::Interrupted | ServeOutcome::PumpEnded => {
                if deregistered {
                    tracing::info!(service = %service_name, "serve: exited cleanly");
                    0
                } else {
                    // §3.5: grace exceeded / deregister failed — exit
                    // non-zero so systemd records an unclean stop and an
                    // operator can investigate a possibly-stale name.
                    1
                }
            }
        };
        if let Some(stats) = eval.take_stats() {
            stats_io::flush_batch(stats);
        }
        exit_code
    }))
}

fn check_syntax(source: &str, filename: &str) -> i32 {
    let mut lexer = cosmix_mix::lexer::Lexer::new(source);
    match lexer.tokenize() {
        Ok(tokens) => {
            let mut parser = cosmix_mix::parser::Parser::new(tokens, source);
            match parser.parse_program() {
                Ok(_) => {
                    println!("{}: OK", filename);
                    0
                }
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("{}", e);
            1
        }
    }
}

/// Stack size for the evaluation thread. The evaluator's async frames
/// are large in unoptimized builds (~64 KiB each in debug), so the
/// default ~8 MiB main-thread stack overflows at ~120 native recursion
/// frames — BELOW the 128 recursion-depth cap, turning runaway Mix
/// recursion into an uncatchable native stack overflow instead of the
/// clean "recursion depth exceeded" error. Running `real_main` on a
/// dedicated 64 MiB thread (the rustc approach) gives the cap ~10x
/// headroom in debug and even more in release. Children spawned with
/// PR_SET_PDEATHSIG key off this thread's lifetime, which now ends
/// microseconds before process exit — semantically unchanged.
const MAIN_STACK_SIZE: usize = 64 * 1024 * 1024;

fn main() {
    // `--version` answers from a COLD process, before anything below runs:
    // no base-env capture, no session lane, no Bus dispatch, no evaluation
    // thread, no prelude, no rc. Mark's contract (2026-09-21) is that a
    // version query does nothing except report the version and the build
    // hash — including while another mix is already running, which is the
    // case that made the old placement wrong: the arm lived inside
    // `real_main`, so `mix --version` had already started a native session.
    {
        let args: Vec<String> = env::args().collect();
        if let Some(text) = meta::version_request(&args, VERSION) {
            println!("{text}");
            return;
        }
    }
    // Before native_session::start(), because that begins Bus dispatch and a
    // shell.task.submit can arrive immediately. Every invocation mode serves
    // the task verbs, so capturing this from the REPL alone would leave a
    // `mix -c` session handing tasks an environment with no PATH at all.
    session_task::capture_base_env();
    native_session::start();
    job_control::stage_entry();
    let handle = std::thread::Builder::new()
        .name("mix-eval".into())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(eval_thread_main)
        .expect("spawn mix evaluation thread");
    let code = handle.join().unwrap_or(101);
    // A panicked evaluation thread never reached its sweep; PDEATHSIG has
    // already killed the direct children, this reaches their groups.
    owned_spawns_sweep();
    // Kill-on-drop: pdeathsig reaches each task LEADER when its supervisor
    // thread goes, but nothing would reach the leader's own children. This is
    // the only point every invocation mode passes through on the way out.
    session_task::sweep();
    std::process::exit(code);
}

/// End `spawn(argv, {die_with_parent: true})` children with this process.
/// Idempotent — the registry drains on the first call.
pub(crate) fn owned_spawns_sweep() {
    owned_spawns_sweep_count();
}

/// [`owned_spawns_sweep`], reporting how many groups it ended.
#[cfg(target_os = "linux")]
pub(crate) fn owned_spawns_sweep_count() -> usize {
    cosmix_mix::builtins::owned_spawns::sweep()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn owned_spawns_sweep_count() -> usize {
    0
}

/// The evaluation thread's body: run, then end `spawn(argv, {die_with_parent:
/// true})` children — SIGTERM their groups, grace, SIGKILL — on THIS thread,
/// before it exits, because their PDEATHSIG is keyed to it and would otherwise
/// SIGKILL them first with no chance to clean up (TODO-mix P2).
fn eval_thread_main() -> i32 {
    // This thread lives until exit and sweeps before it ends, so it is the
    // one host allowed to create owned children.
    #[cfg(target_os = "linux")]
    let _ = cosmix_mix::builtins::owned_spawns::enable();
    let code = real_main();
    owned_spawns_sweep();
    code
}

fn real_main() -> i32 {
    // Freeze the process-wide kill-switch decision before any prelude or rc
    // code can mutate the environment.
    let _ = stats_io::stats_enabled();
    let args: Vec<String> = env::args().collect();

    // No arguments → REPL
    if args.len() < 2 {
        return repl::run_repl();
    }

    let mut i = 1;
    let mut no_prelude = false;
    let mut interactive_rc = false;
    let mut result_fd: Option<crate::result_fd::ResultFd> = None;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                return 0;
            }
            // Normally unreachable: `main()` answers a version query before
            // this thread exists. Kept so the flag is still honoured if
            // `real_main` is ever reached another way, and delegating to the
            // same function so the two can never drift.
            "--version" | "-V" => {
                println!(
                    "{}",
                    meta::version_request(&args, VERSION)
                        .unwrap_or_else(|| meta::version_line_build(VERSION))
                );
                return 0;
            }
            "--builtins" => {
                meta::cmd_builtins(&[], VERSION);
                return 0;
            }
            "--check" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("mix: --check requires a filename");
                    return 1;
                }
                let filename = &args[i];
                let source = match fs::read_to_string(filename) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error reading '{}': {}", filename, e);
                        return 1;
                    }
                };
                return check_syntax(&source, filename);
            }
            "-i" => {
                // Interactive-config one-shot, à la `bash -ci`: load ~/.mixrc
                // (aliases + toolkit PATH) before the `-c` command runs.
                interactive_rc = true;
                i += 1;
                continue;
            }
            "-c" | "-ci" | "-ic" => {
                // `-ci`/`-ic` are the combined `bash -ci` spelling.
                if args[i] != "-c" {
                    interactive_rc = true;
                }
                i += 1;
                if i >= args.len() {
                    eprintln!("mix: -c requires a code string");
                    return 1;
                }
                let code = &args[i];
                let script_args: Vec<String> = args[i + 1..].to_vec();
                return run_command_line(code, interactive_rc, &script_args, no_prelude, result_fd);
            }
            "--result-fd" => {
                i += 1;
                let raw = args.get(i).and_then(|value| value.parse::<i32>().ok());
                let Some(raw) = raw else {
                    eprintln!("mix: --result-fd requires a descriptor number");
                    return 2;
                };
                // `-c` is the only mode that produces a value to frame. Without
                // one the flag would be accepted and then quietly ignored,
                // which is the same silent-no-result failure the validation
                // below exists to prevent — so refuse it here too.
                //
                // It must match how the loop below actually PARSES, not merely
                // whether the token appears: `-c` takes the next argument as
                // its source, so a trailing `-c` with nothing after it — or one
                // that a script path has already consumed as an argument — is
                // not a `-c` mode at all. Searching for the token alone
                // accepted `mix --result-fd 3 script.mix -c` and then ran the
                // script, silently framing nothing.
                let has_code_mode = args[i + 1..]
                    .iter()
                    .position(|arg| arg == "-c")
                    .is_some_and(|at| {
                        // Every token before it must be a flag; the first
                        // non-flag is a script path, and the mode is settled.
                        args[i + 1..][..at].iter().all(|arg| arg.starts_with('-'))
                            && args[i + 1..].len() > at + 1
                    });
                if !has_code_mode {
                    eprintln!("mix: --result-fd is only meaningful with -c <source>");
                    return 2;
                }
                // Refuse at startup, before any user code runs. A task promised
                // a structured result that silently produced none is the
                // failure with no symptom.
                match crate::result_fd::ResultFd::validate(raw) {
                    Ok(validated) => result_fd = Some(validated),
                    Err(error) => {
                        eprintln!("mix: {error}");
                        return 2;
                    }
                }
                i += 1;
                continue;
            }
            "--no-prelude" => {
                no_prelude = true;
                i += 1;
                continue;
            }
            "--no-traceback" => {
                NO_TRACEBACK.store(true, std::sync::atomic::Ordering::Relaxed);
                i += 1;
                continue;
            }
            "--strict-arity" => {
                STRICT_ARITY.store(true, std::sync::atomic::Ordering::Relaxed);
                i += 1;
                continue;
            }
            "--gui" => return run_gui(),
            "--serve" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("mix: --serve requires a script path");
                    eprintln!("Usage: mix --serve <script> [--name <svc>]");
                    return 1;
                }
                let script_path = args[i].clone();
                i += 1;
                // Trailing options after the script path: `--name <svc>`
                // and `--no-prelude`, in any order. Anything else is a
                // usage error (deterministic, no positional script
                // args in serve mode — a daemon has no argv).
                let mut explicit_name: Option<String> = None;
                while i < args.len() {
                    match args[i].as_str() {
                        "--name" => {
                            i += 1;
                            if i >= args.len() {
                                eprintln!("mix: --name requires a value");
                                return 1;
                            }
                            explicit_name = Some(args[i].clone());
                            i += 1;
                        }
                        "--no-prelude" => {
                            no_prelude = true;
                            i += 1;
                        }
                        other => {
                            eprintln!("mix: unexpected argument after --serve script: '{}'", other);
                            eprintln!("Usage: mix --serve <script> [--name <svc>]");
                            return 1;
                        }
                    }
                }
                let name = match derive_serve_name(explicit_name.as_deref(), &script_path) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("mix: {}", e);
                        return 1;
                    }
                };
                return run_serve(&script_path, &name, no_prelude);
            }
            "-" => {
                // Read the program from stdin. This is the residue-free
                // remote-exec transport: `ssh host /usr/local/bin/mix -`
                // pipes the script over the stdin byte channel, so it is
                // never written to remote disk and never sits in an argv
                // position — no shell re-quoting, nothing to clean up.
                // Explicit only: bare `mix` with piped stdin still starts
                // the REPL (see main()'s args.len() < 2 guard), so an
                // accidental pipe can't silently execute a script.
                let mut source = String::new();
                if let Err(e) = io::stdin().read_to_string(&mut source) {
                    eprintln!("mix: error reading script from stdin: {}", e);
                    return 1;
                }
                let script_args: Vec<String> = args[i + 1..].to_vec();
                return run_source(&source, Some("-"), &script_args, no_prelude);
            }
            arg if arg.starts_with('-') => {
                eprintln!("mix: unknown option '{}'", arg);
                eprintln!("Try 'mix --help' for usage.");
                return 1;
            }
            "stats" => {
                // One-shot `mix stats [subcmd ...]`. Bypass the REPL,
                // load stats from disk, dispatch to the shared
                // `cmd_stats_dispatch` used by the REPL meta-command.
                // Any remaining args are forwarded as the subcommand.
                let sub_args: Vec<String> = args[i + 1..].to_vec();
                return run_stats_subcommand(&sub_args);
            }
            "lint" => {
                // `mix lint` owns its exit code (0/1/2 — the CI
                // contract), so it CANNOT ride the meta path, which
                // exits 0 unconditionally. A CWD script named `lint`
                // is shadowed like the meta names — run it as ./lint.
                let sub_args: Vec<String> = args[i + 1..].to_vec();
                return lint::run_lint(&sub_args, VERSION);
            }
            "edit" => {
                // `mix edit` owns its exit code (0 edited / 1 absent /
                // 2 ambiguous / 3 usage), which is the whole point of
                // the subcommand — so like `lint` it cannot ride the
                // meta path, which exits 0 unconditionally. A CWD
                // script named `edit` is shadowed; run it as ./edit.
                let sub_args: Vec<String> = args[i + 1..].to_vec();
                return edit::run_edit(&sub_args);
            }
            name if META_CLI_COMMANDS.contains(&name) => {
                // One-shot meta command — `mix help`, `mix builtins`,
                // `mix keywords`, `mix mesh`, etc. Dispatches through
                // the shared `meta::dispatch` used by the REPL, but
                // with a minimal Evaluator (no user session state).
                let sub_args: Vec<String> = args[i..].to_vec();
                let code = run_meta_subcommand(&sub_args);
                return code;
            }
            _ => {
                // First non-flag argument is the script filename
                let filename = &args[i];
                let source = match fs::read_to_string(filename) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error reading '{}': {}", filename, e);
                        return 1;
                    }
                };
                let script_args: Vec<String> = args[i + 1..].to_vec();
                return run_source(&source, Some(filename), &script_args, no_prelude);
            }
        }
    }

    // If we get here with no script, start REPL
    repl::run_repl()
}

#[cfg(test)]
mod gui_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn require_executable_image_rejects_non_elf_non_shebang() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mix-gui-image-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let write_exec = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            let mut f = fs::File::create(&p).unwrap();
            f.write_all(bytes).unwrap();
            let mut perm = f.metadata().unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&p, perm).unwrap();
            p
        };
        // ELF magic and a #! script are launchable images and pass.
        let elf = write_exec("elfish", b"\x7fELF\x02\x01\x01");
        assert!(require_executable_image(&elf).is_ok());
        let script = write_exec("scripty", b"#!/bin/sh\nexit 0\n");
        assert!(require_executable_image(&script).is_ok());
        // A +x bare-shell file (no shebang) is exactly the ENOEXEC→/bin/sh trap
        // and MUST be rejected so `--gui` cannot silently exit 0 running a shell.
        let bare = write_exec("bareshell", b"echo hi\nexit 0\n");
        assert!(require_executable_image(&bare).is_err());
        // A missing path is not launchable.
        assert!(require_executable_image(&dir.join("nope")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The PATH tier's candidates must reach `which` as BARE names.
    ///
    /// `which` treats an absolute argument as the candidate itself, so a name
    /// that gets absolutised is looked for at `<cwd>/<name>` and PATH is never
    /// consulted. That is exactly what shipped with the T1 rename: the
    /// resolution list gained `bterm` while the lookup's exemption still named
    /// only `term`, so a `bterm` installed solely on PATH could not be found
    /// and `mix --gui` reported no frontend installed at all.
    ///
    /// Proven able to fail: restore the old literal `path == Path::new("term")`
    /// in `is_bare_frontend_name` and the `bterm` candidate reds.
    #[test]
    fn path_tier_candidates_reach_which_as_bare_names() {
        let mut seen = Vec::new();
        let _ = resolve_term(None, Some("/test-root".into()), |path| {
            seen.push(path.to_path_buf());
            None
        });
        let path_tier = &seen[seen.len() - TERM_FRONTENDS.len()..];
        for candidate in path_tier {
            assert_eq!(
                candidate.parent(),
                Some(Path::new("")),
                "PATH tier handed a qualified path: {candidate:?}"
            );
            assert!(
                is_bare_frontend_name(candidate),
                "{candidate:?} would be absolutised, so PATH is never searched"
            );
        }
        // And a qualified path is still absolutised rather than PATH-searched.
        assert!(!is_bare_frontend_name(Path::new("/opt/cosmix/bin/bterm")));
        assert!(!is_bare_frontend_name(Path::new("./term")));
    }

    #[test]
    fn resolution_order_and_missing_frontend() {
        let mut seen = Vec::new();
        let error = resolve_term(None, Some("/test-root".into()), |path| {
            seen.push(path.to_path_buf());
            None
        })
        .unwrap_err();
        // Tier before name: a $COSMIX dev build of EITHER frontend beats an
        // installed one, so `mix --gui` never silently runs the system
        // terminal while a local build is what is under test.
        //
        // Name order within a tier is D10 and this assertion is what holds it:
        // `bterm` before `term` keeps the daily driver the daily driver, so
        // installing the iced skeleton cannot quietly take over `mix --gui`.
        // If you are here because this test failed after you reordered the
        // list, that is the test doing its job — flipping the order is a T6
        // decision, not a refactor.
        assert_eq!(
            seen,
            [
                "/test-root/bin/bterm",
                "/test-root/bin/term",
                "/opt/cosmix/bin/bterm",
                "/opt/cosmix/bin/term",
                "bterm",
                "term",
            ]
            .map(std::path::PathBuf::from)
        );
        assert!(error.starts_with("mix --gui: no CosMix terminal frontend is installed"));
        assert!(error.ends_with("Install the desktop package."));
        // The message must state the order actually tried. Pinning only the
        // ends let two sibling error strings in other crates go stale the
        // moment D10 flipped the order, each naming a sequence its own list
        // no longer used.
        assert!(
            error.contains(&TERM_FRONTENDS.join(" then ")),
            "the error must name the frontends in TERM_FRONTENDS order: {error}"
        );
        for (winner, expected) in seen.iter().enumerate() {
            let mut index = 0;
            let found = resolve_term(None, Some("/test-root".into()), |path| {
                let selected = index == winner;
                index += 1;
                selected.then(|| path.to_path_buf())
            })
            .unwrap();
            assert_eq!(&found, expected);
            assert_eq!(index, winner + 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn executable_override_and_rejections() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fake-term");
        fs::write(&file, b"test fixture, never executed").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            resolve_term(Some(file.clone().into_os_string()), None, term_lookup).unwrap(),
            file
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        for path in [&file, dir.path(), &dir.path().join("missing")] {
            let error =
                resolve_term(Some(path.as_os_str().to_owned()), None, term_lookup).unwrap_err();
            assert!(error.contains("COSMIX_TERM_BIN must name an executable"));
        }
    }
}

#[cfg(test)]
mod serve_name_tests {
    use super::{derive_serve_name, strip_cosmix_prefix};

    #[test]
    fn strips_only_a_leading_cosmix_prefix() {
        assert_eq!(strip_cosmix_prefix("cosmix-statecache"), "statecache");
        assert_eq!(strip_cosmix_prefix("statecache"), "statecache");
        // Not a prefix match — left intact.
        assert_eq!(strip_cosmix_prefix("my-cosmix-thing"), "my-cosmix-thing");
        // Only the FIRST `cosmix-` is stripped (single strip_prefix).
        assert_eq!(strip_cosmix_prefix("cosmix-cosmix-x"), "cosmix-x");
        assert_eq!(strip_cosmix_prefix("cosmix-"), "");
    }

    #[test]
    fn default_name_is_the_script_stem_in_bus_form() {
        // The reference citizen: POSIX user cosmix-statecache,
        // Bus service name statecache.
        assert_eq!(
            derive_serve_name(None, "/usr/local/lib/cosmix/statecache.mix").unwrap(),
            "statecache"
        );
        // Bare filename, no directory, no extension.
        assert_eq!(derive_serve_name(None, "worker").unwrap(), "worker");
        // A script accidentally named after the POSIX user still
        // yields the canonical Bus identity (never `cosmix-*`).
        assert_eq!(
            derive_serve_name(None, "/opt/cosmix-statecache.mix").unwrap(),
            "statecache"
        );
    }

    #[test]
    fn explicit_name_wins_and_is_canonicalised() {
        assert_eq!(
            derive_serve_name(Some("probe"), "/x/statecache.mix").unwrap(),
            "probe"
        );
        // `--name cosmix-foo` is still canonicalised to the Bus form.
        assert_eq!(
            derive_serve_name(Some("cosmix-foo"), "/x/statecache.mix").unwrap(),
            "foo"
        );
        assert_eq!(
            derive_serve_name(Some("  spaced  "), "/x/s.mix").unwrap(),
            "spaced"
        );
    }

    #[test]
    fn anonymous_serve_is_rejected() {
        // No --name and no derivable stem (empty / root path).
        assert!(derive_serve_name(None, "").is_err());
        assert!(derive_serve_name(None, "/").is_err());
        // `--name` present but empty (or only the strippable prefix /
        // whitespace) is a launch error, not a nameless citizen.
        assert!(derive_serve_name(Some(""), "/x/s.mix").is_err());
        assert!(derive_serve_name(Some("   "), "/x/s.mix").is_err());
        assert!(derive_serve_name(Some("cosmix-"), "/x/s.mix").is_err());
    }

    #[test]
    fn dotfile_only_stem_is_not_a_valid_bus_name() {
        // `Path::file_stem()` of a hidden file is the whole `.`-led
        // name (`.foo`, `.mix`) — that is NOT a Bus identity, so the
        // default path must reject it rather than register `.foo`.
        assert!(derive_serve_name(None, ".foo").is_err());
        assert!(derive_serve_name(None, "/etc/.mix").is_err());
        assert!(derive_serve_name(None, "/srv/.statecache").is_err());
        // The same invariant applies to an explicit `--name`.
        assert!(derive_serve_name(Some(".bad"), "/x/statecache.mix").is_err());
        // A normal stem that merely *contains* a dot is unaffected
        // (file_stem already drops the extension).
        assert_eq!(
            derive_serve_name(None, "/x/state.cache.mix").unwrap(),
            "state.cache"
        );
    }

    /// `serve_name()` (0.91.0) answers the DERIVED name, through the same
    /// seam `run_serve`'s `build_serve_eval` uses: the derivation feeds
    /// `MixServeRuntime::with_script_path`, which `set_serve_runtime`
    /// installs. The broker connect in front of it is not needed to prove
    /// the plumbing, so this drives the evaluator directly.
    #[tokio::test(flavor = "current_thread")]
    async fn serve_name_builtin_answers_the_derived_name() {
        use cosmix_mix::evaluator::{Evaluator, SharedBuf};
        use cosmix_mix::lexer::Lexer;
        use cosmix_mix::parser::Parser;
        use std::rc::Rc;

        async fn probe(explicit: Option<&str>, script: &str) -> String {
            let name = derive_serve_name(explicit, script).unwrap();
            let rt = crate::serve_runtime::MixServeRuntime::with_script_path(
                name,
                script,
                Rc::new(crate::serve_runtime::ReloadIdentity::new()),
            );
            let source = "print(serve_name())\n";
            let stmts = Parser::new(Lexer::new(source).tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            let stdout = SharedBuf::new();
            let mut eval =
                Evaluator::with_output(Box::new(stdout.clone()), Box::new(SharedBuf::new()));
            eval.set_serve_runtime(Rc::new(rt));
            eval.execute(&stmts).await.unwrap();
            stdout.to_string_lossy()
        }

        // --name wins.
        assert_eq!(probe(Some("probe"), "/x/quoin-panel.mix").await, "probe\n");
        // Stem fallback, with the `cosmix-` prefix normalised away.
        assert_eq!(probe(None, "/x/quoin-panel.mix").await, "quoin-panel\n");
        assert_eq!(probe(None, "/opt/cosmix-statecache.mix").await, "statecache\n");
    }
}
