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
    // --version/-V: answer and exit 0 before any other side effect.
    cosmix_buildinfo::exit_on_version!();
    match Cli::parse().command {
        Command::Serve => cosmix_mprisd::citizen::serve().await,
    }
}
