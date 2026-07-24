//! `cybersin approve|deny <call-id>` (spec §8.2): resolve a call parked by
//! an approval-gate policy hook. Approval resumes the session and runs
//! the call; denial resolves it to `failed(reason: "denied")` through the
//! same result path any failed call takes, without killing the session.
//!
//! `approve` runs the now-cleared call through the same compiled,
//! sandboxed executor used by `dlq retry`.

use std::path::PathBuf;

use cybersin_gateway::ToolGateway;
use cybersin_runtime::DaemonHandle;

use crate::commands::dlq::print_outcome;
use crate::commands::sandbox::Backend;
use crate::tool_executor::configured_executor;

pub async fn approve(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: Backend,
    call_id: String,
) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let executor = configured_executor(&dist_dir, &sandbox_root, sandbox_backend)?;
    let gateway = ToolGateway::new(daemon.storage(), executor);
    let outcome = gateway.approve(&call_id).await?;
    print_outcome(&call_id, &outcome);
    Ok(())
}

pub async fn deny(
    db_path: PathBuf,
    dist_dir: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: Backend,
    call_id: String,
) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let executor = configured_executor(&dist_dir, &sandbox_root, sandbox_backend)?;
    let gateway = ToolGateway::new(daemon.storage(), executor);
    let outcome = gateway.deny(&call_id).await?;
    print_outcome(&call_id, &outcome);
    Ok(())
}
