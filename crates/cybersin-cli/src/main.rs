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

pub mod capabilities;
mod commands;
mod git;
mod harness_config;
mod project;
mod readiness;
mod session_liveness;
mod tool_executor;

use std::path::PathBuf;
use std::process::ExitCode;
use std::{env, io};

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};

use crate::capabilities::{
    apply_safety_gate, registry, CapabilityEvent, ConfirmationAnswer, SafetyGateResult,
};

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
    /// same recorded data. Ignored by compile commands. Defaults to
    /// `<project root>/.cybersin/cybersin.db`, where the project root is
    /// discovered by walking up from the current directory for a
    /// `cybersin.yaml` (issue #50); falls back to plain
    /// `.cybersin/cybersin.db` if none is found.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Compiled project artifact directory used by runtime commands.
    /// Defaults to `<project root>/dist` if it exists; errors clearly if it
    /// doesn't, rather than silently substituting the bundled stub fixture
    /// (issue #50).
    #[arg(long, global = true)]
    dist: Option<PathBuf>,

    /// Root directory for tool execution workspaces and snapshots. Defaults
    /// to `<project root>/.cybersin/sandbox` (issue #50); falls back to
    /// plain `.cybersin/sandbox` if no project root is found.
    #[arg(long, global = true)]
    sandbox_root: Option<PathBuf>,

    /// Container backend used for live tool execution. Defaults to the
    /// discovered `cybersin.yaml`'s `sandbox.backend` field when present
    /// (issue #50), otherwise `docker-gvisor`.
    #[arg(long, global = true, value_enum)]
    sandbox_backend: Option<commands::sandbox::Backend>,

    /// Project directory, or a path inside one, used for project discovery
    /// when the shell's current directory is somewhere else.
    #[arg(long, global = true)]
    project: Option<PathBuf>,

    /// Confirm safety-gated capabilities without an interactive prompt.
    #[arg(long, global = true)]
    yes: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Compile and optimize a project.
    Build {
        /// Project directory containing prompts/ and cybersin.lock.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Build profile. `dev` excludes model-assisted compression.
        #[arg(long, value_enum, default_value = "dev")]
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
    /// Convert a raw natural-language prompt into a buildable prompt source.
    Convert {
        /// Output path. Relative paths resolve from the project root.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Standalone OpenAI model used only for this conversion.
        #[arg(long, default_value = commands::convert::DEFAULT_MODEL)]
        model: String,
        /// `-` for stdin, an existing file path, or literal prompt text.
        input: String,
    },
    /// Scaffold a new project spine (spec §5): core config plus empty
    /// source directories, with an opt-in runnable starter template.
    Init {
        /// Directory to scaffold the project into (created if missing).
        dir: PathBuf,
        /// Overwrite scaffold files that already exist.
        #[arg(long)]
        force: bool,
        /// Report what would be created or skipped without writing files.
        #[arg(long)]
        dry_run: bool,
        /// Optional template content to add after the basic project spine.
        #[arg(long, value_enum, default_value = "basic")]
        template: commands::init::InitTemplate,
        /// After scaffolding, write local readiness defaults and run doctor.
        #[arg(long)]
        setup: bool,
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
    /// Run an agent session (spec §11: `cybersin run <agent.yaml>`) or
    /// infer the single runnable agent target in a built project.
    Run(commands::run::RunArgs),
    /// Inspect recorded spans (spec §8.5: `cybersin trace ls|show`).
    Trace {
        #[command(subcommand)]
        command: commands::trace::TraceCommand,
    },
    /// Report local setup readiness by project area.
    Doctor,
    /// Configure local OpenRouter-first readiness, then run doctor.
    Setup(commands::setup::SetupArgs),
    /// Cost rollups (spec §8.5: `cybersin cost --by <dim>`).
    Cost(commands::cost::CostArgs),
    /// Compile, run, and gate single-prompt output-quality eval suites.
    Eval {
        #[command(subcommand)]
        command: commands::eval::EvalCommand,
    },
    /// Explain a compiled prompt's tokens, routing, cost, and operations state.
    Explain(commands::explain::ExplainArgs),
    /// Live-refreshing Sessions/Traces/Cost control room for a project.
    Ops(commands::ops::OpsArgs),
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
    /// Profile-guided optimization (spec §9): re-derive cache/judge
    /// routing thresholds from observed trace data and emit a normal
    /// build plus `optimize-report.md`.
    Optimize(commands::optimize::OptimizeArgs),
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
    let args = normalize_help_alias(env::args_os());
    let Cli {
        db,
        dist,
        sandbox_root,
        sandbox_backend,
        project,
        yes,
        command,
    } = Cli::parse_from(args);
    let project_start = match resolve_project_start(project) {
        Ok(path) => path,
        Err(err) => return from_async(Err(err)),
    };
    let Some(command) = command else {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            eprintln!(
                "error: bare `cybersin` requires an interactive terminal; use `cybersin -help` or an explicit subcommand for non-interactive use"
            );
            return ExitCode::FAILURE;
        }
        return from_async(commands::tui::execute(project_start).await);
    };
    match command {
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
        Command::Convert { out, model, input } => {
            from_async(commands::convert::execute(&project_start, input, out, model).await)
        }
        Command::Init {
            dir,
            force,
            dry_run,
            template,
            setup,
        } => {
            let init = commands::init::run_with_options(
                &dir,
                commands::init::InitOptions {
                    force,
                    dry_run,
                    template,
                },
            );
            match init {
                Ok(message) => {
                    if let Some(message) = message {
                        println!("{message}");
                    }
                    if setup && !dry_run {
                        from_async(commands::setup::execute(
                            &dir,
                            commands::setup::SetupArgs::default(),
                        ))
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(message) => {
                    eprintln!("{message}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Fmt { path, check } => from_sync(commands::fmt::run(&path, check)),
        Command::Run(args) => {
            let (db, sandbox_root, sandbox_backend, defaults) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            let dist = if args.is_resume() {
                PathBuf::new()
            } else {
                match resolve_dist(dist, &defaults) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                }
            };
            from_async(commands::run::execute(db, dist, sandbox_root, sandbox_backend, args).await)
        }
        Command::Trace { command } => {
            let (db, ..) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            from_async(commands::trace::execute(db, command).await)
        }
        Command::Doctor => from_async(commands::doctor::execute(&project_start)),
        Command::Setup(args) => from_async(commands::setup::execute(&project_start, args)),
        Command::Cost(args) => {
            let (db, ..) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            from_async(commands::cost::execute(db, args).await)
        }
        Command::Eval { command } => from_async(commands::eval::execute(command).await),
        Command::Explain(args) => {
            let (db, ..) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            from_async(commands::explain::execute(db, args).await)
        }
        // Unlike every other runtime command, `ops` resolves `--db` (and,
        // since issue #52's Approvals tab needs a `ToolGateway` to
        // approve/deny in place, `--dist`/`--sandbox-root`/
        // `--sandbox-backend` too) from its own `path` argument rather
        // than always from CWD: its `path` might point at a different
        // project than the one you're standing in, so
        // `resolve_runtime_globals` (which is hardwired to CWD) would
        // resolve the wrong project's settings. All four are passed
        // through unresolved for `ops::execute` to resolve itself against
        // `args.path`.
        Command::Ops(args) => {
            from_async(commands::ops::execute(db, dist, sandbox_root, sandbox_backend, args).await)
        }
        Command::Daemon(args) => {
            if let Err(execution) = ensure_cli_safety("control.daemon.server", yes) {
                return from_blocked_capability(execution);
            }
            from_async(commands::daemon::execute(args).await)
        }
        Command::Dlq { command } => {
            let capability_id = match &command {
                commands::dlq::DlqCommand::Retry { .. } => Some("control.dlq.retry"),
                commands::dlq::DlqCommand::Drop { .. } => Some("control.dlq.drop"),
                commands::dlq::DlqCommand::Ls | commands::dlq::DlqCommand::Show { .. } => None,
            };
            if let Some(capability_id) = capability_id {
                if let Err(execution) = ensure_cli_safety(capability_id, yes) {
                    return from_blocked_capability(execution);
                }
            }
            let (db, sandbox_root, sandbox_backend, defaults) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            let dist = match resolve_dist(dist, &defaults) {
                Ok(v) => v,
                Err(e) => return from_async(Err(e)),
            };
            from_async(
                commands::dlq::execute(db, dist, sandbox_root, sandbox_backend, command).await,
            )
        }
        Command::Approve { call_id } => {
            if let Err(execution) = ensure_cli_safety("control.approve", yes) {
                return from_blocked_capability(execution);
            }
            let (db, sandbox_root, sandbox_backend, defaults) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            let dist = match resolve_dist(dist, &defaults) {
                Ok(v) => v,
                Err(e) => return from_async(Err(e)),
            };
            from_async(
                commands::approval::approve(db, dist, sandbox_root, sandbox_backend, call_id).await,
            )
        }
        Command::Deny { call_id } => {
            if let Err(execution) = ensure_cli_safety("control.deny", yes) {
                return from_blocked_capability(execution);
            }
            let (db, sandbox_root, sandbox_backend, defaults) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            let dist = match resolve_dist(dist, &defaults) {
                Ok(v) => v,
                Err(e) => return from_async(Err(e)),
            };
            from_async(
                commands::approval::deny(db, dist, sandbox_root, sandbox_backend, call_id).await,
            )
        }
        Command::Sessions { command } => {
            let capability_id = match &command {
                commands::sessions::SessionsCommand::Resume { .. } => {
                    Some("control.sessions.resume")
                }
                commands::sessions::SessionsCommand::Kill { .. } => Some("control.sessions.kill"),
                commands::sessions::SessionsCommand::Migrate { .. } => {
                    Some("control.sessions.migrate")
                }
                commands::sessions::SessionsCommand::Ls { .. }
                | commands::sessions::SessionsCommand::Show { .. }
                | commands::sessions::SessionsCommand::Materialize { .. } => None,
            };
            if let Some(capability_id) = capability_id {
                if let Err(execution) = ensure_cli_safety(capability_id, yes) {
                    return from_blocked_capability(execution);
                }
            }
            let (db, ..) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            from_async(commands::sessions::execute(db, command).await)
        }
        Command::Notify { session, payload } => {
            if let Err(execution) = ensure_cli_safety("control.notify", yes) {
                return from_blocked_capability(execution);
            }
            let (db, ..) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            from_async(commands::notify::execute(db, session, payload).await)
        }
        Command::Sandbox { command } => match command {
            SandboxCommand::Exec(args) => {
                if let Err(execution) = ensure_cli_safety("sandbox.exec", yes) {
                    return from_blocked_capability(execution);
                }
                from_sync(commands::sandbox::exec(args))
            }
            SandboxCommand::Snapshot(args) => from_sync(commands::sandbox::snapshot(args)),
            SandboxCommand::Diff(args) => from_sync(commands::sandbox::diff(args)),
            SandboxCommand::Restore(args) => {
                if let Err(execution) = ensure_cli_safety("sandbox.restore", yes) {
                    return from_blocked_capability(execution);
                }
                from_sync(commands::sandbox::restore(args))
            }
        },
        Command::Optimize(args) => {
            let (db, ..) =
                match resolve_runtime_globals(&project_start, db, sandbox_root, sandbox_backend) {
                    Ok(v) => v,
                    Err(e) => return from_async(Err(e)),
                };
            from_async(commands::optimize::execute(db, args).await)
        }
    }
}

fn ensure_cli_safety(
    capability_id: &str,
    auto_confirm: bool,
) -> Result<(), capabilities::CapabilityExecution> {
    let registry = registry();
    let Some(spec) = registry.get(capability_id) else {
        return Ok(());
    };
    let interactive = auto_confirm || (io::stdin().is_terminal() && io::stderr().is_terminal());
    match apply_safety_gate(spec, interactive, |_decision, message| {
        if auto_confirm || confirm_on_stderr(message).unwrap_or(false) {
            ConfirmationAnswer::Accepted
        } else {
            ConfirmationAnswer::Denied
        }
    }) {
        SafetyGateResult::Accepted { .. } => Ok(()),
        SafetyGateResult::Blocked { execution } => Err(execution),
    }
}

fn confirm_on_stderr(message: &str) -> io::Result<bool> {
    eprint!("{message} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn from_blocked_capability(execution: capabilities::CapabilityExecution) -> ExitCode {
    for event in &execution.events {
        match event {
            CapabilityEvent::Started { capability_id } => {
                eprintln!("capability started: {}", capability_id.as_str());
            }
            CapabilityEvent::Prompt { message, .. } => {
                eprintln!("capability prompt: {message}");
            }
            CapabilityEvent::Failed { message } => {
                eprintln!("capability failed: {message}");
            }
            CapabilityEvent::Progress { message, .. } => {
                eprintln!("capability progress: {message}")
            }
            CapabilityEvent::Output { .. } | CapabilityEvent::Completed { .. } => {}
        }
    }
    ExitCode::FAILURE
}

fn normalize_help_alias<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .map(|arg| if arg == "-help" { "--help".into() } else { arg })
        .collect()
}

fn resolve_project_start(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let Some(project) = project else {
        return Ok(cwd);
    };
    let path = if project.is_absolute() {
        project
    } else {
        cwd.join(project)
    };
    if path.is_file() {
        Ok(path.parent().unwrap_or(&path).to_path_buf())
    } else {
        Ok(path)
    }
}

/// Resolves the three infallible runtime globals (`--db`/`--sandbox-root`/
/// `--sandbox-backend`) against the discovered project root (issue #50),
/// applying an explicit flag over its corresponding discovered/config
/// default. Also returns the `ProjectDefaults` so callers that need
/// `--dist` (which can fail, see `resolve_dist`) can resolve it from the
/// same discovery pass.
fn resolve_runtime_globals(
    project_start: &std::path::Path,
    db: Option<PathBuf>,
    sandbox_root: Option<PathBuf>,
    sandbox_backend: Option<commands::sandbox::Backend>,
) -> anyhow::Result<(
    PathBuf,
    PathBuf,
    commands::sandbox::Backend,
    project::ProjectDefaults,
)> {
    let defaults = project::ProjectDefaults::detect(project_start)?;
    load_project_dotenv(project_start)?;
    Ok((
        db.unwrap_or_else(|| defaults.db_default()),
        sandbox_root.unwrap_or_else(|| defaults.sandbox_root_default()),
        sandbox_backend.unwrap_or_else(|| defaults.sandbox_backend_default()),
        defaults,
    ))
}

fn load_project_dotenv(project_start: &std::path::Path) -> anyhow::Result<()> {
    project::ProjectDefaults::detect(project_start)?.load_dotenv()
}

/// Resolves `--dist`: an explicit flag wins, otherwise `<project
/// root>/dist` if it exists, otherwise a clear error (issue #50) — no more
/// silent fallback to the bundled stub fixture.
fn resolve_dist(
    dist: Option<PathBuf>,
    defaults: &project::ProjectDefaults,
) -> anyhow::Result<PathBuf> {
    match dist {
        Some(dist) => Ok(dist),
        None => defaults.dist_default(),
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
            // `:#` prints the whole anyhow context chain ("configuring live
            // model calling: OPENROUTER_API_KEY is not set; ...") — the root
            // cause is usually the only actionable part.
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::CommandFactory;

    use super::*;

    #[test]
    fn capability_catalog_covers_every_cli_operation() {
        let actual_cli_leaves = cli_leaf_commands();
        let expected_cli_leaves = capability_cli_coverage()
            .iter()
            .map(|(command, _)| command.to_string())
            .collect::<BTreeSet<_>>();

        assert_eq!(
            actual_cli_leaves, expected_cli_leaves,
            "update capability_cli_coverage whenever the Clap command tree changes"
        );

        let registry = capabilities::registry();
        for (command, capability_ids) in capability_cli_coverage() {
            assert!(
                !capability_ids.is_empty(),
                "{command} should map to at least one capability"
            );
            for capability_id in *capability_ids {
                let spec = registry
                    .get(capability_id)
                    .unwrap_or_else(|| panic!("{command} maps to missing {capability_id}"));
                assert_eq!(
                    spec.adapters.cli,
                    capabilities::AdapterSupport::Available,
                    "{capability_id} should declare CLI adapter coverage for {command}"
                );
            }
        }

        for capability_id in registry.cli_operations() {
            assert!(
                capability_cli_coverage()
                    .iter()
                    .any(|(_, ids)| ids.contains(&capability_id)),
                "{capability_id} declares CLI support but is absent from capability_cli_coverage"
            );
        }

        for capability_id in [
            "compile.build.watch",
            "compile.fmt.check",
            "runtime.run.stub",
            "runtime.run.agent",
        ] {
            assert!(
                registry.get(capability_id).is_some(),
                "flag-specific capability {capability_id} should remain explicit"
            );
        }

        let scaffold = registry
            .get("workflow.scaffold-build")
            .expect("TUI-only scaffold/build workflow should be cataloged");
        assert!(matches!(
            scaffold.adapters.cli,
            capabilities::AdapterSupport::Unavailable { .. }
        ));
    }

    fn cli_leaf_commands() -> BTreeSet<String> {
        let root = Cli::command();
        let mut leaves = BTreeSet::new();
        collect_leaf_commands(&root, Vec::new(), &mut leaves);
        leaves
    }

    fn collect_leaf_commands(
        command: &clap::Command,
        path: Vec<String>,
        leaves: &mut BTreeSet<String>,
    ) {
        let subcommands = command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .collect::<Vec<_>>();
        if subcommands.is_empty() {
            if !path.is_empty() {
                leaves.insert(path.join(" "));
            }
            return;
        }

        for subcommand in subcommands {
            let mut subcommand_path = path.clone();
            subcommand_path.push(subcommand.get_name().to_string());
            collect_leaf_commands(subcommand, subcommand_path, leaves);
        }
    }

    fn capability_cli_coverage() -> &'static [(&'static str, &'static [&'static str])] {
        &[
            ("approve", &["control.approve"]),
            ("build", &["compile.build", "compile.build.watch"]),
            ("check", &["compile.check"]),
            ("convert", &["compile.convert"]),
            ("cost", &["inspection.cost"]),
            ("daemon", &["control.daemon.server"]),
            ("deny", &["control.deny"]),
            ("diff", &["compile.diff"]),
            ("doctor", &["inspection.doctor"]),
            ("setup", &["workflow.setup"]),
            ("dlq drop", &["control.dlq.drop"]),
            ("dlq ls", &["control.dlq.ls"]),
            ("dlq retry", &["control.dlq.retry"]),
            ("dlq show", &["control.dlq.show"]),
            ("eval gate", &["workflow.eval.gate"]),
            ("eval run", &["workflow.eval.run"]),
            ("explain", &["inspection.explain"]),
            ("fmt", &["compile.fmt", "compile.fmt.check"]),
            ("init", &["workflow.init"]),
            ("notify", &["control.notify"]),
            ("ops", &["control.ops"]),
            ("optimize", &["workflow.optimize"]),
            ("run", &["runtime.run.stub", "runtime.run.agent"]),
            ("sandbox diff", &["sandbox.diff"]),
            ("sandbox exec", &["sandbox.exec"]),
            ("sandbox restore", &["sandbox.restore"]),
            ("sandbox snapshot", &["sandbox.snapshot"]),
            ("sessions kill", &["control.sessions.kill"]),
            ("sessions ls", &["control.sessions.ls"]),
            ("sessions materialize", &["control.sessions.materialize"]),
            ("sessions migrate", &["control.sessions.migrate"]),
            ("sessions resume", &["control.sessions.resume"]),
            ("sessions show", &["control.sessions.show"]),
            ("trace ls", &["inspection.trace.ls"]),
            ("trace sample", &["inspection.trace.sample"]),
            ("trace show", &["inspection.trace.show"]),
        ]
    }
}
