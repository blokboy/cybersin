//! `cybersin cost --by session|agent|model|tool|day` (spec §8.5, §11).

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use cybersin_runtime::DaemonHandle;
use cybersin_trace::CostDimension;

use crate::capabilities::{execute_cost, rendered_text, simple_result, CostInput};

/// CLI-facing mirror of `cybersin_trace::CostDimension`, so `clap`'s
/// `ValueEnum` derive (and its dependencies) stay out of `cybersin-trace`
/// — that crate's dependency list stops at serde/sqlx/thiserror (spec
/// §13's dependency discipline).
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CostByArg {
    Session,
    Agent,
    Model,
    Tool,
    Day,
}

impl From<CostByArg> for CostDimension {
    fn from(value: CostByArg) -> Self {
        match value {
            CostByArg::Session => CostDimension::Session,
            CostByArg::Agent => CostDimension::Agent,
            CostByArg::Model => CostDimension::Model,
            CostByArg::Tool => CostDimension::Tool,
            CostByArg::Day => CostDimension::Day,
        }
    }
}

#[derive(Debug, Args)]
pub struct CostArgs {
    /// Grouping dimension for the rollup.
    #[arg(long = "by", value_enum)]
    pub by: CostByArg,
}

pub async fn execute(db_path: PathBuf, args: CostArgs) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(&db_path).await?;
    let dimension: CostDimension = args.by.into();
    let execution = execute_cost(&daemon.spans(), CostInput { by: dimension }).await;
    print!("{}", rendered_text(&execution.events));
    simple_result(&execution.events)
        .unwrap_or_else(|| Err("cost failed: capability did not emit a terminal event".to_string()))
        .map_err(anyhow::Error::msg)
}
