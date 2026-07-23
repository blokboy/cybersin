//! Pipeline-composition + end-to-end pipeline tests (spec §6.2's
//! acceptance criteria for issues #4 and #6): the pass plan is data per
//! build profile, and running the real (non-`compress`) passes
//! together over one IR produces their composed result.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cybersin_ir::{PromptIr, QualityTier, Section};
use cybersin_passes::{
    build_pipeline, build_pipeline_with_compress, input_hash, profile_plan, run_pipeline, Compress,
    CompressError, CompressMode, CompressionLock, CompressionProvider, PassSlot, Profile,
};

fn section(id: &str, priority: u32, body: &str) -> Section {
    Section {
        id: id.to_string(),
        priority,
        body: body.to_string(),
        dedup_ref: None,
    }
}

// ---------------------------------------------------------------------
// `--profile dev` skips compression by construction: proven at the
// pipeline-composition level (`profile_plan`, the data itself), not by
// faking a `compress` `Pass` impl that doesn't exist yet (issue #5).
// ---------------------------------------------------------------------

#[test]
fn dev_profile_plan_never_lists_compress() {
    let plan = profile_plan(Profile::Dev);
    assert!(
        !plan.contains(&PassSlot::Compress),
        "dev profile's pass plan must not include a compression slot, got {plan:?}"
    );
    assert_eq!(
        plan,
        vec![
            PassSlot::LintFast,
            PassSlot::Dedupe,
            PassSlot::Reorder,
            PassSlot::Budget,
            PassSlot::Lint,
        ]
    );
}

#[test]
fn release_profile_plan_lists_compress() {
    // Release's *intent* includes compression (it's opt-in per spec
    // §6.2, but a release build wants it): the plan says so as data,
    // which is exactly the contrast that makes dev's omission meaningful
    // rather than accidental.
    let plan = profile_plan(Profile::Release);
    assert!(plan.contains(&PassSlot::Compress));
}

#[test]
fn build_pipeline_for_dev_runs_structural_and_budget_passes_in_spec_order() {
    let pipeline = build_pipeline(Profile::Dev);
    let names: Vec<&str> = pipeline.iter().map(|p| p.name()).collect();
    assert_eq!(
        names,
        vec!["lint-fast", "dedupe", "reorder", "budget", "lint"]
    );
}

#[test]
fn structural_only_pipeline_skips_stateful_compress() {
    // Callers without provider/lockfile state can explicitly request the
    // structural-only pipeline. Real release builds use
    // `build_pipeline_with_compress`.
    let pipeline = build_pipeline(Profile::Release);
    let names: Vec<&str> = pipeline.iter().map(|p| p.name()).collect();
    assert_eq!(
        names,
        vec!["lint-fast", "dedupe", "reorder", "budget", "lint"]
    );
    assert!(!names.contains(&"compress"));
}

struct FixtureProvider {
    calls: AtomicUsize,
}

impl CompressionProvider for FixtureProvider {
    fn model(&self) -> &str {
        "recorded/fixture-v1"
    }

    fn compress(&self, input: &str) -> Result<String, CompressError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/compress.json")).unwrap();
        assert_eq!(fixture["input"], input);
        Ok(fixture["output"].as_str().unwrap().to_string())
    }
}

#[test]
fn compress_reduces_tokens_preserves_fixture_behavior_and_pins_by_input_hash() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/compress.json")).unwrap();
    let input = fixture["input"].as_str().unwrap();
    let expected_behavior = fixture["behavior"].as_array().unwrap();
    let provider = Arc::new(FixtureProvider {
        calls: AtomicUsize::new(0),
    });
    let lock = Arc::new(Mutex::new(CompressionLock::default()));
    let pass = Compress::new(provider.clone(), lock.clone(), CompressMode::Update);
    let ir = PromptIr::new(
        "support",
        QualityTier::High,
        BTreeMap::new(),
        vec![],
        vec![section("instructions", 100, input)],
        None,
    );

    let outcome = run_pipeline(&[Box::new(pass)], ir, None);
    assert!(!outcome.has_error(), "{:?}", outcome.diagnostics);
    let output = &outcome.ir.sections[0].body;
    assert!(output.split_whitespace().count() < input.split_whitespace().count());
    for behavior in expected_behavior {
        assert!(
            output.contains(behavior.as_str().unwrap()),
            "compressed output lost fixture behavior `{behavior}`"
        );
    }
    let key = input_hash(input);
    let pinned = lock.lock().unwrap().compress.get(&key).cloned().unwrap();
    assert_eq!(pinned.output, *output);
    assert_eq!(pinned.model, "recorded/fixture-v1");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn frozen_compress_uses_pin_without_provider_and_fails_on_cache_miss() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/compress.json")).unwrap();
    let input = fixture["input"].as_str().unwrap();
    let provider = Arc::new(FixtureProvider {
        calls: AtomicUsize::new(0),
    });
    let lock = Arc::new(Mutex::new(CompressionLock::default()));
    let ir = PromptIr::new(
        "support",
        QualityTier::High,
        BTreeMap::new(),
        vec![],
        vec![section("instructions", 100, input)],
        None,
    );
    let pass = Compress::new(provider.clone(), lock.clone(), CompressMode::Frozen);
    let miss = run_pipeline(&[Box::new(pass.clone())], ir.clone(), None);
    assert!(miss.has_error());
    assert!(miss.diagnostics[0]
        .message
        .contains("would require a network call"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

    lock.lock().unwrap().compress.insert(
        input_hash(input),
        cybersin_passes::LockedCompression {
            model: "recorded/fixture-v1".into(),
            output: fixture["output"].as_str().unwrap().into(),
        },
    );
    let hit = run_pipeline(&[Box::new(pass)], ir, None);
    assert!(!hit.has_error());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn release_pipeline_runs_compress_while_dev_omits_it() {
    let provider = Arc::new(FixtureProvider {
        calls: AtomicUsize::new(0),
    });
    let compress = Compress::new(
        provider,
        Arc::new(Mutex::new(CompressionLock::default())),
        CompressMode::Update,
    );
    let release_names: Vec<_> = build_pipeline_with_compress(Profile::Release, compress.clone())
        .iter()
        .map(|pass| pass.name())
        .collect();
    let dev_names: Vec<_> = build_pipeline_with_compress(Profile::Dev, compress)
        .iter()
        .map(|pass| pass.name())
        .collect();
    assert!(release_names.contains(&"compress"));
    assert!(!dev_names.contains(&"compress"));
}

// ---------------------------------------------------------------------
// End-to-end: running the real dev pipeline over one IR composes all
// four passes' effects (dedupe collapses a shared fragment, reorder
// puts the now-empty deduped/stable sections first, lint-fast/lint stay
// clean).
// ---------------------------------------------------------------------

#[test]
fn dev_pipeline_composes_dedupe_and_reorder_and_stays_clean() {
    let mut inputs = BTreeMap::new();
    inputs.insert("topic".to_string(), cybersin_ir::InputType::String);

    let ir = PromptIr::new(
        "researcher",
        QualityTier::High,
        inputs,
        vec![],
        vec![
            section("greeting", 95, "Hello, {{ topic }}."),
            section("role", 100, "Follow the house safety policy."),
            section("system_reminder", 10, "Follow the house safety policy."),
        ],
        None,
    );

    let pipeline = build_pipeline(Profile::Dev);
    let outcome = run_pipeline(&pipeline, ir, None);

    assert!(
        !outcome.has_error(),
        "expected a clean run, got {:?}",
        outcome.diagnostics
    );

    // dedupe: "system_reminder" collapsed onto "role".
    let system_reminder = outcome
        .ir
        .sections
        .iter()
        .find(|s| s.id == "system_reminder")
        .unwrap();
    assert_eq!(system_reminder.body, "");
    assert_eq!(system_reminder.dedup_ref, Some("role".to_string()));

    // reorder: stable sections ("role", now-empty "system_reminder")
    // ahead of the dynamic "greeting" — deduped-away content is static
    // (empty) text, so it sorts as stable too.
    let ids: Vec<&str> = outcome.ir.sections.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["role", "system_reminder", "greeting"]);
}

#[test]
fn dev_pipeline_halts_before_dedupe_when_lint_fast_finds_an_unused_input() {
    let mut inputs = BTreeMap::new();
    inputs.insert("topic".to_string(), cybersin_ir::InputType::String);
    inputs.insert("unused".to_string(), cybersin_ir::InputType::String);

    let ir = PromptIr::new(
        "researcher",
        QualityTier::High,
        inputs,
        vec![],
        vec![
            section("role", 100, "Follow the house safety policy."),
            section("system_reminder", 10, "Follow the house safety policy."),
        ],
        None,
    );

    let pipeline = build_pipeline(Profile::Dev);
    let outcome = run_pipeline(&pipeline, ir, None);

    assert!(outcome.has_error());
    // dedupe never ran: the duplicate "system_reminder" body is
    // untouched, proving the pipeline stopped after lint-fast rather
    // than continuing to a later (in a real build, potentially paid)
    // pass.
    let system_reminder = outcome
        .ir
        .sections
        .iter()
        .find(|s| s.id == "system_reminder")
        .unwrap();
    assert_eq!(system_reminder.dedup_ref, None);
    assert_eq!(system_reminder.body, "Follow the house safety policy.");
}
