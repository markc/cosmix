use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cosmix-mprisd",
    version,
    about = "Event-driven MPRIS2 media-player citizen"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register the mpris Bus service and monitor session MPRIS players.
    Serve,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve => cosmix_mprisd::citizen::serve().await,
    }
}
