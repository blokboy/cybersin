//! `cybersin trace ls|show` (spec §8.5, §11).

use std::path::PathBuf;

use clap::Subcommand;
use cybersin_runtime::DaemonHandle;

use crate::capabilities::{
    execute_trace_ls, execute_trace_show, rendered_text, simple_result, trace_ls_result,
    TraceLsInput, TraceShowInput,
};

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
    /// Promote one production LLM trace to a portable eval fixture.
    Sample {
        /// Span id, as printed by `cybersin trace ls`.
        id: String,
        /// Destination `*.eval.yaml` file.
        #[arg(long)]
        to_eval: PathBuf,
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
            let spans = daemon.spans();
            let execution = execute_trace_ls(
                &spans,
                TraceLsInput {
                    session,
                    agent,
                    model,
                    limit,
                },
            )
            .await;
            print!("{}", rendered_text(&execution.events));
            trace_ls_result(&execution.events)
                .unwrap_or_else(|| {
                    Err("trace ls failed: capability did not emit a terminal event".to_string())
                })
                .map_err(anyhow::Error::msg)?;
        }
        TraceCommand::Show { id } => {
            let execution = execute_trace_show(&daemon.spans(), TraceShowInput { id }).await;
            print!("{}", rendered_text(&execution.events));
            simple_result(&execution.events)
                .unwrap_or_else(|| {
                    Err("trace show failed: capability did not emit a terminal event".to_string())
                })
                .map_err(anyhow::Error::msg)?;
        }
        TraceCommand::Sample { id, to_eval } => {
            let span = daemon
                .spans()
                .get(&id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no span with id {id:?}"))?;
            if span.kind != cybersin_trace::SpanKind::LlmCall {
                anyhow::bail!("span {id:?} is not an LLM call and cannot become a prompt eval");
            }
            let inputs = span
                .attributes
                .get("inputs")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let output = span
                .attributes
                .get("output")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let fixture = serde_json::json!({
                "prompt": span.name,
                "cases": [{
                    "name": format!("production_{}", span.id),
                    "inputs": inputs,
                    "assertions": [{"type": "contains_none", "values": ["__cybersin_never__"]}],
                    "recorded_outputs": [{
                        "output": output,
                        "judge_score": span.attributes.get("judge_score").cloned()
                    }]
                }],
                "runs_per_case": 1
            });
            if let Some(parent) = to_eval.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&to_eval, serde_yaml::to_string(&fixture)?)?;
            println!("wrote {}", to_eval.display());
        }
    }
    Ok(())
}

#[cfg(test)]
fn trace_ls_rendered_text(events: &[crate::capabilities::CapabilityEvent]) -> String {
    rendered_text(events)
}

#[cfg(test)]
mod tests {
    use crate::capabilities::{trace_ls_output_stream, CapabilityEvent, OutputMode};
    use cybersin_trace::{CacheStatus, Span, SpanKind, SpanStatus, SpanStore};

    use super::*;

    fn sample_span(id: &str, start_unix_ms: i64) -> Span {
        Span {
            id: id.to_string(),
            session_id: "sess-1".to_string(),
            agent_name: "agent-a".to_string(),
            kind: SpanKind::LlmCall,
            name: "researcher".to_string(),
            start_unix_ms,
            end_unix_ms: start_unix_ms + 10,
            model: Some("gpt-4o-mini".to_string()),
            tokens_prompt: Some(120),
            tokens_completion: Some(40),
            usd_cost: 0.000432,
            cache_status: CacheStatus::Miss,
            retries: 0,
            evicted_sections: vec![],
            status: SpanStatus::Ok,
            attributes: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn cli_adapter_text_matches_direct_trace_ls_capability() {
        let store = SpanStore::in_memory().await.unwrap();
        store.insert(&sample_span("older", 1)).await.unwrap();
        store.insert(&sample_span("newer", 2)).await.unwrap();

        let input = TraceLsInput {
            session: Some("sess-1".to_string()),
            limit: Some(1),
            ..Default::default()
        };
        let direct = execute_trace_ls(&store, input).await;
        let capability_text = direct
            .events
            .iter()
            .find_map(|event| match event {
                CapabilityEvent::Output {
                    mode: OutputMode::Text,
                    value,
                } => trace_ls_output_stream(value).map(|(_, text)| text.to_string()),
                _ => None,
            })
            .expect("trace ls capability should emit text output");

        assert_eq!(trace_ls_result(&direct.events), Some(Ok(())));
        assert_eq!(trace_ls_rendered_text(&direct.events), capability_text);
        assert!(capability_text.contains("ID"));
        assert!(capability_text.contains("newer"));
        assert!(!capability_text.contains("older"));
    }
}
