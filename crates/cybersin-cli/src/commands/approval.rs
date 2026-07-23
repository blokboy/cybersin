//! `cybersin approve|deny <call-id>` (spec §8.2): resolve a call parked by
//! an approval-gate policy hook. Approval resumes the session and runs
//! the call; denial resolves it to `failed(reason: "denied")` through the
//! same result path any failed call takes, without killing the session.
//!
//! Same stand-in-executor caveat as `dlq retry` (see `commands::dlq`'s
//! doc): `approve` runs the now-cleared call against
//! [`cybersin_gateway::EchoExecutor`], since no real tool backend is
//! wired into this workspace yet.

use std::path::PathBuf;
use std::sync::Arc;

use cybersin_gateway::{EchoExecutor, ToolGateway};
use cybersin_runtime::DaemonHandle;

use crate::commands::dlq::print_outcome;

pub async fn approve(db_path: PathBuf, call_id: String) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor));
    let outcome = gateway.approve(&call_id).await?;
    print_outcome(&call_id, &outcome);
    Ok(())
}

pub async fn deny(db_path: PathBuf, call_id: String) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor));
    let outcome = gateway.deny(&call_id).await?;
    print_outcome(&call_id, &outcome);
    Ok(())
}
