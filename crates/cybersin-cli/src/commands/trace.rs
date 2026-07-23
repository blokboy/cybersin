//! `cybersin trace ls|show` (spec §8.5, §11).

use std::path::PathBuf;

use clap::Subcommand;
use cybersin_runtime::DaemonHandle;
use cybersin_trace::SpanFilter;

#[derive(Debug, Subcommand)]
pub enum TraceCommand {
    /// List recorded spans, most recent first.
    Ls {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Show one span's full detail as JSON.
    Show {
        /// Span id, as printed by `cybersin trace ls`.
        id: String,
    },
}

pub async fn execute(db_path: PathBuf, cmd: TraceCommand) -> anyhow::Result<()> {
    // Same auto-start entry point `run` uses: `trace`/`cost` are runtime
    // commands too (spec §1), so they auto-start `cybersind` against the
    // same state file rather than requiring a prior `run` in-process.
    let daemon = DaemonHandle::auto_start(&db_path).await?;

    match cmd {
        TraceCommand::Ls {
            session,
            agent,
            model,
            limit,
        } => {
            let filter = SpanFilter {
                session_id: session,
                agent_name: agent,
                kind: None,
                model,
                limit,
            };
            let spans = daemon.spans().list(&filter).await?;
            if spans.is_empty() {
                println!("no spans recorded yet — try `cybersin run --stub` first");
                return Ok(());
            }
            println!(
                "{:<24} {:<14} {:<16} {:<16} {:>6} {:>6} {:>10} {:<8}",
                "ID", "KIND", "NAME", "MODEL", "PTOK", "CTOK", "USD", "CACHE"
            );
            for span in spans {
                println!(
                    "{:<24} {:<14} {:<16} {:<16} {:>6} {:>6} {:>10.6} {:<8}",
                    span.id,
                    span.kind.as_str(),
                    span.name,
                    span.model.as_deref().unwrap_or("-"),
                    span.tokens_prompt
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    span.tokens_completion
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    span.usd_cost,
                    span.cache_status.as_str(),
                );
            }
        }
        TraceCommand::Show { id } => match daemon.spans().get(&id).await? {
            Some(span) => println!("{}", serde_json::to_string_pretty(&span)?),
            None => anyhow::bail!("no span with id {id:?}"),
        },
    }
    Ok(())
}
