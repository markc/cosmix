use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cosmix-powerd",
    version,
    about = "Event-driven UPower battery and power citizen"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register the `power` Bus service and monitor UPower.
    Serve,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --version/-V: answer and exit 0 before any other side effect.
    cosmix_buildinfo::exit_on_version!();
    match Cli::parse().command {
        Command::Serve => cosmix_powerd::citizen::serve().await,
    }
}
