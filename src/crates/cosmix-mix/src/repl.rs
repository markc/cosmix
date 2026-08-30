use std::env;

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::stats::{ExecutionMode, StatsContext, UsageStats};
use cosmix_mix::value::Value;
use rustyline::Editor;
use rustyline::error::ReadlineError;

use crate::completion::MixHelper;
use crate::exec::{self, PipelineResult};
use crate::jobs::JobTable;
use crate::meta;
use crate::shell::{self, InputKind};
use crate::stats_io;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RESUME_FLAG: &str = ".claude-resume";

/// Ensure the terminal performs output post-processing (NL -> CR-NL) while
/// interactive Mix drives it.
///
/// rustyline snapshots the tty's termios at the *start of each* `readline()`,
/// enters raw mode, then restores that per-call snapshot on return — it
/// deliberately leaves the output flags (`c_oflag`) untouched. So Mix never
/// clears `ONLCR` itself, but it also never *repairs* it: if Mix is launched
/// with `OPOST`/`ONLCR` already off (a login shell inheriting a raw tty, or a
/// prior child that exited without restoring it), every external command's
/// LF-terminated output "staircases" — a bare `\n` moves the cursor down one
/// row without returning it to column 0, so each line marches rightward.
///
/// Mix owns exactly these two output bits while interactive and re-asserts
/// them on both stdout and stderr (each tty-guarded). Consequence (intentional,
/// tested): a `stty -onlcr` does not persist across prompts in interactive Mix
/// — Mix re-enables CR-NL output every cycle. Everything else — input flags,
/// local flags, erase/flow-control/signal characters — is left alone: rustyline
/// manages raw input per readline, and we don't clobber later `stty` changes to
/// unrelated settings. Best-effort: any failure just leaves the staircase, it
/// never disrupts the REPL. Called at REPL startup, at the top of each loop, and
/// again after the prompt is built (which runs arbitrary user `prompt()` code)
/// immediately before `readline()`.
fn ensure_interactive_output_mode() {
    // External commands staircase on either output stream, so repair both: with
    // `mix -i >session.log` stdout is a file (a harmless no-op below) while the
    // controlling terminal — and the command stderr still on it — is the fd that
    // needs the fix. Each call is isatty-guarded and idempotent.
    ensure_output_post_processing(libc::STDOUT_FILENO);
    ensure_output_post_processing(libc::STDERR_FILENO);
}

/// fd-taking core of [`ensure_interactive_output_mode`], split out so it can be
/// unit-tested against a pty pair. OR-s `OPOST | ONLCR` into `c_oflag` and
/// touches nothing else; a no-op when `fd` is not a tty or the bits are set.
fn ensure_output_post_processing(fd: libc::c_int) {
    unsafe {
        if libc::isatty(fd) != 1 {
            return;
        }
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            return;
        }
        let want = libc::OPOST | libc::ONLCR;
        if termios.c_oflag & want == want {
            // Already sane — avoid a needless tcsetattr each prompt.
            return;
        }
        termios.c_oflag |= want;
        let _ = libc::tcsetattr(fd, libc::TCSANOW, &termios);
    }
}

/// Check for the claude-resume flag file in $COSMIX_SRC.
/// Returns the resume command if the flag exists.
fn check_resume_flag() -> Option<String> {
    let flag = crate::cosmix_paths::cosmix_src().join(RESUME_FLAG);
    if flag.exists() {
        std::fs::read_to_string(&flag).ok()
    } else {
        None
    }
}

/// Delete the resume flag file.
fn clear_resume_flag() {
    let _ = std::fs::remove_file(crate::cosmix_paths::cosmix_src().join(RESUME_FLAG));
}

/// exec() into the new Mix binary, preserving args so the new instance
/// picks up the resume flag on startup.
fn exec_restart(
    eval: &mut Evaluator,
    rl: &mut Editor<MixHelper, rustyline::history::DefaultHistory>,
    history_path: &std::path::Path,
) -> ! {
    // Save state before exec
    if let Some(mut stats) = eval.take_stats() {
        stats_io::save_stats(&mut stats);
    }
    let _ = rl.save_history(&history_path);

    let mix_bin = crate::cosmix_paths::cosmix_path(crate::cosmix_paths::CosmixDir::Bin)
        .join("mix")
        .to_string_lossy()
        .into_owned();
    eprintln!("Restarting Mix...");
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&mix_bin).exec();
    eprintln!("Failed to restart: {}", err);
    std::process::exit(1);
}

/// Run the interactive REPL.
pub fn run_repl() -> i32 {
    meta::init_start_time();

    // Establish a sane interactive output baseline first thing — before the
    // prelude, .mixrc, or a resume command can print/run, any of which would
    // staircase on a raw inherited tty (see ensure_interactive_output_mode).
    ensure_interactive_output_mode();

    let rt = crate::build_runtime();

    let history_path = dirs::home_dir()
        .map(|h| h.join(".mix_history"))
        .unwrap_or_default();

    let helper = MixHelper::new();
    let state = helper.state.clone();

    let mut rl = match Editor::new() {
        Ok(mut rl) => {
            rl.set_helper(Some(helper));
            rl
        }
        Err(e) => {
            eprintln!("Failed to initialize readline: {}", e);
            return 1;
        }
    };

    let _ = rl.load_history(&history_path);

    let mut eval = Evaluator::new();
    eval.set_limits(crate::script_limits());
    eval.set_bus_handler(std::rc::Rc::new(crate::bus::MixBusHandler::new()));
    // Match REPL semantics inside `source`: a sourced file may mix
    // bareword shell commands with Mix code (this is the whole point
    // of a .mixrc). The handler only activates when the whole-file
    // Mix parse fails — pure-Mix sourced files run unchanged.
    eval.set_shell_handler(std::rc::Rc::new(
        crate::shell_handler::ReplShellHandler::new(),
    ));

    // Register AI extension functions
    register_ai_extensions(&mut eval);

    // Wire SIGINT to interrupt flag (for interrupting running Mix code).
    // Two paths cooperate here:
    //   1. cosmix_mix::interrupt::init installs a `signal-hook` SIGINT
    //      handler that runs in async-signal-safe context and stores
    //      `true` into the same Arc<AtomicBool>. This is what allows
    //      blocking builtins like ssh_run to notice Ctrl-C while a
    //      synchronous syscall is on the stack — tokio's ctrl_c future
    //      can't make progress while the current-thread runtime is
    //      blocked on the builtin.
    //   2. The tokio task below remains as a no-op safety net for the
    //      pre-existing path that the script-mode select! in main.rs
    //      relies on. Both handlers chain via signal_hook_registry, so
    //      both fire on each Ctrl-C.
    // In the REPL, rustyline handles Ctrl-C in raw mode as
    // ReadlineError::Interrupted. The handlers below only run when
    // code is executing between prompts (cooked mode restored).
    let flag = eval.interrupt_flag();
    cosmix_mix::interrupt::init(flag.clone());
    // A second handle on the same flag, kept in the loop's scope — the
    // original `flag` is moved into the Ctrl-C watcher task below. Used to
    // discard a stale interrupt before each prompt (see the loop top).
    let idle_flag = flag.clone();
    rt.spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });

    // Load prelude before .mixrc
    rt.block_on(eval.load_prelude());

    let mut line_buf = String::new();
    let mut dir_stack: Vec<String> = Vec::new();
    let mut job_table = JobTable::new();
    let mut auto_diagnose = false;

    // Load ~/.mixrc if it exists
    if let Some(code) = load_mixrc(&mut eval, &rt) {
        return code;
    }
    if stats_io::stats_enabled() {
        eval.attach_stats(UsageStats::for_execution(StatsContext::new(
            ExecutionMode::Interactive,
            None,
        )));
    }

    // Check for claude-resume flag (written by /mix-build skill).
    // If found, auto-start claude --continue to resume the conversation.
    if let Some(cmd) = check_resume_flag() {
        clear_resume_flag();
        let cmd = cmd.trim().to_string();
        if !cmd.is_empty() {
            eprintln!("Resuming: {}", cmd);
            if let Ok(pipeline) = exec::parse_pipeline(&cmd, &exec::NoVars) {
                let _ = exec::execute_pipeline(&pipeline);
            }
        }
    }

    let mut exit_code = 0;
    'repl: loop {
        // Discard any interrupt left over from a just-finished command. When
        // Ctrl-C lands during a blocking child (e.g. `tail -f` via run_stream),
        // the child takes the SIGINT and run_stream returns normally, so the
        // flag the signal handler set is never consumed by an expression-
        // boundary poll. Left set, it trips the very next prompt() evaluation
        // in build_prompt, which then silently falls back to the uncolored
        // default prompt until the next keypress clears the flag. We're idle at
        // the prompt here, so clearing it keeps the colored prompt stable.
        idle_flag.store(false, std::sync::atomic::Ordering::SeqCst);

        // Check for completed background jobs
        job_table.reap();

        // Update completion state from evaluator
        {
            let mut s = state.borrow_mut();
            s.variable_names = eval.scope().variable_names();
            s.alias_names = eval.aliases().keys().cloned().collect();
        }

        // Re-assert CR-NL output before we render the prompt: a foreground child
        // that exited leaving the tty raw would otherwise staircase the prompt
        // itself and every command for the rest of the session.
        ensure_interactive_output_mode();

        let prompt = if line_buf.is_empty() {
            match build_prompt(&mut eval, &rt) {
                Ok(prompt) => prompt,
                Err(code) => {
                    exit_code = code;
                    break 'repl;
                }
            }
        } else {
            "  > ".to_string()
        };

        // build_prompt() ran the user's `prompt()` function — arbitrary Mix that
        // could itself have re-poisoned the tty (e.g. `run_stream(["stty",
        // "-onlcr"])`). Repair once more so the mode rustyline is about to
        // snapshot in readline() is the sane one.
        ensure_interactive_output_mode();

        match rl.readline(&prompt) {
            Ok(line) => {
                if line_buf.is_empty() && line.trim().is_empty() {
                    continue;
                }

                if !line_buf.is_empty() {
                    line_buf.push('\n');
                }
                line_buf.push_str(&line);

                // `time <line>` — a modifier, not a command (bash's `time` is a
                // keyword; there is no `time` binary to exec). Stripped BEFORE
                // alias expansion so the wrapped head still expands
                // (`time ll` → `time ls -l`), and before classification so it
                // wraps whatever the rest turns out to be: external command,
                // pipeline, chain, bareword function, or Mix code. `mix time
                // EXPR` is the same modifier — one timing semantic, not two.
                let (timed, work) = match shell::strip_time_prefix(&line_buf) {
                    Some("") => {
                        eprintln!("mix: time: usage: time <command | mix expression>");
                        line_buf.clear();
                        continue;
                    }
                    Some(rest) => (true, rest.to_string()),
                    None => (false, line_buf.clone()),
                };

                // Expand aliases before classification
                let (input, alias_name) = {
                    let aliases = eval.aliases();
                    let alias_name = work
                        .split_whitespace()
                        .next()
                        .filter(|name| aliases.contains_key(*name))
                        .map(str::to_string);
                    (shell::expand_alias(&work, &aliases), alias_name)
                };

                // Track alias usage
                if let Some(alias_name) = alias_name
                    && let Some(mut s) = eval.stats_mut()
                {
                    s.track_alias(&alias_name);
                }

                let kind = {
                    let aliases = eval.aliases();
                    let functions = eval.function_names();
                    shell::classify_input_fns(&input, &aliases, &functions)
                };
                // Arm the elapsed report only for the arms that actually RUN
                // something — Empty/Incomplete/ParseError `continue` below, and a
                // half-typed `time for $i in …` continuation must not report an
                // elapsed for a line that never executed. Using the guard rather
                // than a print after the match is what catches the arms that
                // `continue` out early having really run the command: `time cd
                // /tmp`, `time a && b`, and the job-control builtins.
                let executing = !matches!(
                    kind,
                    InputKind::Empty | InputKind::Incomplete | InputKind::ParseError(_)
                );
                let mut _timer = shell::TimeGuard::armed(timed && executing);
                match kind {
                    InputKind::Incomplete => {
                        // Don't clear line_buf — wait for more input
                        continue;
                    }
                    InputKind::Empty => {
                        line_buf.clear();
                        continue;
                    }
                    InputKind::ParseError(msg) => {
                        // `classify_input` no longer prints; the REPL surfaces
                        // the real Mix lex/parse error and keeps the session.
                        if let Some(mut s) = eval.stats_mut() {
                            s.track_error(&msg);
                        }
                        eprintln!("{}", msg);
                        line_buf.clear();
                        continue;
                    }
                    InputKind::MixCode(stmts) => {
                        let _ = rl.add_history_entry(line_buf.trim());
                        line_buf.clear();

                        if stmts.is_empty() {
                            continue;
                        }

                        match rt.block_on(eval.execute(&stmts)) {
                            Ok(Value::Nil) => {
                                if let Some(mut s) = eval.stats_mut() {
                                    s.increment_commands();
                                }
                            }
                            Ok(val) => {
                                if let Some(mut s) = eval.stats_mut() {
                                    s.increment_commands();
                                }
                                let trimmed = input.trim();
                                if !trimmed.starts_with("print") && !trimmed.starts_with("eprint") {
                                    println!("{}", val.to_mix_string());
                                }
                            }
                            Err(MixError::ExitRequest { code }) => {
                                let _ = rl.save_history(&history_path);
                                exit_code = code;
                                break 'repl;
                            }
                            Err(e) => {
                                if let Some(mut s) = eval.stats_mut() {
                                    s.track_error(&format!("{}", e));
                                }
                                eprintln!("{}", e);

                                // Auto-diagnose: if enabled, ask Claude to diagnose the error
                                if auto_diagnose {
                                    let error_str = format!("{}", e);
                                    let diag_prompt = format!(
                                        "Diagnose this Mix language error concisely (1-2 sentences): {}",
                                        error_str
                                    );
                                    match std::process::Command::new("claude")
                                        .args(["-p", &diag_prompt, "--output-format", "text"])
                                        .output()
                                    {
                                        Ok(out) if out.status.success() => {
                                            let advice = String::from_utf8_lossy(&out.stdout);
                                            eprintln!(
                                                "\x1b[36m[diagnose]\x1b[0m {}",
                                                advice.trim()
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    InputKind::FunctionCommand { name, args } => {
                        // Bareword call of a defined function (`sc restart nginx`).
                        // Runs it as `sc("restart", "nginx")` — the ported bash
                        // toolkit works with no parens. Result handling mirrors the
                        // MixCode arm (print a non-nil value unless it self-printed).
                        let _ = rl.add_history_entry(line_buf.trim());
                        line_buf.clear();

                        match rt.block_on(eval.call_function_by_name_with_args(&name, &args)) {
                            Ok(Value::Nil) => {
                                if let Some(mut s) = eval.stats_mut() {
                                    s.increment_commands();
                                }
                            }
                            Ok(val) => {
                                if let Some(mut s) = eval.stats_mut() {
                                    s.increment_commands();
                                }
                                println!("{}", val.to_mix_string());
                            }
                            Err(MixError::ExitRequest { code }) => {
                                let _ = rl.save_history(&history_path);
                                exit_code = code;
                                break 'repl;
                            }
                            Err(e) => {
                                if let Some(mut s) = eval.stats_mut() {
                                    s.track_error(&format!("{}", e));
                                }
                                eprintln!("{}", e);
                            }
                        }
                    }
                    InputKind::ExternalCommand(command) => {
                        let _ = rl.add_history_entry(line_buf.trim());
                        line_buf.clear();

                        // Trace shell-dispatch lines too when `mix trace on` — a
                        // command (external binary, chain/pipeline, or a shell
                        // builtin like `cd`) never reaches the evaluator's
                        // per-statement tracer, so this arm is the only place to
                        // surface it. The two emit sites below sit AFTER the
                        // command head is known: a chain is always traced as a
                        // whole line, and a single command is traced unless its
                        // parsed program is `mix` (the `status`/`trace`/`vars`…
                        // meta-commands are REPL machinery, excluded like the
                        // prompt() render). Emitting on the raw `input` up here
                        // instead would mis-handle a `mix …; real-cmd` chain (the
                        // whole line suppressed) and an `ENV=v mix …` prefix.
                        let trace_shell = eval.trace();

                        // A &&/||/; chain runs as an external command list with
                        // short-circuit semantics, bypassing the single-command
                        // builtin handling below (chains are all-external). The
                        // chain is split from LITERAL text STRUCTURALLY (no
                        // resolver) so the probe never runs a `$(...)`; each piece
                        // is then expanded+run (via &eval) only when its connector
                        // selects it — so a substitution in a skipped branch never
                        // executes, and a lone command is not double-executed.
                        if let Ok(items) = exec::split_command_list(&command)
                            && items.len() > 1
                        {
                            if trace_shell {
                                eprintln!("trace <repl> shell: {}", command.trim());
                            }
                            let outcome = exec::execute_command_list_outcome(
                                &items,
                                &eval,
                                Some(&mut job_table),
                            );
                            if let Some(mut stats) = eval.stats_mut() {
                                for command in &outcome.commands {
                                    stats.track_command(command);
                                }
                            }
                            // Background is read from what the chain actually RAN,
                            // not from a scan of the pieces: a `&` in a branch the
                            // connectors skip (`false && sleep 5 &`) spawns nothing,
                            // so that line still has a real foreground duration and
                            // stays timeable. A job that IS spawned leaves only
                            // spawn latency to report — cancel the timer instead.
                            if outcome.backgrounded {
                                _timer.disarm();
                                if timed {
                                    eprintln!("mix: time: backgrounded (&) — not timed");
                                }
                            }
                            eval.set_global("status", Value::Number(outcome.code as f64));
                            if let Some(mut s) = eval.stats_mut() {
                                s.increment_commands();
                            }
                            continue;
                        }

                        let pipeline = match exec::parse_pipeline(&command, &eval) {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("{}", e);
                                continue;
                            }
                        };

                        let first = &pipeline.segments[0].program;
                        // Single-command trace (see the chain emit above): the
                        // parsed program is the real command head — env-prefixes
                        // (`ENV=v cmd`) already split off — so excluding `mix`
                        // here correctly skips the meta-commands without a
                        // brittle first-word string match. Emitted before the
                        // builtin dispatch so `cd`/`exit`/etc. trace too.
                        // A `mix …` meta-command runs in-process and prints
                        // straight to the terminal, so it cannot honor shell
                        // plumbing on the line — a pipe tail, a redirect
                        // (`>` `>>` `<` `2>` …), a background `&`, or an env
                        // prefix. Such lines used to be intercepted anyway
                        // with the plumbing silently DROPPED (`mix stats
                        // never | wc -l` printed the report; `> x` created
                        // nothing). A plumbed line now skips interception:
                        // stateless subcommands run as a real external
                        // pipeline (same output; live stats flushed first so
                        // `stats` reads current data), state-bound ones are
                        // refused loudly in the second `"mix"` arm below.
                        let meta_plumbed = first == "mix" && pipeline_has_plumbing(&pipeline);
                        // Trace-suppress only the in-process meta path: a
                        // plumbed `mix …` line (a `mix ./job.mix | cat`
                        // script pipeline, a falling-through `mix stats |
                        // wc`, even a refusal) is real shell dispatch and
                        // traces like any other command.
                        if trace_shell && (first != "mix" || meta_plumbed) {
                            eprintln!("trace <repl> shell: {}", command.trim());
                        }
                        // Set when a plumbed meta line records itself under
                        // its meta category, so the generic external-command
                        // tracking below doesn't ALSO record it as command
                        // `mix` (the bare in-process path records only the
                        // meta category).
                        let mut plumbed_meta_recorded = false;
                        match first.as_str() {
                            "exit" => {
                                let code = pipeline.segments[0]
                                    .args
                                    .first()
                                    .and_then(|s| s.parse::<i32>().ok())
                                    .unwrap_or(0);
                                let _ = rl.save_history(&history_path);
                                exit_code = code;
                                break 'repl;
                            }
                            "cd" => {
                                handle_cd(&pipeline.segments[0].args, &mut eval);
                                continue;
                            }
                            "pushd" => {
                                if !pipeline.segments[0].args.is_empty() {
                                    if let Ok(cwd) = env::current_dir() {
                                        dir_stack.push(cwd.to_string_lossy().to_string());
                                    }
                                    handle_cd(&pipeline.segments[0].args, &mut eval);
                                } else {
                                    eprintln!("pushd: no directory specified");
                                }
                                continue;
                            }
                            "popd" => {
                                if let Some(dir) = dir_stack.pop() {
                                    handle_cd(&[dir], &mut eval);
                                } else {
                                    eprintln!("popd: directory stack empty");
                                }
                                continue;
                            }
                            "history" => {
                                for (i, entry) in rl.history().iter().enumerate() {
                                    println!("{:5}  {}", i + 1, entry);
                                }
                                continue;
                            }
                            "which" | "type" => {
                                for arg in &pipeline.segments[0].args {
                                    match which_command(arg) {
                                        Some(path) => println!("{}", path),
                                        None => eprintln!("{}: not found", arg),
                                    }
                                }
                                continue;
                            }
                            "unalias" => {
                                for arg in &pipeline.segments[0].args {
                                    if !eval.remove_alias(arg) {
                                        eprintln!("unalias: {}: not found", arg);
                                    }
                                }
                                continue;
                            }
                            "jobs" => {
                                job_table.list();
                                continue;
                            }
                            "fg" => {
                                let id = pipeline.segments[0]
                                    .args
                                    .first()
                                    .and_then(|s| s.parse::<usize>().ok());
                                if let Some(code) = job_table.fg(id) {
                                    eval.set_global("status", Value::Number(code as f64));
                                }
                                continue;
                            }
                            "bg" => {
                                eprintln!(
                                    "bg: not yet implemented (jobs run in background by default with &)"
                                );
                                continue;
                            }
                            "mix" if !meta_plumbed => {
                                let meta_args: Vec<&str> = pipeline.segments[0]
                                    .args
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect();
                                // Track meta-command usage
                                if let Some(subcmd) = meta_args.first()
                                    && let Some(mut s) = eval.stats_mut()
                                {
                                    s.track_meta(subcmd);
                                }
                                // Handle subcommands that need REPL state
                                match meta_args.first().copied() {
                                    Some("history") => {
                                        let pattern = meta_args.get(1).copied();
                                        for (i, entry) in rl.history().iter().enumerate() {
                                            if let Some(pat) = pattern
                                                && !entry.contains(pat)
                                            {
                                                continue;
                                            }
                                            println!("{:5}  {}", i + 1, entry);
                                        }
                                    }
                                    Some("reload") => {
                                        if let Some(code) = load_mixrc(&mut eval, &rt) {
                                            exit_code = code;
                                            break 'repl;
                                        }
                                        println!("Reloaded .mixrc");
                                    }
                                    Some("trace") => match meta_args.get(1).copied() {
                                        Some("on") => {
                                            eval.set_trace(true);
                                            println!("Trace: on");
                                        }
                                        Some("off") => {
                                            eval.set_trace(false);
                                            println!("Trace: off");
                                        }
                                        _ => {
                                            println!(
                                                "Trace is {}",
                                                if eval.trace() { "on" } else { "off" }
                                            );
                                            println!("Usage: mix trace on|off");
                                        }
                                    },
                                    // `mix time EXPR` is no longer a meta-command:
                                    // it is normalized to the `time` modifier at
                                    // input intake (shell::strip_time_prefix), so
                                    // it now times shell commands and bareword
                                    // functions too, not just Mix code, and never
                                    // reaches this match.
                                    Some("diagnose") => match meta_args.get(1).copied() {
                                        Some("on") => {
                                            auto_diagnose = true;
                                            println!(
                                                "Auto-diagnose: on (errors will be sent to Claude)"
                                            );
                                        }
                                        Some("off") => {
                                            auto_diagnose = false;
                                            println!("Auto-diagnose: off");
                                        }
                                        _ => {
                                            println!(
                                                "Auto-diagnose is {}",
                                                if auto_diagnose { "on" } else { "off" }
                                            );
                                            println!("Usage: mix diagnose on|off");
                                        }
                                    },
                                    Some("stats") => match eval.stats_mut() {
                                        Some(mut stats) => {
                                            let _ = stats_io::cmd_stats_dispatch(
                                                &meta_args[1..],
                                                Some(&mut *stats),
                                            );
                                        }
                                        None => {
                                            let _ =
                                                stats_io::cmd_stats_dispatch(&meta_args[1..], None);
                                        }
                                    },
                                    _ => {
                                        if let Some(exec_path) =
                                            meta::dispatch(&meta_args, &eval, VERSION)
                                        {
                                            // Save stats and history before exec'ing into new binary
                                            if let Some(mut stats) = eval.take_stats() {
                                                stats_io::save_stats(&mut stats);
                                            }
                                            let _ = rl.save_history(&history_path);
                                            use std::os::unix::process::CommandExt;
                                            let err = std::process::Command::new(&exec_path).exec();
                                            eprintln!("Failed to restart: {}", err);
                                            if stats_io::stats_enabled() {
                                                eval.attach_stats(UsageStats::for_execution(
                                                    StatsContext::new(
                                                        ExecutionMode::Interactive,
                                                        None,
                                                    ),
                                                ));
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                            // Plumbed `mix …` line (pipe/redirect/&/env
                            // prefix): the in-process meta path can't honor
                            // it. Refuse subcommands bound to this shell's
                            // live state — an external child would silently
                            // answer from different state — and let every
                            // other subcommand fall out of this match to run
                            // as a normal external pipeline below.
                            "mix" => {
                                let sub = pipeline.segments[0]
                                    .args
                                    .first()
                                    .map(String::as_str)
                                    .unwrap_or("");
                                // `stats reset`/`stats clear` mutate the
                                // persisted store under an invariant that
                                // spans THIS shell's live collector (see
                                // `reset_current_week`): an external child
                                // can wipe the disk but not the collector,
                                // so the exit flush would silently recreate
                                // supposedly-reset data. Refuse those too.
                                let stats_mutation = sub == "stats"
                                    && matches!(
                                        pipeline.segments[0].args.get(1).map(String::as_str),
                                        Some("reset") | Some("clear")
                                    );
                                if meta_needs_repl_state(sub) || stats_mutation {
                                    eprintln!(
                                        "mix{}{}: runs inside the interactive shell — \
                                         pipes, redirects and & are not supported; run it bare",
                                        if sub.is_empty() { "" } else { " " },
                                        sub
                                    );
                                    eval.set_global("status", Value::Number(2.0));
                                    continue;
                                }
                                // Record a real meta name under its meta
                                // category, matching the bare in-process
                                // path — and BEFORE the stats drain, so a
                                // `mix stats` report includes its own
                                // invocation like the bare path does. A
                                // plumbed `mix ./script.mix | …` is not a
                                // meta name and keeps plain command
                                // tracking.
                                let is_meta_name =
                                    matches!(sub, "stats" | "lint" | "--version" | "-V")
                                        || crate::META_CLI_COMMANDS.contains(&sub);
                                if is_meta_name {
                                    plumbed_meta_recorded = true;
                                    if let Some(mut s) = eval.stats_mut() {
                                        s.track_meta(sub);
                                    }
                                }
                                // Only a `stats` report benefits from seeing
                                // this session's pending counters: drain and
                                // flush them so the external child reads
                                // current data. The drain does NOT finalize
                                // the session (exit still records exactly
                                // one), and a failed flush merges back ONLY
                                // the unpersisted residual — counters are
                                // never lost or double-counted, the child
                                // just reads slightly stale data. Other
                                // subcommands (`builtins`, `man`, …) don't
                                // read the stats store, so no flush.
                                if sub == "stats"
                                    && let Some(mut stats) = eval.stats_mut()
                                {
                                    let delta = stats.drain_pending_buckets();
                                    if let Err(residual) = stats_io::flush_pending_delta(&delta) {
                                        stats.merge(&residual);
                                    }
                                }
                            }
                            _ => {}
                        }

                        let cmd_str = input.trim().to_string();

                        // Track external command usage — unless this line
                        // already recorded itself as a plumbed meta-command.
                        if !plumbed_meta_recorded
                            && let Some(mut s) = eval.stats_mut()
                            && let Some(first) = pipeline.segments.first()
                        {
                            s.track_command(&first.program);
                        }

                        match exec::execute_pipeline(&pipeline) {
                            Ok(PipelineResult::Done(status)) => {
                                let code = status.code().unwrap_or(-1);
                                eval.set_global("status", Value::Number(code as f64));
                                if let Some(mut s) = eval.stats_mut() {
                                    s.increment_commands();
                                }
                            }
                            // Backgrounded: the child was SPAWNED and we return to
                            // the prompt now, so the only thing a timer could
                            // report is spawn latency — which would read as the
                            // command's runtime. Cancel it.
                            Ok(PipelineResult::Background(child)) => {
                                if let Some(mut s) = eval.stats_mut() {
                                    s.increment_commands();
                                }
                                _timer.disarm();
                                if timed {
                                    eprintln!("mix: time: backgrounded (&) — not timed");
                                }
                                job_table.add(cmd_str, child);
                            }
                            Err(e) => {
                                eprintln!("{}: {}", pipeline.segments[0].program, e);
                                eval.set_global("status", Value::Number(127.0));
                            }
                        }

                        // After external command exits, check for claude-resume flag.
                        // If present, exec() into new Mix binary (which will then
                        // auto-start claude --continue on startup).
                        if check_resume_flag().is_some() {
                            exec_restart(&mut eval, &mut rl, &history_path);
                        }
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                if !line_buf.is_empty() {
                    line_buf.clear();
                    println!();
                } else {
                    println!("^C");
                }
            }
            Err(ReadlineError::Eof) => {
                if !line_buf.is_empty() {
                    match cosmix_mix::continuation::splice_explicit_continuations(&line_buf) {
                        Err(error) if error.is_incomplete_input() => eprintln!("{}", error),
                        _ => eprintln!("mix: unexpected EOF while input is incomplete"),
                    }
                }
                break;
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    // Save usage stats before exit
    if let Some(stats) = eval.take_stats() {
        stats_io::flush_batch(stats);
    }

    let _ = rl.save_history(&history_path);
    exit_code
}

fn build_prompt(eval: &mut Evaluator, rt: &tokio::runtime::Runtime) -> Result<String, i32> {
    if eval.has_function("prompt") {
        // Suppress statement tracing for the duration of the internal
        // `prompt()` render: with `mix trace on`, this user/.mixrc function
        // runs before every prompt and would otherwise spam the trace
        // channel with the REPL's own machinery (the phantom `<repl>:N`
        // lines), drowning out the statements the user is actually tracing.
        let traced = eval.trace();
        if traced {
            eval.set_trace(false);
        }
        let rendered = rt.block_on(eval.call_function_by_name("prompt"));
        if traced {
            eval.set_trace(true);
        }
        match rendered {
            Ok(val) => return Ok(val.to_mix_string()),
            Err(MixError::ExitRequest { code }) => return Err(code),
            Err(_) => {}
        }
    }

    let user = env::var("USER").unwrap_or_else(|_| "mix".to_string());
    let dir = env::current_dir()
        .map(|p| {
            let home = env::var("HOME").unwrap_or_default();
            let path = p.to_string_lossy().to_string();
            if !home.is_empty() && path.starts_with(&home) {
                format!("~{}", &path[home.len()..])
            } else {
                path
            }
        })
        .unwrap_or_else(|_| "?".to_string());

    let sigil = if user == "root" { "#" } else { "$" };
    Ok(format!(
        "\x1b[32m{}\x1b[0m:\x1b[34m{}\x1b[0m {} ",
        user, dir, sigil
    ))
}

/// REPL `cd`: the canonical `exec::builtin_cd` plus publishing the exit code
/// as `$?`. Delegating fixed two REPL-only divergences: `cd -` with OLDPWD
/// unset is now an error (was a silent no-op chdir to cwd), and `~user/...`
/// stays literal → ENOENT (was mangled to `$HOMEuser/...`).
fn handle_cd(args: &[impl AsRef<str>], eval: &mut Evaluator) {
    let args: Vec<String> = args.iter().map(|a| a.as_ref().to_string()).collect();
    let code = exec::builtin_cd(&args);
    eval.set_global("?", Value::Number(code as f64));
}

/// Source ~/.mixrc into `eval` within an existing async context (no
/// nested `block_on`). Shared by the REPL's `load_mixrc` wrapper and by
/// the binary's `mix -i -c` one-shot path (which is already inside a
/// `block_on(LocalSet)` and cannot nest another runtime).
pub(crate) async fn load_mixrc_async(eval: &mut Evaluator) -> Option<i32> {
    // Treat unset OR empty HOME as "no rc" — `PathBuf::from("").join(".mixrc")`
    // collapses to a relative `.mixrc` and would otherwise auto-source a
    // cwd-local file.
    let home = env::var_os("HOME").filter(|h| !h.is_empty());
    let home = home?;
    let rc_path = std::path::PathBuf::from(home).join(".mixrc");
    // stat (following symlinks) rather than `exists()`: sourcing OPENS the
    // path, so a FIFO at ~/.mixrc would block REPL startup forever. Only a
    // regular file — or a symlink to one, the normal dotfile-manager case —
    // is sourced; anything else is skipped with a warning.
    let meta = match std::fs::metadata(&rc_path) {
        Ok(m) => m,
        Err(_) => return None, // no ~/.mixrc — nothing to load
    };
    if !meta.is_file() {
        eprintln!("mix: {}: not a regular file; skipping", rc_path.display());
        return None;
    }
    // Route through a synthetic `source "<path>"` statement so the
    // evaluator's `exec_source` path runs — that's the one wired to
    // the registered `ShellHandler` per-line fallback. Lexing the
    // file directly here would bypass the fallback entirely and
    // re-introduce the original bug where a `.mixrc` containing
    // bareword shell lines (e.g. `cd ~/foo`, `eza --version`)
    // alongside Mix code fails the whole-file Mix parse.
    let rc_str = rc_path.to_string_lossy().to_string();
    let stmts = vec![cosmix_mix::ast::Stmt::new(
        cosmix_mix::ast::StmtKind::Source {
            path: cosmix_mix::ast::Expr::StringLiteral(rc_str),
        },
        0,
    )];
    match eval.execute(&stmts).await {
        Ok(_) => None,
        Err(MixError::ExitRequest { code }) => Some(code),
        Err(e) => {
            eprintln!("{}: {}", rc_path.display(), e);
            None
        }
    }
}

fn load_mixrc(eval: &mut Evaluator, rt: &tokio::runtime::Runtime) -> Option<i32> {
    rt.block_on(load_mixrc_async(eval))
}

/// Register AI extension functions (ai, ai_diagnose, context).
pub fn register_ai_extensions(eval: &mut Evaluator) {
    use cosmix_mix::error::MixError;
    use cosmix_mix::evaluator::sync_ext;

    // ai(prompt [, context]) — call Claude CLI, return response as string
    eval.register(
        "ai",
        std::rc::Rc::new(|args| {
            Box::pin(async move {
                let prompt = args.first().map(|v| v.to_mix_string()).unwrap_or_default();

                if prompt.is_empty() {
                    return Err(MixError::RuntimeError {
                        msg: "ai(): requires a prompt string".to_string(),
                        span: None,
                    });
                }

                // If second arg provided, prepend as context
                let full_prompt = if args.len() > 1 {
                    let context = args[1].to_mix_string();
                    format!("Context: {}\n\n{}", context, prompt)
                } else {
                    prompt
                };

                let output = std::process::Command::new("claude")
                    .args(["-p", &full_prompt, "--output-format", "text"])
                    .output();

                match output {
                    Ok(out) if out.status.success() => Ok(Value::String(
                        String::from_utf8_lossy(&out.stdout).trim().to_string(),
                    )),
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        Err(MixError::RuntimeError {
                            msg: format!("ai(): claude error: {}", stderr.trim()),
                            span: None,
                        })
                    }
                    Err(e) => Err(MixError::RuntimeError {
                        msg: format!("ai(): claude CLI not available: {}", e),
                        span: None,
                    }),
                }
            })
        }),
    );

    // ai_diagnose(error_string) — ask Claude to diagnose an error
    eval.register(
        "ai_diagnose",
        std::rc::Rc::new(|args| {
            Box::pin(async move {
                let error_msg = args.first().map(|v| v.to_mix_string()).unwrap_or_default();

                if error_msg.is_empty() {
                    return Ok(Value::String("No error to diagnose".to_string()));
                }

                let prompt = format!(
                    "Diagnose this Mix scripting language error and suggest a fix. \
                 Mix is an ARexx-inspired language with $sigil variables, \
                 keyword-driven syntax (if/end, for/next, while/done), \
                 everything-is-a-string semantics, and function scope isolation. \
                 Be concise (2-3 sentences max).\n\nError: {}",
                    error_msg
                );

                let output = std::process::Command::new("claude")
                    .args(["-p", &prompt, "--output-format", "text"])
                    .output();

                match output {
                    Ok(out) if out.status.success() => Ok(Value::String(
                        String::from_utf8_lossy(&out.stdout).trim().to_string(),
                    )),
                    Ok(_) | Err(_) => Ok(Value::String(format!(
                        "(ai_diagnose unavailable) Error was: {}",
                        error_msg
                    ))),
                }
            })
        }),
    );

    // context() — return session state as a string (JSON summary)
    eval.register(
        "context",
        sync_ext(|_args| {
            let pid = std::process::id();
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            Ok(Value::String(format!("pid={}, cwd={}", pid, cwd)))
        }),
    );
}

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

/// True when a parsed command line carries shell plumbing the in-process
/// meta-command path cannot honor: a pipe tail, any redirect (`>` `>>` `<`
/// `2>` `2>&1` …), a background `&`, or an env-var prefix. A `mix …` line
/// with plumbing must never be intercepted silently — it either runs as a
/// real external pipeline or is refused, per [`meta_needs_repl_state`].
fn pipeline_has_plumbing(pipeline: &exec::Pipeline) -> bool {
    pipeline.segments.len() > 1
        || pipeline.background
        || !pipeline.segments[0].redirects.is_empty()
        || !pipeline.segments[0].env_vars.is_empty()
}

/// Meta-subcommands whose answer comes from live state of THIS shell
/// process: the REPL-loop-only commands handled before `meta::dispatch`
/// (rustyline history, `.mixrc` reload, the trace/diagnose toggles) plus
/// every dispatch arm that reads the live evaluator or exec-restarts the
/// process ([`meta::needs_live_eval`], kept next to the dispatch match). An
/// external `mix <sub>` child would silently answer these from different
/// state, so a plumbed invocation is refused instead of run externally.
fn meta_needs_repl_state(sub: &str) -> bool {
    matches!(sub, "history" | "reload" | "trace" | "diagnose") || meta::needs_live_eval(sub)
}

#[cfg(test)]
mod tests {
    use super::ensure_output_post_processing;
    use super::{meta_needs_repl_state, pipeline_has_plumbing};
    use crate::exec;

    fn parsed(line: &str) -> exec::Pipeline {
        exec::parse_pipeline(line, &exec::NoVars).expect(line)
    }

    /// The plumbing detector is what stops the meta-command path from
    /// silently eating `| wc -l` / `> x` / `>> x` on a `mix …` line
    /// (2026-08-21 report: `mix stats never | wc -l` printed the report and
    /// the pipe never ran; `> x` created no file). Every plumbing form the
    /// parser can represent must register.
    #[test]
    fn meta_plumbing_detects_every_form() {
        assert!(!pipeline_has_plumbing(&parsed("mix stats never")));
        assert!(!pipeline_has_plumbing(&parsed("mix builtins --json")));

        assert!(pipeline_has_plumbing(&parsed("mix stats never | wc -l")));
        assert!(pipeline_has_plumbing(&parsed("mix stats never > x")));
        assert!(pipeline_has_plumbing(&parsed("mix stats never >> x")));
        assert!(pipeline_has_plumbing(&parsed("mix stats never 2> err")));
        assert!(pipeline_has_plumbing(&parsed("mix stats never 2>&1")));
        assert!(pipeline_has_plumbing(&parsed("mix check f < input")));
        assert!(pipeline_has_plumbing(&parsed("mix stats never &")));
        assert!(pipeline_has_plumbing(&parsed("FOO=1 mix stats never")));
    }

    /// Subcommands answering from live REPL state are refused when plumbed
    /// (an external child would silently answer from different state);
    /// stateless ones fall through to a real external run.
    #[test]
    fn meta_state_classifier_splits_refuse_from_fallthrough() {
        for sub in [
            "",
            "history",
            "reload",
            "trace",
            "diagnose",
            "vars",
            "aliases",
            "functions",
            "all",
            "type",
            "status",
            "context",
            "snapshot",
            "ask",
            "chat",
            "build",
            "update",
        ] {
            assert!(meta_needs_repl_state(sub), "{sub:?} must refuse plumbing");
        }
        // Every fall-through subcommand must be one the external CLI actually
        // dispatches (META_CLI_COMMANDS or a dedicated arm like `stats`) —
        // otherwise a plumbed line dies with "Error reading '<sub>'".
        for sub in [
            "stats", "builtins", "man", "keywords", "help", "version", "config", "what",
        ] {
            assert!(!meta_needs_repl_state(sub), "{sub:?} must run externally");
            assert!(
                sub == "stats" || crate::META_CLI_COMMANDS.contains(&sub),
                "{sub:?} falls through but the external CLI does not dispatch it"
            );
        }
    }

    /// A raw-poisoned tty (OPOST/ONLCR cleared) is repaired to CR-NL output,
    /// and every other termios field is preserved untouched. This is the unit
    /// under the interactive staircase fix.
    #[test]
    fn repairs_onlcr_and_preserves_other_fields() {
        unsafe {
            let mut master: libc::c_int = 0;
            let mut slave: libc::c_int = 0;
            let rc = libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            );
            assert_eq!(rc, 0, "openpty failed");

            // Poison the slave: clear OPOST|ONLCR, and set a couple of unrelated
            // fields we can prove are preserved (a custom VERASE, ICRNL off).
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave, &mut t), 0);
            t.c_oflag &= !(libc::OPOST | libc::ONLCR);
            t.c_iflag &= !libc::ICRNL; // an input flag we must NOT touch
            t.c_cc[libc::VERASE] = 0x7f; // a control char we must NOT touch
            assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &t), 0);

            let before: libc::termios = {
                let mut b: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(slave, &mut b), 0);
                b
            };
            assert_eq!(before.c_oflag & (libc::OPOST | libc::ONLCR), 0, "setup");

            ensure_output_post_processing(slave);

            let after: libc::termios = {
                let mut a: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(slave, &mut a), 0);
                a
            };

            // Output post-processing restored.
            assert_eq!(
                after.c_oflag & (libc::OPOST | libc::ONLCR),
                libc::OPOST | libc::ONLCR,
                "OPOST|ONLCR must be set"
            );
            // Everything else is left exactly as it was.
            assert_eq!(after.c_iflag, before.c_iflag, "c_iflag must be untouched");
            assert_eq!(after.c_cflag, before.c_cflag, "c_cflag must be untouched");
            assert_eq!(after.c_lflag, before.c_lflag, "c_lflag must be untouched");
            assert_eq!(
                after.c_cc, before.c_cc,
                "the entire control-character array must be untouched"
            );
            // No output bit beyond the two we own was flipped on.
            assert_eq!(
                after.c_oflag & !(libc::OPOST | libc::ONLCR),
                before.c_oflag & !(libc::OPOST | libc::ONLCR),
                "no other c_oflag bit may change"
            );

            libc::close(slave);
            libc::close(master);
        }
    }

    /// Non-tty fds are a no-op (stdout redirected to a file/pipe must not error).
    #[test]
    fn non_tty_is_noop() {
        // A pipe read end is not a tty.
        let mut fds = [0 as libc::c_int; 2];
        unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            // Must simply return without touching anything / panicking.
            ensure_output_post_processing(fds[0]);
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
