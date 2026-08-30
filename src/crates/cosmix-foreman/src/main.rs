mod cli;
mod cmd;
mod task_cli;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let policy_check = matches!(&cli.command, Cmd::PolicyCheck { .. });
    match run(cli) {
        Err(error) if policy_check => {
            eprintln!("foreman policy: startup failed ({error:#}); denying");
            std::process::exit(2);
        }
        result => result,
    }
}

fn run(cli: Cli) -> Result<()> {
    cmd::run(cli)
}
