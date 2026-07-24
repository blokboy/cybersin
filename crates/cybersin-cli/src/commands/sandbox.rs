use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args, ValueEnum};
use cybersin_sandbox::{
    DiffKind, DockerBackend, ExecRequest, GvisorBackend, ResourceLimits, SandboxBackend,
    SandboxScope, Termination, WorkspaceStore,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Backend {
    Docker,
    #[value(name = "docker-gvisor")]
    DockerGvisor,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Scope {
    Call,
    Session,
}

impl From<Scope> for SandboxScope {
    fn from(value: Scope) -> Self {
        match value {
            Scope::Call => SandboxScope::Call,
            Scope::Session => SandboxScope::Session,
        }
    }
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[arg(long, value_enum, default_value = "docker-gvisor")]
    backend: Backend,
    #[arg(long)]
    image: String,
    #[arg(long, default_value = ".cybersin/sandbox")]
    root: PathBuf,
    #[arg(long)]
    session: String,
    #[arg(long)]
    call: String,
    #[arg(long, value_enum, default_value = "call")]
    scope: Scope,
    #[arg(long, default_value_t = 1.0)]
    cpus: f64,
    #[arg(long, default_value_t = 512)]
    memory_mb: u64,
    #[arg(long, default_value_t = 128)]
    pids: u32,
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct LifecycleArgs {
    #[arg(long, default_value = ".cybersin/sandbox")]
    root: PathBuf,
    #[arg(long)]
    session: String,
    #[arg(long)]
    checkpoint: String,
}

pub fn exec(args: ExecArgs) -> Result<Option<String>, String> {
    let store = WorkspaceStore::new(&args.root).map_err(|err| err.to_string())?;
    let workspace = store
        .open(args.scope.into(), &args.session, &args.call)
        .map_err(|err| err.to_string())?;
    let binary = std::env::var_os("CYBERSIN_CONTAINER_RUNTIME").unwrap_or_else(|| "docker".into());
    let backend: Box<dyn SandboxBackend> = match args.backend {
        Backend::Docker => Box::new(DockerBackend::with_binary(binary)),
        Backend::DockerGvisor => Box::new(GvisorBackend::with_binary(binary)),
    };
    let outcome = backend
        .exec(ExecRequest {
            image: args.image,
            command: args.command,
            workspace: workspace.path().to_path_buf(),
            scope: args.scope.into(),
            limits: ResourceLimits {
                cpus: args.cpus,
                memory_mb: args.memory_mb,
                pids: args.pids,
                wall_clock: std::time::Duration::from_secs(args.timeout_seconds),
            },
        })
        .map_err(|err| err.to_string())?;

    io::stdout()
        .write_all(outcome.stdout.as_bytes())
        .map_err(|err| err.to_string())?;
    io::stdout().flush().map_err(|err| err.to_string())?;
    if outcome.succeeded() {
        Ok(None)
    } else {
        let reason = match outcome.termination {
            Termination::KilledByLimit(limit) => format!("killed by {limit:?} limit"),
            Termination::Exited => format!("container exited with {:?}", outcome.exit_code),
        };
        Err(if outcome.stderr.is_empty() {
            reason
        } else {
            format!("{reason}: {}", outcome.stderr.trim())
        })
    }
}

pub fn snapshot(args: LifecycleArgs) -> Result<Option<String>, String> {
    session_workspace(&args)?
        .snapshot(&args.checkpoint)
        .map_err(|err| err.to_string())?;
    Ok(None)
}

pub fn diff(args: LifecycleArgs) -> Result<Option<String>, String> {
    let changes = session_workspace(&args)?
        .diff(&args.checkpoint)
        .map_err(|err| err.to_string())?;
    let mut stdout = io::stdout().lock();
    for (path, kind) in changes {
        let marker = match kind {
            DiffKind::Added => 'A',
            DiffKind::Modified => 'M',
            DiffKind::Deleted => 'D',
        };
        writeln!(stdout, "{marker} {}", path.display()).map_err(|err| err.to_string())?;
    }
    stdout.flush().map_err(|err| err.to_string())?;
    Ok(None)
}

pub fn restore(args: LifecycleArgs) -> Result<Option<String>, String> {
    session_workspace(&args)?
        .restore(&args.checkpoint)
        .map_err(|err| err.to_string())?;
    Ok(None)
}

fn session_workspace(args: &LifecycleArgs) -> Result<cybersin_sandbox::Workspace, String> {
    WorkspaceStore::new(&args.root)
        .and_then(|store| store.open(SandboxScope::Session, &args.session, "cli"))
        .map_err(|err| err.to_string())
}
