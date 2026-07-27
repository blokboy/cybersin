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

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Args;
use cybersin_adapter::channel::DaemonChannel;
use cybersin_adapter::transport::grpc;
use cybersin_adapter::transport::stdio::StdioDaemonChannel;
use cybersin_runtime::{
    stub_agent, DaemonHandle, DistFixture, LocalConfigFile, ModelAllowlist, OpenRouterModelCaller,
    RuntimeSessionSummary,
};
use tokio::process::{Child, Command};

use crate::commands::build::discover_agent_sources;
use crate::harness_config::AgentMeta;
use crate::readiness;
use crate::tool_executor::{self, GatewayToolCaller};

/// How long `cybersin run`'s `harness.adapter: grpc` path waits for the
/// spawned harness process to open its `Session` RPC before giving up —
/// bounded so a harness that hangs or never speaks gRPC can't block
/// forever. Internal plumbing, not exposed as a CLI flag (matches this
/// codebase's existing `CYBERSIN_CONTAINER_RUNTIME`-style scope
/// discipline for knobs nothing has asked to configure yet).
const GRPC_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

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
    #[arg(long)]
    pub session_id: Option<String>,

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
    match (args.stub, args.agent_yaml.clone()) {
        (true, _) => run_stub(db_path, dist_dir, args).await,
        (false, explicit_agent_yaml) => {
            let agent_yaml = match explicit_agent_yaml {
                Some(agent_yaml) => agent_yaml,
                None => infer_agent_yaml(&dist_dir)?,
            };
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
    }
}

fn infer_agent_yaml(dist_dir: &Path) -> anyhow::Result<PathBuf> {
    let project_dir = project_dir_from_dist(dist_dir);
    let candidates = runnable_agent_targets(project_dir)?;
    match candidates.as_slice() {
        [] => anyhow::bail!(
            "no runnable agent targets found in {}; create an agents/*.agent.yaml file or run `cybersin run <agent.yaml>` explicitly",
            project_dir.join("agents").display()
        ),
        [single] => Ok(single.clone()),
        multiple => {
            let choices = multiple
                .iter()
                .map(|path| format!("  - cybersin run {}", display_project_path(project_dir, path)))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "multiple runnable agent targets found; choose one explicitly:\n{choices}"
            )
        }
    }
}

fn runnable_agent_targets(project_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let candidates = discover_agent_sources(project_dir).map_err(anyhow::Error::msg)?;
    let mut runnable = Vec::new();
    for candidate in candidates {
        let yaml_source = std::fs::read_to_string(&candidate)
            .with_context(|| format!("reading {}", candidate.display()))?;
        AgentMeta::from_agent_yaml(&yaml_source)
            .with_context(|| format!("parsing {}", candidate.display()))?;
        runnable.push(candidate);
    }
    Ok(runnable)
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

    println!(
        "running stub agent: session={session_id} agent={agent_name} dist={}",
        dist_dir.display()
    );

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

async fn run_live(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    agent_yaml: PathBuf,
    args: RunArgs,
) -> anyhow::Result<RuntimeSessionSummary> {
    let dist = Arc::new(DistFixture::load_dir(&dist_dir)?);

    let yaml_source = std::fs::read_to_string(&agent_yaml)
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

    println!("cybersind: auto-starting (state: {})", db_path.display());
    let daemon = DaemonHandle::auto_start(&db_path).await?;

    let project_dir = project_dir_from_dist(&dist_dir);
    let local_config = LocalConfigFile::load_optional(project_dir).with_context(|| {
        format!(
            "reading {}",
            project_dir.join("cybersin.local.yaml").display()
        )
    })?;
    let model_caller = openrouter_from_local_config(dist.clone(), local_config.as_ref())
        .context("configuring live model calling")?;
    let allowlist = local_config
        .as_ref()
        .map(LocalConfigFile::model_allowlist)
        .unwrap_or_else(ModelAllowlist::allow_all);

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

    println!(
        "spawning harness: session={session_id} agent={agent_name} command={:?} (adapter={})",
        meta.harness.command, meta.harness.adapter
    );

    let (mut child, channel) = spawn_harness(&meta.harness, project_dir).await?;

    let mut runtime_daemon = cybersin_runtime::RuntimeDaemon::new(
        channel,
        daemon.storage(),
        daemon.spans(),
        dist,
        session_id.clone(),
        agent_name,
    )
    .with_models(model_caller, allowlist)
    .with_tool_caller(tool_caller);
    // A harness that crashes immediately closes its stdin/stdout out from
    // under the channel before we've necessarily observed its exit status,
    // so a send/recv here can fail with a raw transport error (e.g. a
    // broken pipe) instead of the more useful "the process died" signal.
    // Any time a channel operation on this session errors, prefer the
    // child's actual exit status over the raw transport error.
    if let Err(err) = runtime_daemon.start_session(inputs).await {
        return Err(harness_crash_or(&mut child, err.into()).await);
    }

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
    let daemon_fut = runtime_daemon.run();
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
                Err(err) => return Err(harness_crash_or(&mut child, err.into()).await),
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
}
