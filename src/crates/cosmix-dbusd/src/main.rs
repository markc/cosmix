use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cosmix-dbusd",
    version,
    about = "D-Bus boundary daemon: hosts the per-domain D-Bus adapters"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register the `dbusd` Bus service and supervise the D-Bus adapters.
    Serve,
}

/// How long the runtime waits for remaining tasks after `serve()`
/// returns. Keep in step with the shutdown budget in
/// `citizen::serve` (the unit's `TimeoutStopSec=75`).
const RUNTIME_STOP: Duration = Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    let command = Cli::parse().command;
    // The runtime is built explicitly (not `#[tokio::main]`) so teardown
    // is bounded even with a wedged adapter run: a task that never
    // yields survives its abort, and `Runtime::shutdown_timeout` gives
    // it this window and then returns instead of hanging forever —
    // the leaked task dies with the process.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(match command {
        Command::Serve => cosmix_dbusd::citizen::serve(),
    });
    runtime.shutdown_timeout(RUNTIME_STOP);
    result
}
