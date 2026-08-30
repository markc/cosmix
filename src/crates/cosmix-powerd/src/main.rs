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
    match Cli::parse().command {
        Command::Serve => cosmix_powerd::citizen::serve().await,
    }
}
