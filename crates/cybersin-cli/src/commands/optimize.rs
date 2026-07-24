//! `cybersin optimize [PATH] [--since t] [--traces f]` (spec §9, §11):
//! reads recorded trace data — either the daemon's SQLite span store, or
//! a portable JSONL export for CI/portability — and re-derives the
//! cache-similarity threshold and judge-trigger band from observed
//! judge-band verdicts. Output is a normal build (via
//! [`crate::commands::build::run_into`], threading the derived
//! [`ObservedRoutingStats`] through) plus `optimize-report.md` naming
//! each change and its evidence.
//!
//! Eval-scored production samples (spec §6.4/§8.6) would let this
//! command also validate *direct* cache hits, not just judge-band ones.
//! Runtime spans do not carry that linkage yet, so this first cut tunes
//! the two levers the router already exposes an observed override for
//! ([`ObservedRoutingStats`]), from data the route/cache executor (issue
//! #15) records on every `CacheDecision` span: similarity and judge
//! accept/reject. `cybersin eval gate` remains the independent quality
//! regression gate.
//!
//! Nothing here writes back to `cybersin.yaml`. The declared cost model
//! stays the conservative cold-start floor; `optimize`'s tuning is
//! re-derived fresh from whatever trace window is passed each time it
//! runs, and ships as a normal build + report for a human to review and
//! commit — spec §9's "PGO changes always ship through PR review", "auto-
//! merges" never happens because this command doesn't touch git at all.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use cybersin_router::{ObservedRoutingStats, ProjectConfig};
use cybersin_runtime::DaemonHandle;
use cybersin_trace::{CacheStatus, Span, SpanFilter, SpanKind};

use crate::commands::build::{self, BuildProfile};

#[derive(Debug, Args)]
pub struct OptimizeArgs {
    /// Project directory containing prompts/ and cybersin.lock.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Only consider spans recorded in this trailing window, e.g. `7d`,
    /// `24h`, `30m`. Default: every recorded span.
    #[arg(long)]
    pub since: Option<String>,
    /// Read spans from a portable JSONL export (one `Span` per line)
    /// instead of the daemon's trace store — the CI-friendly path.
    #[arg(long)]
    pub traces: Option<PathBuf>,
    /// Build profile for the emitted build.
    #[arg(long, value_enum, default_value = "release")]
    pub profile: BuildProfile,
    /// Refuse any build pass that would need a network call.
    #[arg(long)]
    pub frozen: bool,
}

/// The judge-trigger band is split into this many equal-width similarity
/// buckets for evidence-gathering. Only the two edge buckets (nearest
/// the cache threshold, and nearest the band floor) ever drive a
/// proposed change — see [`analyze`].
const BAND_BUCKETS: usize = 5;
/// A bucket needs at least this many judge calls before its accept/
/// reject rate is trusted for a routing change — small samples swing
/// wildly and would make `optimize` noisy from run to run.
const MIN_BUCKET_SAMPLES: usize = 20;
/// How consistently a bucket must agree before `optimize` acts on it.
/// Symmetric for "safe to promote to a direct hit" and "safe to skip
/// the judge entirely" — both are a bet that the judge's verdict in
/// that bucket is now predictable enough not to need calling it live.
const AGREEMENT_RATE: f64 = 0.95;

pub async fn execute(db_path: PathBuf, args: OptimizeArgs) -> anyhow::Result<()> {
    let since_unix_ms = args
        .since
        .as_deref()
        .map(parse_since)
        .transpose()
        .map_err(anyhow::Error::msg)?;

    let (spans, source_desc) = if let Some(traces_path) = &args.traces {
        let mut spans = load_jsonl(traces_path).map_err(anyhow::Error::msg)?;
        if let Some(cutoff) = since_unix_ms {
            spans.retain(|span| span.start_unix_ms >= cutoff);
        }
        (spans, traces_path.display().to_string())
    } else {
        let daemon = DaemonHandle::auto_start(&db_path).await?;
        let filter = SpanFilter {
            since_unix_ms,
            ..Default::default()
        };
        let spans = daemon.spans().list(&filter).await?;
        (spans, format!("trace store at {}", db_path.display()))
    };

    let project_yaml_path = args.path.join("cybersin.yaml");
    let project_yaml = fs::read_to_string(&project_yaml_path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", project_yaml_path.display()))?;
    let project: ProjectConfig = serde_yaml::from_str(&project_yaml)
        .map_err(|e| anyhow::anyhow!("invalid {}: {e}", project_yaml_path.display()))?;

    let analysis = analyze(
        &spans,
        project.cost_model.cache_similarity_threshold,
        project.cost_model.judge_trigger_band,
    );

    let report = render_report(&analysis, &source_desc, since_unix_ms, spans.len());
    let report_path = args.path.join("optimize-report.md");
    fs::write(&report_path, &report)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", report_path.display()))?;

    let observed = analysis.observed_stats();
    build::run_into(
        &args.path,
        &args.path.join("dist"),
        args.profile,
        args.frozen,
        observed.as_ref(),
    )
    .map_err(|message| anyhow::anyhow!(message))?;

    println!("wrote {}", report_path.display());
    if analysis.changes.is_empty() {
        println!("no routing changes recommended");
    } else {
        for change in &analysis.changes {
            println!("{}", change.headline);
        }
    }
    Ok(())
}

/// One proposed routing-config change, with the observed evidence that
/// justifies it — the pair `optimize-report.md` renders per bullet.
struct Change {
    headline: String,
    evidence: String,
}

#[derive(Default)]
struct Stats {
    cache_decisions: usize,
    hits: usize,
    judge_calls: usize,
    judge_accepted: usize,
    cascade_calls: usize,
    cascade_escalations: usize,
    observed_cost_usd: f64,
}

struct Analysis {
    baseline_threshold: f64,
    baseline_band: [f64; 2],
    new_threshold: Option<f64>,
    new_band: Option<[f64; 2]>,
    changes: Vec<Change>,
    stats: Stats,
}

impl Analysis {
    fn observed_stats(&self) -> Option<ObservedRoutingStats> {
        if self.new_threshold.is_none() && self.new_band.is_none() {
            return None;
        }
        Some(ObservedRoutingStats {
            cache_similarity_threshold: self.new_threshold,
            judge_trigger_band: self.new_band,
        })
    }
}

#[derive(Default, Clone, Copy)]
struct Bucket {
    accepted: u32,
    rejected: u32,
}

impl Bucket {
    fn record(&mut self, accepted: bool) {
        if accepted {
            self.accepted += 1;
        } else {
            self.rejected += 1;
        }
    }

    fn total(&self) -> usize {
        (self.accepted + self.rejected) as usize
    }

    fn accept_rate(&self) -> f64 {
        self.accepted as f64 / self.total() as f64
    }

    fn reject_rate(&self) -> f64 {
        self.rejected as f64 / self.total() as f64
    }
}

/// Derive proposed `(cache_similarity_threshold, judge_trigger_band)`
/// overrides from observed `CacheDecision` spans (spec §8.3's "every
/// decision... lands in span attributes — PGO raw material").
///
/// Only judge-reviewed decisions (`judge_hit`/`judge_reject`, recorded
/// with a `similarity` attribute by `route_executor::execute_cache`)
/// carry a verdict this analysis can trust: hash/knn hits never asked
/// the judge, so there's no signal on whether they *should* have. The
/// band is split into [`BAND_BUCKETS`] equal-width similarity buckets;
/// only the two edge buckets ever move a threshold:
///
/// - the bucket nearest the cache threshold, if the judge agrees with
///   a cache hit almost every time there — promote it to a direct hit
///   and lower the threshold to match, saving a judge call per request.
/// - the bucket nearest the band floor, if the judge almost always
///   rejects there — raise the band's lower edge past it, skipping a
///   judge call that was never going to accept anyway.
fn analyze(spans: &[Span], baseline_threshold: f64, baseline_band: [f64; 2]) -> Analysis {
    let mut stats = Stats::default();
    let band_width = baseline_band[1] - baseline_band[0];
    let mut buckets = [Bucket::default(); BAND_BUCKETS];

    for span in spans {
        stats.observed_cost_usd += span.usd_cost;

        match span.kind {
            SpanKind::CacheDecision => {
                stats.cache_decisions += 1;
                if span.cache_status == CacheStatus::Hit {
                    stats.hits += 1;
                }

                let decision = span.attributes.get("decision").and_then(|v| v.as_str());
                if !matches!(decision, Some("judge_hit") | Some("judge_reject")) {
                    continue;
                }
                let Some(similarity) = span.attributes.get("similarity").and_then(|v| v.as_f64())
                else {
                    continue;
                };
                stats.judge_calls += 1;
                let accepted = decision == Some("judge_hit");
                if accepted {
                    stats.judge_accepted += 1;
                }

                if band_width <= 0.0
                    || similarity < baseline_band[0]
                    || similarity > baseline_band[1]
                {
                    continue;
                }
                let bucket_width = band_width / BAND_BUCKETS as f64;
                let idx = (((similarity - baseline_band[0]) / bucket_width) as usize)
                    .min(BAND_BUCKETS - 1);
                buckets[idx].record(accepted);
            }
            SpanKind::LlmCall => {
                if let Some(decision) = span.attributes.get("decision").and_then(|v| v.as_str()) {
                    if matches!(decision, "cascade_accept" | "cascade_escalation") {
                        stats.cascade_calls += 1;
                        if decision == "cascade_escalation" {
                            stats.cascade_escalations += 1;
                        }
                    }
                }
            }
            SpanKind::ToolCall | SpanKind::SandboxExec => {}
        }
    }

    let mut new_threshold = None;
    let mut new_band_lo = None;
    let mut new_band_hi = None;
    let mut changes = Vec::new();

    if band_width > 0.0 {
        let bucket_width = band_width / BAND_BUCKETS as f64;

        let top = buckets[BAND_BUCKETS - 1];
        if top.total() >= MIN_BUCKET_SAMPLES && top.accept_rate() >= AGREEMENT_RATE {
            let bucket_lo = baseline_band[0] + bucket_width * (BAND_BUCKETS - 1) as f64;
            new_threshold = Some(bucket_lo);
            new_band_hi = Some(bucket_lo);
            changes.push(Change {
                headline: format!(
                    "cache_similarity_threshold: {:.4} -> {:.4} (lowered)",
                    baseline_threshold, bucket_lo
                ),
                evidence: format!(
                    "{:.1}% of judge calls with similarity in [{:.4}, {:.4}] were accepted \
                     ({} samples) — safe to skip the judge there and treat them as direct hits.",
                    top.accept_rate() * 100.0,
                    bucket_lo,
                    baseline_band[1],
                    top.total()
                ),
            });
        }

        let bottom = buckets[0];
        let effective_threshold = new_threshold.unwrap_or(baseline_threshold);
        let bucket_hi = baseline_band[0] + bucket_width;
        if bottom.total() >= MIN_BUCKET_SAMPLES
            && bottom.reject_rate() >= AGREEMENT_RATE
            && bucket_hi <= effective_threshold
        {
            new_band_lo = Some(bucket_hi);
            changes.push(Change {
                headline: format!(
                    "judge_trigger_band lower bound: {:.4} -> {:.4} (raised)",
                    baseline_band[0], bucket_hi
                ),
                evidence: format!(
                    "{:.1}% of judge calls with similarity in [{:.4}, {:.4}] were rejected \
                     ({} samples) — not worth a judge call anymore; routing straight past it.",
                    bottom.reject_rate() * 100.0,
                    baseline_band[0],
                    bucket_hi,
                    bottom.total()
                ),
            });
        }
    }

    let new_band = if new_band_lo.is_some() || new_band_hi.is_some() {
        Some([
            new_band_lo.unwrap_or(baseline_band[0]),
            new_band_hi.unwrap_or(baseline_band[1]),
        ])
    } else {
        None
    };

    Analysis {
        baseline_threshold,
        baseline_band,
        new_threshold,
        new_band,
        changes,
        stats,
    }
}

fn render_report(
    analysis: &Analysis,
    source_desc: &str,
    since_unix_ms: Option<i64>,
    span_count: usize,
) -> String {
    let mut out = String::new();
    out.push_str("# cybersin optimize report\n\n");
    out.push_str(&format!("Source: {source_desc}\n"));
    match since_unix_ms {
        Some(cutoff) => out.push_str(&format!("Window: spans since unix_ms {cutoff}\n")),
        None => out.push_str("Window: all recorded spans\n"),
    }
    out.push_str(&format!("Spans considered: {span_count}\n\n"));

    out.push_str(&format!(
        "Baseline: cache_similarity_threshold = {:.4}, judge_trigger_band = [{:.4}, {:.4}]\n\n",
        analysis.baseline_threshold, analysis.baseline_band[0], analysis.baseline_band[1]
    ));

    out.push_str("## Changes\n\n");
    if analysis.changes.is_empty() {
        out.push_str(&format!(
            "No routing changes recommended — not enough observed judge-band evidence yet \
             (need at least {MIN_BUCKET_SAMPLES} samples per similarity bucket, with \
             {:.0}% agreement).\n\n",
            AGREEMENT_RATE * 100.0
        ));
    } else {
        for change in &analysis.changes {
            out.push_str(&format!(
                "- **{}**\n  {}\n",
                change.headline, change.evidence
            ));
        }
        out.push('\n');
    }

    out.push_str("## Observed stats (informational)\n\n");
    let stats = &analysis.stats;
    out.push_str(&format!(
        "- Cache decisions: {}, hit rate {:.1}%\n",
        stats.cache_decisions,
        percent(stats.hits, stats.cache_decisions)
    ));
    out.push_str(&format!(
        "- Judge calls: {}, accept rate {:.1}%\n",
        stats.judge_calls,
        percent(stats.judge_accepted, stats.judge_calls)
    ));
    out.push_str(&format!(
        "- Cascade escalations: {}/{} ({:.1}%)\n",
        stats.cascade_escalations,
        stats.cascade_calls,
        percent(stats.cascade_escalations, stats.cascade_calls)
    ));
    out.push_str(&format!(
        "- Observed cost in window: ${:.6}\n\n",
        stats.observed_cost_usd
    ));

    out.push_str(
        "Direct cache-hit quality scores are not recorded on runtime spans yet, so this \
         report tunes only judge-reviewed decisions. `cybersin eval gate` remains the \
         independent quality regression gate.\n",
    );

    out
}

fn percent(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

/// Parse a trailing-window spec (`7d`, `24h`, `30m`, `90s`) into a unix-ms
/// cutoff, i.e. "now minus this much". No `chrono`/`time` dependency for
/// four unit suffixes — same call the rest of this workspace makes (see
/// `cybersin-trace::store::day_bucket_to_iso_date`).
fn parse_since(spec: &str) -> Result<i64, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("error: --since must not be empty".into());
    }
    let (digits, unit) = spec.split_at(spec.len() - 1);
    let amount: u64 = digits.parse().map_err(|_| {
        format!("error: invalid --since {spec:?}, expected e.g. \"7d\", \"24h\", \"30m\"")
    })?;
    let seconds = match unit {
        "d" => amount * 86_400,
        "h" => amount * 3_600,
        "m" => amount * 60,
        "s" => amount,
        other => {
            return Err(format!(
                "error: invalid --since unit {other:?}, expected d/h/m/s"
            ))
        }
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("error: system clock before unix epoch: {e}"))?
        .as_millis() as i64;
    Ok(now_ms - (seconds as i64) * 1000)
}

/// Parse a portable JSONL trace export (one `Span` per line) — the
/// `--traces file.jsonl` alternative to reading the daemon's live trace
/// store, for CI/portability (spec §9).
fn load_jsonl(path: &std::path::Path) -> Result<Vec<Span>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("error: failed to read {}: {e}", path.display()))?;
    let mut spans = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let span: Span = serde_json::from_str(line).map_err(|e| {
            format!(
                "error: {}:{}: invalid span JSON: {e}",
                path.display(),
                i + 1
            )
        })?;
        spans.push(span);
    }
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cybersin_trace::SpanStatus;

    fn cache_decision_span(id: &str, decision: &str, similarity: f64) -> Span {
        Span {
            id: id.to_string(),
            session_id: "sess-1".to_string(),
            agent_name: "agent-a".to_string(),
            kind: SpanKind::CacheDecision,
            name: "hello".to_string(),
            start_unix_ms: 1_700_000_000_000,
            end_unix_ms: 1_700_000_000_100,
            model: None,
            tokens_prompt: None,
            tokens_completion: None,
            usd_cost: 0.0,
            cache_status: if decision == "judge_hit" {
                CacheStatus::Hit
            } else {
                CacheStatus::Miss
            },
            retries: 0,
            evicted_sections: vec![],
            status: SpanStatus::Ok,
            attributes: serde_json::json!({"decision": decision, "similarity": similarity}),
        }
    }

    #[test]
    fn no_changes_when_data_is_sparse() {
        let spans = vec![cache_decision_span("s1", "judge_hit", 0.96)];
        let analysis = analyze(&spans, 0.97, [0.90, 0.97]);
        assert!(analysis.changes.is_empty());
        assert!(analysis.observed_stats().is_none());
    }

    #[test]
    fn lowers_threshold_when_top_bucket_is_reliably_accepted() {
        // Band [0.90, 0.97] split into 5 buckets of width 0.014; the top
        // bucket is [0.956, 0.97). Put MIN_BUCKET_SAMPLES accepts there.
        let mut spans = Vec::new();
        for i in 0..MIN_BUCKET_SAMPLES {
            spans.push(cache_decision_span(&format!("s{i}"), "judge_hit", 0.965));
        }
        let analysis = analyze(&spans, 0.97, [0.90, 0.97]);
        assert_eq!(analysis.changes.len(), 1);
        let stats = analysis.observed_stats().expect("stats");
        assert!(stats.cache_similarity_threshold.unwrap() < 0.97);
        assert!((stats.cache_similarity_threshold.unwrap() - 0.956).abs() < 1e-9);
    }

    #[test]
    fn raises_band_floor_when_bottom_bucket_is_reliably_rejected() {
        // Bottom bucket is [0.90, 0.914).
        let mut spans = Vec::new();
        for i in 0..MIN_BUCKET_SAMPLES {
            spans.push(cache_decision_span(&format!("s{i}"), "judge_reject", 0.905));
        }
        let analysis = analyze(&spans, 0.97, [0.90, 0.97]);
        assert_eq!(analysis.changes.len(), 1);
        let stats = analysis.observed_stats().expect("stats");
        assert!(stats.cache_similarity_threshold.is_none());
        let band = stats.judge_trigger_band.unwrap();
        assert!(band[0] > 0.90);
        assert_eq!(band[1], 0.97);
    }

    #[test]
    fn ignores_direct_hits_and_bypasses_with_no_similarity_verdict() {
        let spans = vec![
            cache_decision_span("s1", "hash_hit", 1.0),
            cache_decision_span("s2", "knn_hit", 0.99),
        ];
        let analysis = analyze(&spans, 0.97, [0.90, 0.97]);
        assert_eq!(analysis.stats.judge_calls, 0);
        assert!(analysis.changes.is_empty());
    }

    #[test]
    fn since_parses_day_hour_minute_second_suffixes() {
        assert!(parse_since("7d").unwrap() < parse_since("1h").unwrap());
        assert!(parse_since("garbage").is_err());
        assert!(parse_since("7x").is_err());
    }
}
