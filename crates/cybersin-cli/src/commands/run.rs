//! `cybersin run` (spec §11: `cybersin run <agent.yaml> [--input f]`).
//!
//! Two paths: `--stub` drives the M1 stub agent against a hand-written
//! `dist/` fixture (spec §14's M1 exit criterion); `<agent.yaml>` (issue
//! #35 Phase 3) spawns the declared `harness: { adapter: process, command:
//! [...] }` process and drives a real `RuntimeDaemon` session against it
//! over the stdio adapter protocol (spec §10), with live OpenRouter model
//! calling (Phase 1) and sandboxed tool execution (Phase 2) both wired in.
//! gRPC transport and full retry-class-bounded tool-call ledger
//! integration are out of scope for this issue — see
//! `crate::tool_executor::GatewayToolCaller`'s doc comment for the latter.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Args;
use cybersin_adapter::transport::stdio::StdioDaemonChannel;
use cybersin_runtime::{
    stub_agent, DaemonHandle, DistFixture, ModelAllowlist, OpenRouterModelCaller,
};
use tokio::process::Command;

use crate::harness_config::AgentMeta;
use crate::tool_executor::{self, GatewayToolCaller};

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
    match (args.stub, args.agent_yaml.clone()) {
        (true, _) => run_stub(db_path, dist_dir, args).await,
        (false, Some(agent_yaml)) => {
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
        (false, None) => anyhow::bail!("cybersin run needs either <agent.yaml> or --stub"),
    }
}

async fn run_stub(db_path: PathBuf, dist_dir: PathBuf, args: RunArgs) -> anyhow::Result<()> {
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

    let summary = stub_agent::run_stub_session(
        daemon.storage(),
        daemon.spans(),
        dist,
        session_id.clone(),
        agent_name,
    )
    .await?;

    print_summary(&summary);
    Ok(())
}

async fn run_live(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: crate::commands::sandbox::Backend,
    agent_yaml: PathBuf,
    args: RunArgs,
) -> anyhow::Result<()> {
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

    let model_caller =
        OpenRouterModelCaller::from_env(dist.clone()).context("configuring live model calling")?;
    let project_dir: &Path = dist_dir.parent().unwrap_or_else(|| Path::new("."));
    let allowlist = ModelAllowlist::load(project_dir).with_context(|| {
        format!(
            "reading {}",
            project_dir.join("cybersin.local.yaml").display()
        )
    })?;

    let executor = tool_executor::configured_executor(&dist_dir, &sandbox_root, sandbox_backend)
        .context("configuring live tool execution")?;
    let tool_caller = GatewayToolCaller::new(executor);

    let session_id = args
        .session_id
        .unwrap_or_else(|| format!("sess-{}", now_unix_ms()));

    println!(
        "spawning harness: session={session_id} agent={agent_name} command={:?}",
        meta.harness.command
    );

    let mut child = Command::new(&meta.harness.command[0])
        .args(&meta.harness.command[1..])
        .current_dir(project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning harness process {:?}", meta.harness.command))?;

    let child_stdin = child.stdin.take().expect("stdin piped above");
    let child_stdout = child.stdout.take().expect("stdout piped above");
    let channel = StdioDaemonChannel::new(child_stdout, child_stdin);

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
    let mut daemon_task = tokio::spawn(runtime_daemon.run());

    let summary = tokio::select! {
        result = &mut daemon_task => {
            // The daemon loop ended on its own (SessionComplete or a
            // closed channel) — reap the child now that its stdin (owned
            // by the dropped RuntimeDaemon's channel) has closed; a
            // well-behaved harness exits promptly once it sees EOF.
            match result.context("daemon task panicked")? {
                Ok(summary) => summary,
                Err(err) => return Err(harness_crash_or(&mut child, err.into()).await),
            }
        }
        status = child.wait() => {
            // The process exited before the daemon observed completion —
            // a crash, not a normal end of session.
            daemon_task.abort();
            let status = status.context("waiting on harness process")?;
            anyhow::bail!(
                "harness process exited unexpectedly ({}) before completing the session",
                status
                    .code()
                    .map(|code| format!("code {code}"))
                    .unwrap_or_else(|| "killed by signal".to_string())
            );
        }
    };

    print_summary(&summary);
    Ok(())
}

/// Waits for `child` to exit and, if it exited non-zero, returns a clear
/// "harness process exited unexpectedly" error in place of `err` — the
/// process's own exit status is more useful than whatever transport error
/// its death produced on the channel talking to it.
async fn harness_crash_or(child: &mut tokio::process::Child, err: anyhow::Error) -> anyhow::Error {
    match child.wait().await {
        Ok(status) if !status.success() => anyhow::anyhow!(
            "harness process exited unexpectedly ({}) before completing the session",
            status
                .code()
                .map(|code| format!("code {code}"))
                .unwrap_or_else(|| "killed by signal".to_string())
        ),
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
