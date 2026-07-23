//! `cybersin dlq ls|show|retry|drop` (spec §8.2, §11): the dead-letter
//! queue of `tool_calls` rows that reached `failed` and haven't been
//! `drop`ped.
//!
//! Like `run --stub`, this issue has no real tool backend to execute
//! against yet (that's a later issue, alongside `cybersin-sandbox`) — a
//! `dlq retry` here runs against [`cybersin_gateway::EchoExecutor`], the
//! gateway crate's own stand-in, so the command is fully real end to end
//! except for what tool code actually gets invoked.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use cybersin_gateway::{EchoExecutor, GatewayOutcome, ToolGateway};
use cybersin_runtime::DaemonHandle;

#[derive(Debug, Subcommand)]
pub enum DlqCommand {
    /// List dead letters, most recently updated first.
    Ls,
    /// Show one dead letter's full detail as JSON.
    Show {
        /// Call id, as printed by `cybersin dlq ls` (`"{tool}:{idem_key}"`).
        call_id: String,
    },
    /// Reopen a dead letter and run it again, regardless of retry class —
    /// an explicit human override (spec §8.2).
    Retry { call_id: String },
    /// Acknowledge and discard a dead letter without deleting its audit
    /// row — it stops showing up in `dlq ls`.
    Drop { call_id: String },
}

pub async fn execute(db_path: PathBuf, cmd: DlqCommand) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor));

    match cmd {
        DlqCommand::Ls => {
            let rows = gateway.dlq_list().await?;
            if rows.is_empty() {
                println!("no dead letters");
                return Ok(());
            }
            println!(
                "{:<28} {:<12} {:<10} {:>8} {:<20}",
                "CALL_ID", "SESSION", "CLASS", "ATTEMPTS", "REASON"
            );
            for row in rows {
                println!(
                    "{:<28} {:<12} {:<10} {:>8} {:<20}",
                    row.call_id,
                    row.session_id,
                    row.retry_class,
                    row.attempts,
                    row.failure_reason.as_deref().unwrap_or("-"),
                );
            }
        }
        DlqCommand::Show { call_id } => {
            let row = gateway.dlq_show(&call_id).await?;
            println!("{}", serde_json::to_string_pretty(&row)?);
        }
        DlqCommand::Retry { call_id } => {
            let outcome = gateway.dlq_retry(&call_id).await?;
            print_outcome(&call_id, &outcome);
        }
        DlqCommand::Drop { call_id } => {
            gateway.dlq_drop(&call_id).await?;
            println!("dropped {call_id}");
        }
    }
    Ok(())
}

pub(crate) fn print_outcome(call_id: &str, outcome: &GatewayOutcome) {
    match outcome {
        GatewayOutcome::Resolved(cybersin_adapter::messages::CallOutcome::Ok { value }) => {
            println!("{call_id} succeeded: {value}");
        }
        GatewayOutcome::Resolved(cybersin_adapter::messages::CallOutcome::Failed {
            reason,
            retriable,
        }) => {
            println!("{call_id} failed (retriable={retriable}): {reason}");
        }
        GatewayOutcome::Parked { approval_id, .. } => {
            println!("{call_id} parked, awaiting approval {approval_id}");
        }
    }
}
