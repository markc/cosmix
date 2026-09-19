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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve => cosmix_dbusd::citizen::serve().await,
    }
}
