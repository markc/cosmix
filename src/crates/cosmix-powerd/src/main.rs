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

fn main() -> anyhow::Result<()> {
    // --version/-V first, before the tokio runtime exists: a thread- or
    // fd-starved host must still get an answer, not a runtime-build panic.
    cosmix_buildinfo::exit_on_version!();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build the tokio runtime")
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve => cosmix_powerd::citizen::serve().await,
    }
}
