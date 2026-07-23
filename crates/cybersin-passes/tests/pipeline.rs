//! Pipeline-composition + end-to-end pipeline tests (spec §6.2's
//! acceptance criteria for issue #4): the pass plan is data per build
//! profile, and running the real (non-`compress`, non-`budget`) passes
//! together over one IR produces the composed result of all four.

use std::collections::BTreeMap;

use cybersin_ir::{PromptIr, QualityTier, Section};
use cybersin_passes::{build_pipeline, profile_plan, run_pipeline, PassSlot, Profile};

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
            PassSlot::Lint,
        ]
    );
}

#[test]
fn release_profile_plan_does_list_compress_as_data_even_though_unimplemented() {
    // Release's *intent* includes compression (it's opt-in per spec
    // §6.2, but a release build wants it): the plan says so as data,
    // which is exactly the contrast that makes dev's omission meaningful
    // rather than accidental.
    let plan = profile_plan(Profile::Release);
    assert!(plan.contains(&PassSlot::Compress));
}

#[test]
fn build_pipeline_for_dev_runs_exactly_the_four_structural_passes_in_spec_order() {
    let pipeline = build_pipeline(Profile::Dev);
    let names: Vec<&str> = pipeline.iter().map(|p| p.name()).collect();
    assert_eq!(names, vec!["lint-fast", "dedupe", "reorder", "lint"]);
}

#[test]
fn build_pipeline_for_release_skips_compress_since_no_impl_exists_yet_but_keeps_the_rest() {
    // `profile_plan(Release)` wants `compress`; `build_pipeline` can
    // only resolve slots it has a `Pass` impl for, so the *runnable*
    // pipeline for release today is identical to dev's. Once issue #5
    // lands, only `build_pipeline`'s match arm changes — `profile_plan`
    // already asked for it.
    let pipeline = build_pipeline(Profile::Release);
    let names: Vec<&str> = pipeline.iter().map(|p| p.name()).collect();
    assert_eq!(names, vec!["lint-fast", "dedupe", "reorder", "lint"]);
    assert!(!names.contains(&"compress"));
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
