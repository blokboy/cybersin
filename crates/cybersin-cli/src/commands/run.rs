//! `cybersin run` (spec §11: `cybersin run <agent.yaml> [--input f]`).
//!
//! Two paths: `--stub` drives the M1 stub agent against a hand-written
//! `dist/` fixture (spec §14's M1 exit criterion); `<agent.yaml>` (issue
//! #35 Phase 3) spawns the declared `harness: { adapter, command: [...] }`
//! process and drives a real `RuntimeDaemon` session against it, with live
//! OpenRouter model calling (Phase 1), sandboxed tool execution (Phase 2),
//! and gateway-backed ledger/retry semantics for ungated tool calls (issue
//! #37) all wired in. `harness.adapter` selects the transport (spec §10):
//! `process` speaks newline-JSON over the spawned process's own
//! stdin/stdout; `grpc` spawns the process with its connect address in
//! `CYBERSIN_ADAPTER_ADDR` and accepts its `Session` RPC instead.

use std::env;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Args;
use cybersin_adapter::channel::DaemonChannel;
use cybersin_adapter::messages::CallOutcome;
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::grpc;
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_adapter::transport::stdio::StdioDaemonChannel;
use cybersin_router::{ModelKind, RouteDecision};
use cybersin_runtime::{
    heartbeat_liveness_at, is_terminal_session_status, materialize_artifact_bundle, stub_agent,
    ArtifactIngestOutcome, DaemonHandle, DistFixture, HeartbeatLiveness, LocalConfigFile,
    ModelAllowlist, OpenRouterModelCaller, RuntimeSessionSummary, SessionSupervisor, Storage,
    DEFAULT_HEARTBEAT_STALE_AFTER_MS,
};
use cybersin_sandbox::WorkspaceStore;
use tokio::process::{Child, Command};

use crate::commands::build::discover_agent_sources;
use crate::harness_config::AgentMeta;
use crate::readiness;
use crate::session_liveness::now_unix_ms as session_now_unix_ms;
use crate::tool_executor::{self, GatewayToolCaller};

/// How long `cybersin run`'s `harness.adapter: grpc` path waits for the
/// spawned harness process to open its `Session` RPC before giving up —
/// bounded so a harness that hangs or never speaks gRPC can't block
/// forever. Internal plumbing, not exposed as a CLI flag (matches this
/// codebase's existing `CYBERSIN_CONTAINER_RUNTIME`-style scope
/// discipline for knobs nothing has asked to configure yet).
const GRPC_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

enum RunTarget {
    Agent(PathBuf),
    BuiltInStarter { prompt_name: String },
}

#[derive(Debug, Clone)]
struct LiveRunSpec {
    dist_dir: PathBuf,
    project_dir: PathBuf,
    meta: AgentMeta,
    agent_name: String,
    inputs: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeLeaseOutcome {
    Stale,
    Forced,
    Failed,
    NotRunning,
}

impl ResumeLeaseOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::Forced => "forced",
            Self::Failed => "failed",
            Self::NotRunning => "not_running",
        }
    }
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to an `*.agent.yaml` (spec §11).
    #[arg(conflicts_with = "stub")]
    pub agent_yaml: Option<PathBuf>,

    /// Run the M1 stub agent end-to-end against a hand-written `dist/`
    /// fixture instead of a compiled agent (spec §14's M1 exit criterion:
    /// "stub agent runs on a hand-written dist/").
    #[arg(long)]
    pub stub: bool,

    /// Session id to record this run under. Defaults to a fresh
    /// timestamp-based id so repeated runs don't collide in the trace
    /// store.
    #[arg(long, conflicts_with = "resume")]
    pub session_id: Option<String>,

    /// Relaunch an existing session from its latest checkpoint.
    #[arg(
        long,
        value_name = "SESSION_ID",
        conflicts_with_all = ["agent_yaml", "stub", "session_id", "agent", "input"]
    )]
    pub resume: Option<String>,

    /// Override a fresh heartbeat lease when used with `--resume`.
    #[arg(long, requires = "resume")]
    pub force: bool,

    /// Agent name spans/sessions are attributed to (the `agent` dimension
    /// of `cybersin cost --by agent`). Defaults to `agent.yaml`'s `name:`
    /// for a real run, `"research-agent"` for `--stub`.
    #[arg(long)]
    pub agent: Option<String>,

    /// JSON file of session inputs (spec §11's `[--input f]`). Real runs
    /// only — `--stub` uses its own fixed inputs. Defaults to `{}`.
    #[arg(long)]
    pub input: Option<PathBuf>,
}

impl RunArgs {
    pub(crate) fn is_resume(&self) -> bool {
        self.resume.is_some()
    }
}

pub async fn execute(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    args: RunArgs,
) -> anyhow::Result<()> {
    let summary = run_session(db_path, dist_dir, sandbox_root, sandbox_backend, args).await?;
    print_summary(&summary);
    Ok(())
}

pub async fn run_session(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    args: RunArgs,
) -> anyhow::Result<RuntimeSessionSummary> {
    if let Some(session_id) = args.resume.clone() {
        return run_resume(
            db_path,
            sandbox_root,
            sandbox_backend,
            session_id,
            args.force,
        )
        .await;
    }
    match (args.stub, args.agent_yaml.clone()) {
        (true, _) => run_stub(db_path, dist_dir, args).await,
        (false, explicit_agent_yaml) => {
            let target = match explicit_agent_yaml {
                Some(agent_yaml) => RunTarget::Agent(agent_yaml),
                None => infer_run_target(&dist_dir)?,
            };
            match target {
                RunTarget::Agent(agent_yaml) => {
                    run_live(
                        db_path,
                        dist_dir,
                        sandbox_root,
                        sandbox_backend,
                        agent_yaml,
                        args,
                    )
                    .await
                }
                RunTarget::BuiltInStarter { prompt_name } => {
                    run_builtin_starter(
                        db_path,
                        dist_dir,
                        sandbox_root,
                        sandbox_backend,
                        prompt_name,
                        args,
                    )
                    .await
                }
            }
        }
    }
}

#[cfg(test)]
fn infer_agent_yaml(dist_dir: &Path) -> anyhow::Result<PathBuf> {
    match infer_run_target(dist_dir)? {
        RunTarget::Agent(agent_yaml) => Ok(agent_yaml),
        RunTarget::BuiltInStarter { .. } => anyhow::bail!(
            "no runnable agent targets found in {}; create an agents/*.agent.yaml file or run `cybersin run <agent.yaml>` explicitly",
            project_dir_from_dist(dist_dir).join("agents").display()
        ),
    }
}

fn infer_run_target(dist_dir: &Path) -> anyhow::Result<RunTarget> {
    let project_dir = project_dir_from_dist(dist_dir);
    let candidates = runnable_agent_targets(project_dir)?;
    match candidates.as_slice() {
        [] => infer_builtin_starter_target(project_dir, dist_dir),
        [single] => Ok(RunTarget::Agent(single.clone())),
        multiple => {
            let choices = multiple
                .iter()
                .map(|path| {
                    format!(
                        "  - cybersin run {}",
                        display_project_path(project_dir, path)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "multiple runnable agent targets found; choose one explicitly:\n{choices}"
            )
        }
    }
}

fn infer_builtin_starter_target(project_dir: &Path, dist_dir: &Path) -> anyhow::Result<RunTarget> {
    let no_agent_message = || {
        anyhow::anyhow!(
            "no runnable agent targets found in {}; create an agents/*.agent.yaml file or run `cybersin run <agent.yaml>` explicitly",
            project_dir.join("agents").display()
        )
    };
    if !project_dir.join("cybersin.yaml").is_file() {
        return Err(no_agent_message());
    }

    let dist = DistFixture::load_dir(dist_dir)
        .with_context(|| format!("loading built dist from {}", dist_dir.display()))?;
    match dist.prompts.keys().cloned().collect::<Vec<_>>().as_slice() {
        [prompt_name] => Ok(RunTarget::BuiltInStarter {
            prompt_name: prompt_name.clone(),
        }),
        [] => Err(no_agent_message()),
        prompts => anyhow::bail!(
            "no runnable agent targets found and built-in starter requires exactly one compiled prompt; found {} prompts in {}: {}",
            prompts.len(),
            dist_dir.join("prompts").display(),
            prompts.join(", ")
        ),
    }
}

fn runnable_agent_targets(project_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let candidates = discover_agent_sources(project_dir).map_err(anyhow::Error::msg)?;
    let mut runnable = Vec::new();
    for candidate in candidates {
        let yaml_source = std::fs::read_to_string(&candidate)
            .with_context(|| format!("reading {}", candidate.display()))?;
        let meta = AgentMeta::from_agent_yaml(&yaml_source)
            .with_context(|| format!("parsing {}", candidate.display()))?;
        if inferred_harness_is_launchable(project_dir, &meta) {
            runnable.push(candidate);
        }
    }
    Ok(runnable)
}

fn inferred_harness_is_launchable(project_dir: &Path, meta: &AgentMeta) -> bool {
    command_program_is_launchable(project_dir, &meta.harness.command[0])
        && command_local_file_args_exist(project_dir, &meta.harness.command[1..])
}

fn command_program_is_launchable(project_dir: &Path, program: &str) -> bool {
    let path = Path::new(program);
    if path.is_absolute() {
        return path.exists();
    }
    if program.contains(std::path::MAIN_SEPARATOR) {
        return project_dir.join(path).exists();
    }
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

fn command_local_file_args_exist(project_dir: &Path, args: &[String]) -> bool {
    args.iter().all(|arg| {
        let path = Path::new(arg);
        if path.is_absolute() || arg.contains(std::path::MAIN_SEPARATOR) || looks_like_script(arg) {
            path.exists() || project_dir.join(path).exists()
        } else {
            true
        }
    })
}

fn looks_like_script(arg: &str) -> bool {
    matches!(
        Path::new(arg).extension().and_then(OsStr::to_str),
        Some("py" | "js" | "mjs" | "cjs" | "sh" | "bash" | "zsh")
    )
}

fn project_dir_from_dist(dist_dir: &Path) -> &Path {
    dist_dir.parent().unwrap_or_else(|| Path::new("."))
}

fn display_project_path(project_dir: &Path, path: &Path) -> String {
    path.strip_prefix(project_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

async fn run_stub(
    db_path: PathBuf,
    dist_dir: PathBuf,
    args: RunArgs,
) -> anyhow::Result<RuntimeSessionSummary> {
    let dist = Arc::new(DistFixture::load_dir(&dist_dir)?);

    // `cybersind` auto-starts here: this is the first point a runtime
    // command needs the daemon, and DaemonHandle::auto_start transparently
    // opens (and, on first run, migrates) the SQLite state file at
    // `db_path` — see cybersin_runtime::daemon's doc comment for why this
    // is in-process rather than a real subprocess for M1.
    println!("cybersind: auto-starting (state: {})", db_path.display());
    let daemon = DaemonHandle::auto_start(&db_path).await?;

    let session_id = args
        .session_id
        .unwrap_or_else(|| format!("sess-{}", now_unix_ms()));
    let agent_name = args.agent.unwrap_or_else(|| "research-agent".to_string());
    ensure_fresh_session_id(daemon.storage().as_ref(), &session_id).await?;

    println!(
        "running stub agent: session={session_id} agent={agent_name} dist={}",
        dist_dir.display()
    );
    ingest_dist_for_session(daemon.storage().as_ref(), &dist_dir, &session_id).await?;

    stub_agent::run_stub_session(
        daemon.storage(),
        daemon.spans(),
        dist,
        session_id.clone(),
        agent_name,
    )
    .await
    .map_err(Into::into)
}

fn live_run_spec_from_agent(
    dist_dir: &Path,
    agent_yaml: &Path,
    args: &RunArgs,
) -> anyhow::Result<LiveRunSpec> {
    let yaml_source = std::fs::read_to_string(agent_yaml)
        .with_context(|| format!("reading {}", agent_yaml.display()))?;
    let meta = AgentMeta::from_agent_yaml(&yaml_source)
        .with_context(|| format!("parsing {}", agent_yaml.display()))?;
    let agent_name = args.agent.clone().unwrap_or_else(|| meta.name.clone());
    let inputs = match &args.input {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        None => serde_json::json!({}),
    };
    Ok(LiveRunSpec {
        dist_dir: dist_dir.to_path_buf(),
        project_dir: project_dir_from_dist(dist_dir).to_path_buf(),
        meta,
        agent_name,
        inputs,
    })
}

async fn record_live_run_spec(
    storage: &dyn Storage,
    session_id: &str,
    spec: &LiveRunSpec,
) -> anyhow::Result<()> {
    storage
        .append_event(
            session_id,
            "run.harness",
            serde_json::json!({
                "agent_name": spec.agent_name,
                "inputs": spec.inputs,
                "harness": {
                    "adapter": spec.meta.harness.adapter,
                    "command": spec.meta.harness.command,
                }
            }),
        )
        .await?;
    Ok(())
}

async fn load_live_run_spec(
    storage: &dyn Storage,
    session_id: &str,
) -> anyhow::Result<Option<LiveRunSpec>> {
    let Some(event) = storage
        .load_events(session_id)
        .await?
        .into_iter()
        .rev()
        .find(|event| event.kind == "run.harness")
    else {
        return Ok(None);
    };
    let harness = &event.payload["harness"];
    let adapter = harness["adapter"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("run.harness event is missing harness.adapter"))?
        .to_string();
    let command = harness["command"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("run.harness event is missing harness.command"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("run.harness command contains a non-string value"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let agent_name = event.payload["agent_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("run.harness event is missing agent_name"))?
        .to_string();
    Ok(Some(LiveRunSpec {
        dist_dir: PathBuf::new(),
        project_dir: PathBuf::new(),
        meta: AgentMeta {
            name: agent_name.clone(),
            harness: crate::harness_config::HarnessConfig { adapter, command },
        },
        agent_name,
        inputs: event
            .payload
            .get("inputs")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    }))
}

fn resume_gate(
    session: &cybersin_runtime::SessionRecord,
    force: bool,
) -> anyhow::Result<(ResumeLeaseOutcome, Option<String>)> {
    let holder = session.heartbeat_holder.clone();
    match session.status.as_str() {
        "failed" | "aborted" => Ok((ResumeLeaseOutcome::Failed, holder)),
        "completed" => anyhow::bail!(
            "session {:?} is completed; start a new run instead",
            session.session_id
        ),
        "running" => {
            let liveness = heartbeat_liveness_at(
                session,
                session_now_unix_ms(),
                DEFAULT_HEARTBEAT_STALE_AFTER_MS,
            );
            match (force, liveness) {
                (true, _) => Ok((ResumeLeaseOutcome::Forced, holder)),
                (false, HeartbeatLiveness::Fresh) => anyhow::bail!(
                    "session {:?} is still running under a fresh lease held by {}; pass --force to override",
                    session.session_id,
                    holder.as_deref().unwrap_or("unknown holder")
                ),
                (false, _) => Ok((ResumeLeaseOutcome::Stale, holder)),
            }
        }
        status if is_terminal_session_status(status) => anyhow::bail!(
            "session {:?} has terminal status {status:?}; start a new run instead",
            session.session_id
        ),
        _ => Ok((ResumeLeaseOutcome::NotRunning, holder)),
    }
}

async fn prepare_live_daemon(
    daemon: DaemonHandle,
    spec: LiveRunSpec,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    session_id: String,
) -> anyhow::Result<(
    cybersin_runtime::RuntimeDaemon<Box<dyn DaemonChannel>>,
    Child,
)> {
    let dist = Arc::new(DistFixture::load_dir(&spec.dist_dir)?);
    let local_config = LocalConfigFile::load_optional(&spec.project_dir).with_context(|| {
        format!(
            "reading {}",
            spec.project_dir.join("cybersin.local.yaml").display()
        )
    })?;
    let model_caller = openrouter_from_local_config(dist.clone(), local_config.as_ref())
        .context("configuring live model calling")?;
    let allowlist = local_config
        .as_ref()
        .map(LocalConfigFile::model_allowlist)
        .unwrap_or_else(ModelAllowlist::allow_all);
    let retry_policy = local_config
        .as_ref()
        .map(LocalConfigFile::retry_policy)
        .unwrap_or_default();

    let executor = tool_executor::configured_executor_with_local_config(
        &spec.dist_dir,
        &sandbox_root,
        sandbox_backend,
        local_config.as_ref(),
    )
    .context("configuring live tool execution")?;
    let session_sandbox = WorkspaceStore::new(&sandbox_root)?;
    let tool_caller = GatewayToolCaller::new(executor, daemon.storage(), dist.clone());
    let (child, channel) = spawn_harness(&spec.meta.harness, &spec.project_dir).await?;
    let runtime_daemon = cybersin_runtime::RuntimeDaemon::new(
        channel,
        daemon.storage(),
        daemon.spans(),
        dist,
        session_id,
        spec.agent_name,
    )
    .with_models(model_caller, allowlist)
    .with_retry_policy(retry_policy)
    .with_session_sandbox(session_sandbox)
    .with_tool_caller(tool_caller);
    Ok((runtime_daemon, child))
}

async fn run_resume(
    db_path: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    session_id: String,
    force: bool,
) -> anyhow::Result<RuntimeSessionSummary> {
    println!("cybersind: auto-starting (state: {})", db_path.display());
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let storage = daemon.storage();
    let session = storage
        .get_session(&session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {session_id:?} not found"))?;
    let (lease_outcome, holder) = resume_gate(&session, force)?;
    if session.config_hash.is_empty() {
        anyhow::bail!(
            "session {session_id:?} has no pinned config_hash; run `cybersin sessions migrate` after re-ingesting the bundle"
        );
    }
    if !storage.has_artifact_bundle(&session.config_hash).await? {
        anyhow::bail!(
            "artifact bundle for pinned config_hash {:?} is not stored; run `cybersin sessions migrate` or re-ingest the bundle",
            session.config_hash
        );
    }
    let checkpoint = storage
        .latest_checkpoint(&session_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("session {session_id:?} has no checkpoint to resume from")
        })?;
    let stored_spec = load_live_run_spec(storage.as_ref(), &session_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "session {session_id:?} is missing run harness metadata; start a new run or re-ingest with a current cybersin"
            )
        })?;

    let scratch = tempfile::Builder::new()
        .prefix("cybersin-resume-")
        .tempdir()?;
    let scratch_dist = scratch.path().join("dist");
    materialize_artifact_bundle(storage.as_ref(), &session.config_hash, &scratch_dist).await?;
    let spec = LiveRunSpec {
        dist_dir: scratch_dist,
        project_dir: scratch.path().to_path_buf(),
        meta: stored_spec.meta,
        agent_name: stored_spec.agent_name,
        inputs: stored_spec.inputs,
    };

    let workspaces = WorkspaceStore::new(&sandbox_root)?;
    let resume_state = SessionSupervisor::with_session_sandbox(storage.clone(), workspaces)
        .resume_with_payload(
            &session_id,
            &session.config_hash,
            serde_json::json!({
                "resume_kind": "run_relaunch",
                "lease_outcome": lease_outcome.as_str(),
                "heartbeat_holder": holder,
                "restored_checkpoint_id": checkpoint.checkpoint_id,
            }),
        )
        .await?;

    println!(
        "resuming harness: session={session_id} checkpoint={} lease={} holder={} command={:?} (adapter={})",
        checkpoint.checkpoint_id,
        lease_outcome.as_str(),
        holder.as_deref().unwrap_or("-"),
        spec.meta.harness.command,
        spec.meta.harness.adapter
    );

    let (mut runtime_daemon, mut child) = prepare_live_daemon(
        daemon,
        spec.clone(),
        sandbox_root,
        sandbox_backend,
        session_id,
    )
    .await?;
    if let Err(err) = runtime_daemon
        .start_resumed_session(spec.inputs.clone(), resume_state)
        .await
    {
        return Err(harness_crash_or(&mut child, err.into()).await);
    }
    drive_live_daemon(runtime_daemon, &mut child).await
}

async fn run_live(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    agent_yaml: PathBuf,
    args: RunArgs,
) -> anyhow::Result<RuntimeSessionSummary> {
    println!("cybersind: auto-starting (state: {})", db_path.display());
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let session_id = args
        .session_id
        .clone()
        .unwrap_or_else(|| format!("sess-{}", now_unix_ms()));
    ensure_fresh_session_id(daemon.storage().as_ref(), &session_id).await?;

    let spec = live_run_spec_from_agent(&dist_dir, &agent_yaml, &args)?;
    ingest_dist_for_session(daemon.storage().as_ref(), &dist_dir, &session_id).await?;
    record_live_run_spec(daemon.storage().as_ref(), &session_id, &spec).await?;

    println!(
        "spawning harness: session={session_id} agent={} command={:?} (adapter={})",
        spec.agent_name, spec.meta.harness.command, spec.meta.harness.adapter
    );

    let (mut runtime_daemon, mut child) = prepare_live_daemon(
        daemon,
        spec.clone(),
        sandbox_root,
        sandbox_backend,
        session_id.clone(),
    )
    .await?;
    // A harness that crashes immediately closes its stdin/stdout out from
    // under the channel before we've necessarily observed its exit status,
    // so a send/recv here can fail with a raw transport error (e.g. a
    // broken pipe) instead of the more useful "the process died" signal.
    // Any time a channel operation on this session errors, prefer the
    // child's actual exit status over the raw transport error.
    if let Err(err) = runtime_daemon.start_session(spec.inputs.clone()).await {
        return Err(harness_crash_or(&mut child, err.into()).await);
    }
    drive_live_daemon(runtime_daemon, &mut child).await
}

async fn drive_live_daemon(
    runtime_daemon: cybersin_runtime::RuntimeDaemon<Box<dyn DaemonChannel>>,
    child: &mut Child,
) -> anyhow::Result<RuntimeSessionSummary> {
    // `runtime_daemon.run()` is driven inline via `select!`, not
    // `tokio::spawn`ed onto its own task: `GrpcDaemonChannel` (tonic's
    // `Streaming<T>` inside it) is `Send` but not `Sync`, and
    // `tokio::spawn`'s `Send`-future requirement — which flows through
    // `&self`/`&mut self` held across this method's await points — needs
    // `Sync` too. Polling both futures together in this same task via
    // `select!` needs neither: losing branch is dropped automatically,
    // taking the place of the old `daemon_task.abort()`.
    //
    // `run(self)` takes ownership, so its future is created once and
    // pinned here rather than re-invoked per `select!` iteration — a
    // clean-exit `child.wait()` win (below) needs to keep polling this
    // exact future afterward, which `runtime_daemon.run()` a second time
    // couldn't do (`runtime_daemon` is moved into it the first time).
    let daemon_fut = async move { runtime_daemon.run().await.map_err(anyhow::Error::from) };
    tokio::pin!(daemon_fut);

    let summary = tokio::select! {
        result = &mut daemon_fut => {
            // The daemon loop ended on its own (SessionComplete or a
            // closed channel) — reap the child now that its stdin (owned
            // by the dropped RuntimeDaemon's channel) has closed; a
            // well-behaved harness exits promptly once it sees EOF.
            match result {
                Ok(summary) => {
                    let _ = child.wait().await;
                    summary
                }
                Err(err) => return Err(harness_crash_or(child, err.into()).await),
            }
        }
        status = child.wait() => {
            let status = status.context("waiting on harness process")?;
            if !status.success() {
                anyhow::bail!(
                    "harness process exited unexpectedly ({}) before completing the session",
                    status
                        .code()
                        .map(|code| format!("code {code}"))
                        .unwrap_or_else(|| "killed by signal".to_string())
                );
            }
            // A clean exit winning this race only means this branch got
            // polled before `daemon_fut` was polled again — the harness
            // closes its pipe only *after* writing `session.complete`, so
            // that message is already sitting in the channel's read
            // buffer, not lost. Await the same daemon future through to
            // its own completion instead of treating an ordinary,
            // successful exit as a crash: first surfaced live as a fully
            // successful scripted run (every step, including a
            // Docker-sandboxed approval, resolved correctly) that still
            // reported "exited unexpectedly (code 0)" purely because the
            // OS happened to reap the harness before this task's next
            // poll of the daemon loop.
            match daemon_fut.await {
                Ok(summary) => summary,
                Err(err) => return Err(err.into()),
            }
        }
    };

    Ok(summary)
}

async fn run_builtin_starter(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    prompt_name: String,
    args: RunArgs,
) -> anyhow::Result<RuntimeSessionSummary> {
    let mut dist_fixture = DistFixture::load_dir(&dist_dir)?;
    let agent_name = args
        .agent
        .clone()
        .unwrap_or_else(|| format!("{prompt_name}-starter"));
    let inputs = match &args.input {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        None => serde_json::json!({}),
    };

    println!("cybersind: auto-starting (state: {})", db_path.display());
    let daemon = DaemonHandle::auto_start(&db_path).await?;

    let project_dir = project_dir_from_dist(&dist_dir);
    let local_config = LocalConfigFile::load_optional(project_dir).with_context(|| {
        format!(
            "reading {}",
            project_dir.join("cybersin.local.yaml").display()
        )
    })?;
    retarget_scaffolded_stub_routes(&mut dist_fixture, local_config.as_ref());
    let dist = Arc::new(dist_fixture);
    let model_caller = openrouter_from_local_config(dist.clone(), local_config.as_ref())
        .context("configuring live model calling")?;
    let allowlist = local_config
        .as_ref()
        .map(LocalConfigFile::model_allowlist)
        .unwrap_or_else(ModelAllowlist::allow_all);
    let retry_policy = local_config
        .as_ref()
        .map(LocalConfigFile::retry_policy)
        .unwrap_or_default();
    let executor = tool_executor::configured_executor_with_local_config(
        &dist_dir,
        &sandbox_root,
        sandbox_backend,
        local_config.as_ref(),
    )
    .context("configuring live tool execution")?;
    let tool_caller = GatewayToolCaller::new(executor, daemon.storage(), dist.clone());

    let session_id = args
        .session_id
        .unwrap_or_else(|| format!("sess-{}", now_unix_ms()));
    ensure_fresh_session_id(daemon.storage().as_ref(), &session_id).await?;
    ingest_dist_for_session(daemon.storage().as_ref(), &dist_dir, &session_id).await?;

    println!(
        "running built-in starter harness: session={session_id} agent={agent_name} prompt={prompt_name}"
    );

    let (harness_io, daemon_io) = in_memory_pair();
    let mut runtime_daemon = cybersin_runtime::RuntimeDaemon::new(
        daemon_io,
        daemon.storage(),
        daemon.spans(),
        dist,
        session_id.clone(),
        agent_name,
    )
    .with_models(model_caller, allowlist)
    .with_retry_policy(retry_policy)
    .with_tool_caller(tool_caller);

    runtime_daemon.start_session(inputs.clone()).await?;

    let daemon_fut = async move { runtime_daemon.run().await.map_err(anyhow::Error::from) };
    let starter_fut = run_starter_harness(harness_io, session_id, prompt_name, inputs);
    let (summary, ()) = tokio::try_join!(daemon_fut, starter_fut)?;
    Ok(summary)
}

async fn ensure_fresh_session_id(storage: &dyn Storage, session_id: &str) -> anyhow::Result<()> {
    if let Some(existing) = storage.get_session(session_id).await? {
        anyhow::bail!(
            "session id {session_id:?} already exists with status {:?}; use `cybersin run --resume {session_id}` or pass a fresh --session-id",
            existing.status
        );
    }
    Ok(())
}

async fn ingest_dist_for_session(
    storage: &dyn Storage,
    dist_dir: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    let bundle = DistFixture::load_artifact_bundle(dist_dir)
        .with_context(|| format!("loading artifact bundle from {}", dist_dir.display()))?;
    let config_hash = bundle.config_hash.clone();
    let file_count = bundle.files.len();
    let outcome = storage.ingest_artifact_bundle(&bundle).await?;
    let outcome_text = match outcome {
        ArtifactIngestOutcome::Stored => "stored",
        ArtifactIngestOutcome::Reused => "reused",
    };
    storage
        .append_event(
            session_id,
            "artifact.bundle",
            serde_json::json!({
                "config_hash": config_hash,
                "outcome": outcome_text,
                "file_count": file_count,
            }),
        )
        .await?;
    Ok(())
}

fn retarget_scaffolded_stub_routes(dist: &mut DistFixture, config: Option<&LocalConfigFile>) {
    let Some(config) = config else {
        return;
    };
    let (Some(provider), Some(model_name)) = (&config.defaults.provider, &config.defaults.model)
    else {
        return;
    };
    if provider == "stub" || model_name == "stub-medium" {
        return;
    }

    for route in dist.routing_artifact.prompts.values_mut() {
        for decision in &mut route.decisions {
            match decision {
                RouteDecision::Cache(cache) => {
                    retarget_model(&mut cache.judge, provider, model_name);
                }
                RouteDecision::Cascade(cascade) => {
                    for step in &mut cascade.steps {
                        retarget_model(&mut step.model, provider, model_name);
                    }
                }
                RouteDecision::Fallbacks(fallbacks) => {
                    for model in &mut fallbacks.providers {
                        retarget_model(model, provider, model_name);
                    }
                }
            }
        }
    }
}

fn retarget_model(model: &mut cybersin_router::RouteModel, provider: &str, model_name: &str) {
    if model.provider == "stub" && model.name == "stub-medium" {
        model.provider = provider.to_string();
        model.name = model_name.to_string();
        model.model_kind = ModelKind::Provider;
    }
}

async fn run_starter_harness<C>(
    harness_io: C,
    session_id: String,
    prompt_name: String,
    inputs: serde_json::Value,
) -> anyhow::Result<()>
where
    C: cybersin_adapter::channel::HarnessChannel,
{
    let mut harness = StubHarness::new(harness_io);
    let (started_session, _, _) = harness.recv_session_start().await;
    if started_session != session_id {
        anyhow::bail!(
            "built-in starter harness received session {started_session:?}, expected {session_id:?}"
        );
    }

    let (_, outcome) = harness.llm_request(prompt_name.clone(), inputs).await;
    let result = match outcome {
        CallOutcomeOrPark::Result(CallOutcome::Ok { value }) => value,
        CallOutcomeOrPark::Result(CallOutcome::Failed { reason, .. }) => {
            anyhow::bail!("built-in starter harness prompt {prompt_name:?} failed: {reason}")
        }
        CallOutcomeOrPark::Parked(approval_id) => {
            anyhow::bail!(
                "built-in starter harness prompt {prompt_name:?} parked unexpectedly for approval {approval_id}"
            )
        }
        CallOutcomeOrPark::Aborted(reason) => {
            anyhow::bail!("built-in starter harness session aborted: {reason:?}")
        }
    };
    harness
        .session_complete(
            session_id,
            serde_json::json!({ "prompt": prompt_name, "result": result }),
        )
        .await;
    harness.wait_for_close().await;
    Ok(())
}

fn openrouter_from_local_config(
    dist: Arc<DistFixture>,
    config: Option<&LocalConfigFile>,
) -> Result<OpenRouterModelCaller, cybersin_runtime::MissingApiKey> {
    let caller = match readiness::resolve_openrouter_api_key(config) {
        Some(api_key) => OpenRouterModelCaller::new(dist, api_key),
        None => OpenRouterModelCaller::from_env(dist)?,
    };
    Ok(match readiness::openrouter_base_url(config) {
        Some(base_url) => caller.with_base_url(base_url),
        None => caller,
    })
}

/// Spawns `harness.command` and returns its process handle plus a
/// connected channel, wired up per `harness.adapter` (spec §10):
/// `"process"` pipes the child's own stdin/stdout for the newline-JSON
/// protocol; `"grpc"` starts a local gRPC listener, tells the child where
/// to connect via `CYBERSIN_ADAPTER_ADDR`, and accepts its `Session` RPC
/// — racing that accept against the child exiting early, so a harness
/// that crashes before ever connecting produces a clear "exited
/// unexpectedly" error instead of hanging until `GRPC_ACCEPT_TIMEOUT`.
/// `harness_config::AgentMeta::from_agent_yaml` already restricts
/// `adapter` to these two values, so anything else is unreachable here.
async fn spawn_harness(
    harness: &crate::harness_config::HarnessConfig,
    project_dir: &Path,
) -> anyhow::Result<(Child, Box<dyn DaemonChannel>)> {
    match harness.adapter.as_str() {
        "grpc" => {
            let mut server = grpc::listen("127.0.0.1:0")
                .await
                .context("starting the gRPC adapter listener")?;
            let addr: SocketAddr = server.addr();

            let mut child = Command::new(&harness.command[0])
                .args(&harness.command[1..])
                .current_dir(project_dir)
                .env("CYBERSIN_ADAPTER_ADDR", addr.to_string())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("spawning harness process {:?}", harness.command))?;

            let channel = tokio::select! {
                accepted = server.accept(GRPC_ACCEPT_TIMEOUT) => {
                    accepted.with_context(|| {
                        format!(
                            "waiting for harness process {:?} to connect over gRPC",
                            harness.command
                        )
                    })?
                }
                status = child.wait() => {
                    let status = status.context("waiting on harness process")?;
                    anyhow::bail!(
                        "harness process exited unexpectedly ({}) before connecting over gRPC",
                        status
                            .code()
                            .map(|code| format!("code {code}"))
                            .unwrap_or_else(|| "killed by signal".to_string())
                    );
                }
            };
            Ok((child, Box::new(channel) as Box<dyn DaemonChannel>))
        }
        _ => {
            let mut child = Command::new(&harness.command[0])
                .args(&harness.command[1..])
                .current_dir(project_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("spawning harness process {:?}", harness.command))?;
            let child_stdin = child.stdin.take().expect("stdin piped above");
            let child_stdout = child.stdout.take().expect("stdout piped above");
            let channel = StdioDaemonChannel::new(child_stdout, child_stdin);
            Ok((child, Box::new(channel) as Box<dyn DaemonChannel>))
        }
    }
}

/// Waits for `child` to exit and, if it exited non-zero, prepends a clear
/// "harness process exited unexpectedly" note ahead of `err` — the
/// process's own exit status is useful context (a harness that died is
/// worth knowing about on its own), but `err` is not discarded: a harness
/// crash is very often a *symptom* of a daemon-side failure (a channel
/// closing with no reply, because e.g. `handle_llm_request` propagated a
/// model-call error and aborted the session — see issue #48 — makes any
/// harness blocked on that reply panic on the closed channel), not the
/// root cause. An earlier version of this function discarded `err`
/// entirely whenever the child had crashed, which repeatedly hid the
/// actually-actionable error during live testing behind a content-free
/// "the harness crashed" message.
async fn harness_crash_or(child: &mut tokio::process::Child, err: anyhow::Error) -> anyhow::Error {
    match child.wait().await {
        Ok(status) if !status.success() => {
            let exit_description = status
                .code()
                .map(|code| format!("code {code}"))
                .unwrap_or_else(|| "killed by signal".to_string());
            anyhow::anyhow!(
                "harness process exited unexpectedly ({exit_description}) before completing \
the session -- this is often a symptom of the underlying error below, not independent of it: \
{err}"
            )
        }
        _ => err,
    }
}

fn print_summary(summary: &cybersin_runtime::RuntimeSessionSummary) {
    println!(
        "session {} {}: {} spans recorded (see `cybersin trace ls --session {}` and \
         `cybersin cost --by session`)",
        summary.session_id,
        if summary.completed {
            "completed"
        } else {
            "aborted"
        },
        summary.spans_recorded,
        summary.session_id,
    );
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_agent(path: &Path, name: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            format!(
                r#"
name: {name}
harness:
  adapter: process
  command: ["printf", "%s\n", "{{\"type\":\"session.complete\",\"session_id\":\"sess\",\"result\":{{}}}}"]
budget:
  usd_per_session: 1.00
  on_breach: degrade
tools: []
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn infers_the_single_runnable_agent_target() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("dist")).unwrap();
        let agent = project.path().join("agents/hello.agent.yaml");
        write_agent(&agent, "hello-agent");

        assert_eq!(
            infer_agent_yaml(&project.path().join("dist")).unwrap(),
            agent
        );
    }

    #[test]
    fn zero_runnable_agent_targets_is_a_clear_error() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("dist")).unwrap();

        let err = infer_agent_yaml(&project.path().join("dist")).unwrap_err();

        assert!(err.to_string().contains("no runnable agent targets found"));
        assert!(err.to_string().contains("agents"));
        assert!(err.to_string().contains("cybersin run <agent.yaml>"));
    }

    #[test]
    fn multiple_runnable_agent_targets_list_explicit_choices() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("dist")).unwrap();
        write_agent(&project.path().join("agents/alpha.agent.yaml"), "alpha");
        write_agent(&project.path().join("agents/fleet/beta.agent.yaml"), "beta");

        let err = infer_agent_yaml(&project.path().join("dist")).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("multiple runnable agent targets found"));
        assert!(message.contains("cybersin run agents/alpha.agent.yaml"));
        assert!(message.contains("cybersin run agents/fleet/beta.agent.yaml"));
    }

    #[test]
    fn stale_scaffolded_agent_does_not_preempt_single_prompt_starter() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).expect("init");
        std::fs::write(
            project.path().join("prompts/current.prompt.yaml"),
            r#"name: current
quality: medium
sections:
  - id: body
    priority: 100
    body: Write a concise project brief.
"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join("agents/hello.agent.yaml"),
            r#"name: hello-agent
harness: { adapter: process, command: ["python", "loop.py"] }
budget: { usd_per_session: 1.00, on_breach: degrade }
tools: []
"#,
        )
        .unwrap();
        crate::commands::build::run(
            project.path(),
            crate::commands::build::BuildProfile::Dev,
            true,
        )
        .expect("build");

        match infer_run_target(&project.path().join("dist")).expect("run target") {
            RunTarget::BuiltInStarter { prompt_name } => assert_eq!(prompt_name, "current"),
            RunTarget::Agent(path) => panic!("unexpected stale agent target: {}", path.display()),
        }
    }
}
