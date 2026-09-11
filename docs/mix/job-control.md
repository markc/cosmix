# Interactive job control

Interactive Mix shell commands use one process group per job, including all
pipeline stages. The shell transfers its controlling terminal to foreground
jobs and reclaims it on completion or suspension. `jobs` lists tracked jobs;
`fg N` foregrounds and continues job N; `bg N` continues it without terminal
ownership. Omitting N selects the most recent live job. A background pipeline
returns to the prompt after launch, without waiting for its stages to finish.

Job management is selected by the interactive entry point, and requires tty
stdin plus a controlling terminal. Setup errors produce one warning and
start the shell with noninteractive execution policy. Foreground admission
is bounded if an orphaned process group cannot stop. The shutdown mode
snapshot is seeded after cold-start terminal repair.
Scripts, `-c` (including SSH commands),
serve mode, redirected stdin and sessions without a controlling terminal
retain their noninteractive launch policy. Captured `run_argv` and
`run_pipeline` retain their separate capture/cancellation process groups.
The shell's job-control signal handlers reset on exec. SIGTTOU is blocked
only around terminal handoff and restoration, so captured children do not
inherit shell-only ignored signals or a handoff-time signal mask.

Sourced shell input in an interactive shell shares its job controller. A
stopped foreground command inside source currently waits for external
continuation: returning to a restricted prompt while preserving evaluation
requires the later async host seam. `run_stream` and evaluator-owned legacy
shell/substitution paths are not integrated in this round. Bus publication,
remote admission and request cancellation are separate stages.

On normal shell exit or SIGHUP, Mix sends HUP followed by CONT to its owned
live jobs and allows 500 ms for exit/reaping. Survivors are reported; there
is no forced-kill escalation or guarantee about deliberately detached
descendants. Foreground terminal modes are restored even if a child exits
leaving raw mode enabled. Stopped jobs retain their own modes for `fg`.
Fresh jobs that exit normally retain cooked-mode changes (ICANON and ISIG still enabled), so
commands such as `stty tostop` remain effective. Stops, signal exits and raw
mode leakage restore the saved shell baseline. A resumed stopped job also
restores that baseline when it completes. Mix cannot infer whether an
arbitrary cooked-mode change was deliberate; this is the explicit policy.

The launch barrier runs in a private Mix trampoline **after** its first exec.
It executes `/proc/self/exe`, so open shells keep launching commands after
an installation replaces or unlinks their original executable.
This lets Rust's spawn acknowledgement complete before the barrier waits.
Only after all children share their job group and the foreground terminal
has transferred does Mix release target execution. A second close-on-exec
pipe reports target-exec failures and one-byte trampoline failure reasons.
Acknowledgement waits observe member stops and have a five-second deadline.
Failed launches reclaim the terminal first, send TERM/CONT, allow 500 ms,
then send KILL/CONT and allow another 500 ms. Survivors are reported and
remain registered for eventual reaping; an uninterruptible child cannot
hold the prompt indefinitely.

The process monitor is the sole consumer of registered child statuses,
including stopped/continued states. SIGCHLD wakes it independently of REPL
input or evaluator progress. It never waits for arbitrary child PIDs.
