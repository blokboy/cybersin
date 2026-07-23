//! `cybersin cost --by session|agent|model|tool|day` (spec §8.5, §11).

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use cybersin_runtime::DaemonHandle;
use cybersin_trace::CostDimension;

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
    let rows = daemon.spans().cost_rollup(dimension).await?;

    if rows.is_empty() {
        println!("no cost data yet — try `cybersin run --stub` first");
        return Ok(());
    }

    println!(
        "{:<24} {:>10} {:>8} {:>10} {:>10}",
        dimension.to_string().to_uppercase(),
        "USD",
        "SPANS",
        "PTOK",
        "CTOK"
    );
    let mut total_usd = 0.0;
    for row in &rows {
        println!(
            "{:<24} {:>10.6} {:>8} {:>10} {:>10}",
            row.key, row.usd_cost, row.span_count, row.tokens_prompt, row.tokens_completion
        );
        total_usd += row.usd_cost;
    }
    println!("{:<24} {:>10.6}", "TOTAL", total_usd);
    Ok(())
}
