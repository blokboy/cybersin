//! `cybersin` — the CLI binary (spec §1, §11).
//!
//! This issue wires up the runtime-facing slice of spec §11's surface:
//! `run --stub` (drives the M1 stub agent end-to-end against a
//! hand-written `dist/`), `trace ls|show`, and `cost --by <dim>`. Compile
//! commands (`build`, `check`, `fmt`, ...) and the rest of the runtime
//! surface (`sessions`, `approve`/`deny`, `dlq`, `sandbox`, `eval`,
//! `optimize`, `explain`) belong to later issues.
//!
//! `Command`'s variants are kept additive and self-contained (each
//! delegating immediately to its own `commands::*` module) since another
//! branch may be adding unrelated subcommands to this same crate
//! concurrently (e.g. the frontend's `build`/`check`, issue #3) — the only
//! shared surface is this enum itself.

mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Cybersin: a prompt compiler and agent runtime in one binary (spec §1).
#[derive(Debug, Parser)]
#[command(name = "cybersin", version, about)]
struct Cli {
    /// Path to `cybersind`'s SQLite state file (spec §8: Storage trait,
    /// SQLite in dev). Shared by every runtime subcommand, so `run --stub`
    /// followed by `trace`/`cost` in the same working directory sees the
    /// same recorded data.
    #[arg(long, global = true, default_value = ".cybersin/cybersin.db")]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run an agent session (spec §11: `cybersin run <agent.yaml>`; this
    /// issue: `cybersin run --stub`).
    Run(commands::run::RunArgs),

    /// Inspect recorded spans (spec §8.5: `cybersin trace ls|show`).
    Trace {
        #[command(subcommand)]
        command: commands::trace::TraceCommand,
    },

    /// Cost rollups (spec §8.5: `cybersin cost --by <dim>`).
    Cost(commands::cost::CostArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => commands::run::execute(cli.db, args).await,
        Command::Trace { command } => commands::trace::execute(cli.db, command).await,
        Command::Cost(args) => commands::cost::execute(cli.db, args).await,
    }
}
