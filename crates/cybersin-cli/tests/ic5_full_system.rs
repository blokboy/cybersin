//! IC-5 full-system checkpoint (issue #25).
//!
//! The focused feature suites prove eval, explain, optimize, and storage
//! independently. These scenarios deliberately use the committed IC-1
//! research-team project so the final integration seam is exercised too.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use async_trait::async_trait;
use cybersin_router::RouteModel;
use cybersin_runtime::{
    default_model, CacheEntry, DaemonHandle, DistFixture, ExecutionRequest, Judge, ModelCaller,
    ModelOutput, RouteExecutor,
};
use serde_json::{json, Value};

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").expect("find cybersin binary")
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ic1-research-team")
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct UnusedModel;

#[async_trait]
impl ModelCaller for UnusedModel {
    async fn call(
        &self,
        _model: &RouteModel,
        _prompt_name: &str,
        _inputs: &Value,
    ) -> Result<ModelOutput, String> {
        Err("the accepted cache candidate should avoid a provider call".into())
    }
}

struct AcceptingJudge;

#[async_trait]
impl Judge for AcceptingJudge {
    async fn accepts(
        &self,
        _model: &RouteModel,
        _prompt_name: &str,
        _inputs: &Value,
        _cached_response: &Value,
        _similarity: f64,
    ) -> Result<bool, String> {
        Ok(true)
    }
}

#[test]
fn real_sample_eval_gate_catches_a_seeded_regression() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("ic1-research-team");
    copy_tree(&fixture(), &project);

    cybersin()
        .args(["eval", "gate"])
        .arg(&project)
        .assert()
        .success()
        .stdout(predicates::str::contains("runs=3"))
        .stdout(predicates::str::contains("PASS"));

    let eval_path = project.join("evals/researcher.eval.yaml");
    let passing = fs::read_to_string(&eval_path).unwrap();
    let regressed = passing.replacen("judge_score: 0.92", "judge_score: 0.20", 1);
    assert_ne!(
        passing, regressed,
        "the regression seed must change a sample"
    );
    fs::write(&eval_path, regressed).unwrap();

    cybersin()
        .args(["eval", "gate"])
        .arg(&project)
        .assert()
        .failure()
        .stdout(predicates::str::contains("FAIL"))
        .stderr(predicates::str::contains("eval gate failed"));
}

#[tokio::test]
async fn real_sample_run_explain_and_optimize_share_observed_data() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("ic1-research-team");
    let db = temp.path().join("ic5.db");
    copy_tree(&fixture(), &project);

    // Produce observed LLM cost through the real IC-1 dist/runtime path.
    cybersin()
        .arg("--db")
        .arg(&db)
        .args(["run", "--stub", "--dist"])
        .arg(project.join("dist"))
        .args(["--session-id", "ic5-observed", "--agent", "research-team"])
        .assert()
        .success()
        // The researcher prompt's real compiled cascade (issue #33) now
        // genuinely escalates cheapest-first before settling on
        // premium-high: 2 cache-decision + 4 llm-call (3 real cascade
        // attempts for the miss + 1 hit) + 1 tool-call spans.
        .stdout(predicates::str::contains("7 spans recorded"));

    // Accumulate judge-reviewed cache decisions through the real route
    // executor and the real IC-1 routing artifact. Similarity 0.96 lands
    // in the top [0.956, 0.970] evidence bucket.
    let daemon = DaemonHandle::auto_start(&db).await.unwrap();
    let mut executor = RouteExecutor::load_dir(
        project.join("dist"),
        UnusedModel,
        AcceptingJudge,
        daemon.spans(),
    )
    .unwrap();
    executor.upsert_cache(CacheEntry {
        prompt_name: "researcher".into(),
        input_hash: "semantic-seed".into(),
        embedding: vec![0.96, 0.28],
        response: json!({"summary": "cached evidence"}),
    });
    // A cache hit is attributed to the prompt's default (highest-quality)
    // model for span/cost labeling — a cache entry doesn't record which
    // model originally produced it (see `route_executor::default_model`).
    let researcher_default_model = DistFixture::load_dir(project.join("dist"))
        .unwrap()
        .routing_artifact
        .prompts
        .get("researcher")
        .and_then(default_model)
        .cloned();
    for sample in 0..20 {
        let response = executor
            .execute(&ExecutionRequest {
                session_id: "ic5-pgo".into(),
                agent_name: "research-team".into(),
                prompt_name: "researcher".into(),
                inputs: json!({
                    "topic": format!("cybernetics sample {sample}"),
                    "depth": "thorough",
                    "documents": [],
                }),
                embedding: vec![1.0, 0.0],
                namespace_version: "1".into(),
                bypass: false,
                prompt_tokens: 0,
                completion_tokens: None,
                evicted_sections: Vec::new(),
                context_attributes: Value::Null,
                force_cheapest_cascade_step: false,
                default_model: researcher_default_model.clone(),
            })
            .await
            .unwrap();
        assert!(response.cache_hit);
    }
    drop(executor);
    drop(daemon);

    let explain = cybersin()
        .arg("--db")
        .arg(&db)
        .arg("explain")
        .arg("researcher")
        .arg(&project)
        .arg("--plain")
        .output()
        .unwrap();
    assert!(
        explain.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&explain.stderr)
    );
    let explain = String::from_utf8(explain.stdout).unwrap();
    for expected in [
        "generic (total 36)",
        "role                         10",
        "assignment                   14",
        "documents                    12",
        "openai (total 39)",
        "cache ≥ 0.97; judge 0.90..0.97",
        "premium-high",
        "Estimated: $5.200000 per routed call",
        // 4 real llm-call spans from the "ic5-observed" session (the
        // real cascade's 3 escalation attempts + 1 cache hit, issue #33)
        // plus 20 cache-hit spans from the PGO-seeding loop below, which
        // also now records a real `llm_call` span per hit — a genuine
        // prompt invocation, spec §8.5's "span per LLM call... cache
        // status", even though no model was actually called.
        "Observed: $5.200000 across 24 LLM calls",
        "ic5-observed  completed  research-team",
    ] {
        assert!(
            explain.contains(expected),
            "missing {expected:?} in explain output:\n{explain}"
        );
    }

    cybersin()
        .arg("--db")
        .arg(&db)
        .arg("optimize")
        .arg(&project)
        .args(["--profile", "release", "--frozen"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "cache_similarity_threshold: 0.9700 -> 0.9560 (lowered)",
        ));

    let report = fs::read_to_string(project.join("optimize-report.md")).unwrap();
    for expected in [
        // The real cascade's escalation spans (issue #33) and the PGO
        // loop's now-genuine per-hit `llm_call` spans both land in the
        // trace store, so the window covers more real spans and real
        // accumulated cost than before.
        "Spans considered: 47",
        "cache_similarity_threshold: 0.9700 -> 0.9560 (lowered)",
        "100.0% of judge calls",
        "Judge calls: 20, accept rate 100.0%",
        "Observed cost in window: $5.200800",
        "`cybersin eval gate` remains the independent quality regression gate",
    ] {
        assert!(
            report.contains(expected),
            "missing {expected:?} in optimize report:\n{report}"
        );
    }

    let routing: Value =
        serde_json::from_slice(&fs::read(project.join("dist/routing.json")).unwrap()).unwrap();
    let cache = &routing["prompts"]["researcher"]["decisions"][0];
    assert_eq!(cache["similarity_threshold"], json!(0.956));
    assert_eq!(cache["judge_trigger_band"], json!([0.9, 0.956]));
}
