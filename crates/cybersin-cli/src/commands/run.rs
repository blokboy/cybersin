//! `cybersin run` (spec §11: `cybersin run <agent.yaml> [--input f]`).
//!
//! This issue only implements the `--stub` path: driving the M1 stub
//! agent against a hand-written `dist/` fixture end-to-end (spec §14's M1
//! exit criterion). Compiling and running a real `*.agent.yaml` needs the
//! frontend/passes/router/backends crates this issue deliberately doesn't
//! touch (scope discipline, per the issue description) — `agent_yaml` is
//! accepted as a positional arg so the command's shape already matches
//! spec §11, but is rejected with a clear message until that pipeline
//! exists.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use cybersin_runtime::{bundled_stub_dist_dir, stub_agent, DaemonHandle, DistFixture};

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to an `*.agent.yaml` (spec §11). Not yet implemented in this
    /// issue — pass `--stub` instead.
    pub agent_yaml: Option<PathBuf>,

    /// Run the M1 stub agent end-to-end against a hand-written `dist/`
    /// fixture instead of a compiled agent (spec §14's M1 exit criterion:
    /// "stub agent runs on a hand-written dist/").
    #[arg(long)]
    pub stub: bool,

    /// Override the bundled stub `dist/` fixture directory
    /// (`crates/cybersin-runtime/fixtures/dist/` by default).
    #[arg(long)]
    pub dist: Option<PathBuf>,

    /// Session id to record this run under. Defaults to a fresh
    /// timestamp-based id so repeated runs don't collide in the trace
    /// store.
    #[arg(long)]
    pub session_id: Option<String>,

    /// Agent name spans/sessions are attributed to (the `agent` dimension
    /// of `cybersin cost --by agent`).
    #[arg(long, default_value = "research-agent")]
    pub agent: String,
}

pub async fn execute(db_path: PathBuf, args: RunArgs) -> anyhow::Result<()> {
    if !args.stub {
        anyhow::bail!(
            "cybersin run currently only supports `--stub` (real *.agent.yaml compilation \
             needs the frontend/passes/router/backends crates, which are later issues); \
             try `cybersin run --stub`"
        );
    }

    let dist_dir = args.dist.unwrap_or_else(bundled_stub_dist_dir);
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

    println!(
        "running stub agent: session={session_id} agent={} dist={}",
        args.agent,
        dist_dir.display()
    );

    let summary = stub_agent::run_stub_session(
        daemon.storage(),
        daemon.spans(),
        dist,
        session_id.clone(),
        args.agent,
    )
    .await?;

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
    Ok(())
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
