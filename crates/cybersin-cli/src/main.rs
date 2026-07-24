//! `cybersin` — the CLI binary (spec §1, §11).
//!
//! Merges three issues' worth of subcommands onto one `Command` enum:
//! compile-side `check`/`init`/`fmt` (issue #3, spec §6.1), runtime-side
//! `run --stub`/`trace`/`cost` (issue #10, spec §8.5), and the tool
//! gateway's `dlq`/`approve`/`deny` (issue #11, spec §8.2). Each variant
//! dispatches immediately to its own `commands::*` module, so later
//! issues adding more subcommands (`build`, `sessions`, `sandbox`, `eval`,
//! `optimize`, `explain`, ...) only touch this enum, not the bodies below
//! it.
//!
//! Compile commands (`check`/`init`/`fmt`) are synchronous, pure
//! functions returning `Result<Option<String>, String>` — they never
//! touch the daemon. Runtime commands (`run`/`trace`/`cost`) are async
//! and return `anyhow::Result<()>`, since they auto-start `cybersind`
//! against a shared SQLite state file. `main` stays `ExitCode`-based
//! (rather than propagating `?` straight out of `main`) so both
//! conventions map to the same clean 0/1 exit-code contract.

mod commands;
mod git;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Cybersin: a prompt compiler and agent runtime in one binary (spec §1).
#[derive(Parser)]
#[command(
    name = "cybersin",
    version,
    about = "Cybersin prompt compiler + agent runtime CLI"
)]
struct Cli {
    /// Path to `cybersind`'s SQLite state file (spec §8: Storage trait,
    /// SQLite in dev). Shared by every runtime subcommand, so `run --stub`
    /// followed by `trace`/`cost` in the same working directory sees the
    /// same recorded data. Ignored by compile commands.
    #[arg(long, global = true, default_value = ".cybersin/cybersin.db")]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile and optimize a project.
    Build {
        /// Project directory containing prompts/ and cybersin.lock.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Build profile. `dev` excludes model-assisted compression.
        #[arg(long, value_enum, default_value = "release")]
        profile: commands::build::BuildProfile,
        /// Refuse any pass that would need a network call.
        #[arg(long)]
        frozen: bool,
        /// Rebuild automatically whenever a `*.prompt.yaml`,
        /// `cybersin.yaml`, or `cybersin.lock` source changes.
        #[arg(long)]
        watch: bool,
    },
    /// Compare the current build against a build of the same project
    /// checked out at another git ref (spec §7, §11): which prompts,
    /// routes, and budgets changed, and how.
    Diff {
        /// Git ref to compare against (branch, tag, or commit).
        reference: String,
        /// Project directory containing prompts/ and cybersin.lock.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Build profile used for both sides of the comparison.
        /// `release` only surfaces compressed-rewrite diffs once
        /// compression is pinned in `cybersin.lock` — a frozen release
        /// build refuses to compress anything that isn't.
        #[arg(long, value_enum, default_value = "dev")]
        profile: commands::build::BuildProfile,
    },
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
    /// Run an agent session (spec §11: `cybersin run <agent.yaml>`; for
    /// now: `cybersin run --stub`).
    Run(commands::run::RunArgs),
    /// Inspect recorded spans (spec §8.5: `cybersin trace ls|show`).
    Trace {
        #[command(subcommand)]
        command: commands::trace::TraceCommand,
    },
    /// Cost rollups (spec §8.5: `cybersin cost --by <dim>`).
    Cost(commands::cost::CostArgs),
    /// Compile, run, and gate single-prompt output-quality eval suites.
    Eval {
        #[command(subcommand)]
        command: commands::eval::EvalCommand,
    },
    /// Run the daemon. `--server` enables Postgres-backed TCP+mTLS
    /// multi-worker mode.
    Daemon(commands::daemon::DaemonArgs),
    /// Dead-letter queue over the tool-call ledger (spec §8.2: `cybersin
    /// dlq ls|show|retry|drop`).
    Dlq {
        #[command(subcommand)]
        command: commands::dlq::DlqCommand,
    },
    /// Resume a call parked by an approval-gate policy hook (spec §8.2):
    /// resumes the session and runs the call.
    Approve {
        /// Call id, as printed by `cybersin dlq ls`/the parked-call
        /// message (`"{tool}:{idem_key}"`).
        call_id: String,
    },
    /// Resolve a parked call to `failed(reason: "denied")` (spec §8.2)
    /// without killing the session.
    Deny {
        /// Call id, as printed by `cybersin dlq ls`/the parked-call
        /// message (`"{tool}:{idem_key}"`).
        call_id: String,
    },
    /// Inspect and control durable sessions.
    Sessions {
        #[command(subcommand)]
        command: commands::sessions::SessionsCommand,
    },
    /// Deliver a durable steering signal to a session.
    Notify {
        session: String,
        /// JSON payload; use `{"signal":"name",...}` to target a named wait.
        payload: String,
    },
    /// Execute agent-generated code in an isolated container.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Execute a command in a fresh call workspace or persistent session workspace.
    Exec(commands::sandbox::ExecArgs),
    /// Snapshot a persistent session workspace at a checkpoint.
    Snapshot(commands::sandbox::LifecycleArgs),
    /// Show workspace changes relative to a checkpoint snapshot.
    Diff(commands::sandbox::LifecycleArgs),
    /// Restore a persistent session workspace to a checkpoint snapshot.
    Restore(commands::sandbox::LifecycleArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            path,
            profile,
            frozen,
            watch,
        } => {
            if watch {
                from_sync(commands::build::watch_cli(&path, profile, frozen))
            } else {
                from_sync(commands::build::run(&path, profile, frozen))
            }
        }
        Command::Diff {
            reference,
            path,
            profile,
        } => from_sync(commands::diff::run(&path, &reference, profile)),
        Command::Check { path } => from_sync(commands::check::run(&path)),
        Command::Init { dir } => from_sync(commands::init::run(&dir)),
        Command::Fmt { path, check } => from_sync(commands::fmt::run(&path, check)),
        Command::Run(args) => from_async(commands::run::execute(cli.db, args).await),
        Command::Trace { command } => from_async(commands::trace::execute(cli.db, command).await),
        Command::Cost(args) => from_async(commands::cost::execute(cli.db, args).await),
        Command::Eval { command } => from_async(commands::eval::execute(command).await),
        Command::Daemon(args) => from_async(commands::daemon::execute(args).await),
        Command::Dlq { command } => from_async(commands::dlq::execute(cli.db, command).await),
        Command::Approve { call_id } => {
            from_async(commands::approval::approve(cli.db, call_id).await)
        }
        Command::Deny { call_id } => from_async(commands::approval::deny(cli.db, call_id).await),
        Command::Sessions { command } => {
            from_async(commands::sessions::execute(cli.db, command).await)
        }
        Command::Notify { session, payload } => {
            from_async(commands::notify::execute(cli.db, session, payload).await)
        }
        Command::Sandbox { command } => match command {
            SandboxCommand::Exec(args) => from_sync(commands::sandbox::exec(args)),
            SandboxCommand::Snapshot(args) => from_sync(commands::sandbox::snapshot(args)),
            SandboxCommand::Diff(args) => from_sync(commands::sandbox::diff(args)),
            SandboxCommand::Restore(args) => from_sync(commands::sandbox::restore(args)),
        },
    }
}

/// Exit-code mapping for the synchronous compile commands.
fn from_sync(result: Result<Option<String>, String>) -> ExitCode {
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

/// Exit-code mapping for the async runtime commands.
fn from_async(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
