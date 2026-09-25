use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cosmix-editd",
    version,
    about = "The `edit` Bus citizen: shared text buffers for humans and agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register the `edit` Bus service and serve buffers.
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
        Command::Serve => cosmix_editd::bus::serve().await,
    }
}
