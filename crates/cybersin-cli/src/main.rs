//! `cybersin` — the compiler CLI (spec §11).
//!
//! This issue implements `check`, `init`, and `fmt` (spec §6.1's frontend
//! surface). Every other subcommand in the full §11 surface (`build`,
//! `run`, `sessions`, `trace`, ...) belongs to later issues; the
//! [`Command`] enum below is meant to grow one variant per issue, each
//! dispatching to its own `commands::<name>` module, so adding a command
//! elsewhere doesn't need to touch the arms already here.

mod commands;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cybersin",
    version,
    about = "Cybersin prompt compiler + agent runtime CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a prompt source (or every source in a project) through the
    /// compiler frontend: parse, resolve `!include`s, typecheck inputs,
    /// emit IR. Exits nonzero with a clear error on any failure.
    Check {
        /// A `*.prompt.yaml` file, or a project (or `prompts/`) directory.
        path: PathBuf,
    },
    /// Scaffold a new project layout (spec §5): `cybersin.yaml`,
    /// `cybersin.lock`, `prompts/`, `fragments/`, `evals/`, `agents/`,
    /// `dist/`, plus one working example prompt.
    Init {
        /// Directory to scaffold the project into (created if missing).
        dir: PathBuf,
    },
    /// Normalize the formatting of a `*.prompt.yaml` source file.
    Fmt {
        /// The prompt source file to format.
        path: PathBuf,
        /// Only check whether the file is already canonically formatted;
        /// don't write, exit nonzero if it isn't.
        #[arg(long)]
        check: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check { path } => commands::check::run(&path),
        Command::Init { dir } => commands::init::run(&dir),
        Command::Fmt { path, check } => commands::fmt::run(&path, check),
    };
    match result {
        Ok(message) => {
            if let Some(message) = message {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
