//! `lint-fast` (spec §6.2): structural checks answerable straight off
//! the IR, with no dependency on any other pass's output. Runs first in
//! every profile so a broken prompt fails before `dedupe`/`compress`
//! spend any work on it.
//!
//! Today this is one check: an input declared in `PromptIr::inputs` but
//! never referenced by any section body. The frontend already rejects
//! this at parse time (`cybersin-frontend`'s `typecheck` module) for
//! sources it compiles — this pass exists because the optimizer's
//! contract is IR→IR, not "IR that definitely came from this frontend",
//! and re-deriving the check from `Section::body` text keeps that
//! contract honest without `cybersin-passes` depending on
//! `cybersin-frontend` (spec §13: passes may depend on `cybersin-ir`,
//! never sideways on another compiler-stage crate).

use regex::Regex;

use cybersin_ir::PromptIr;

use crate::{Diagnostic, Pass, PassContext};

pub struct LintFast;

impl Pass for LintFast {
    fn name(&self) -> &'static str {
        "lint-fast"
    }

    fn run(&self, ctx: &mut PassContext) {
        for name in unused_inputs(&ctx.ir) {
            ctx.push(Diagnostic::error(
                self.name(),
                format!("input `{name}` is declared but never referenced by any section"),
            ));
        }
    }
}

fn unused_inputs(ir: &PromptIr) -> Vec<String> {
    ir.inputs
        .keys()
        .filter(|name| !is_referenced(ir, name))
        .cloned()
        .collect()
}

/// Whether `name` appears as a whole identifier anywhere in any section
/// body — covers both scalar interpolation (`{{ topic }}`) and loop
/// binding (`{% for item in documents %}`), since both put the input
/// name as its own token in the body text.
fn is_referenced(ir: &PromptIr, name: &str) -> bool {
    let re = Regex::new(&format!(r"\b{}\b", regex::escape(name))).expect("word-boundary pattern");
    ir.sections.iter().any(|s| re.is_match(&s.body))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cybersin_ir::{InputType, QualityTier, Section};

    use super::*;
    use crate::{PassContext, Severity};

    fn section(id: &str, priority: u32, body: &str) -> Section {
        Section {
            id: id.to_string(),
            priority,
            body: body.to_string(),
            dedup_ref: None,
        }
    }

    #[test]
    fn golden_flags_unused_input_before_any_other_pass_would_run() {
        let mut inputs = BTreeMap::new();
        inputs.insert("topic".to_string(), InputType::String);
        inputs.insert("unused_flag".to_string(), InputType::Bool);

        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            inputs,
            vec![],
            vec![section("role", 100, "About {{ topic }}")],
            None,
        );

        let mut ctx = PassContext::new(ir);
        LintFast.run(&mut ctx);

        assert_eq!(ctx.diagnostics.len(), 1);
        let d = &ctx.diagnostics[0];
        assert_eq!(d.pass, "lint-fast");
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("unused_flag"));
    }

    #[test]
    fn golden_input_referenced_only_as_a_loop_binding_counts_as_used() {
        let mut inputs = BTreeMap::new();
        inputs.insert(
            "documents".to_string(),
            InputType::List {
                of: Box::new(InputType::Document),
            },
        );

        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            inputs,
            vec![],
            vec![section(
                "docs",
                50,
                "{% for item in documents %}{{ item.title }}{% endfor %}",
            )],
            None,
        );

        let mut ctx = PassContext::new(ir);
        LintFast.run(&mut ctx);

        assert!(ctx.diagnostics.is_empty());
    }

    #[test]
    fn golden_all_inputs_used_is_clean() {
        let mut inputs = BTreeMap::new();
        inputs.insert("topic".to_string(), InputType::String);

        let ir = PromptIr::new(
            "researcher",
            QualityTier::High,
            inputs,
            vec![],
            vec![section("role", 100, "About {{ topic }}")],
            None,
        );

        let mut ctx = PassContext::new(ir);
        LintFast.run(&mut ctx);

        assert!(ctx.diagnostics.is_empty());
    }
}
