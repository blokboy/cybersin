//! Optimizer: structural IR→IR passes (spec §6.2).
//!
//! Each pass is a [`Pass`] impl over [`PromptIr`]; the pipeline itself is
//! data (`Vec<Box<dyn Pass>>` per build [`Profile`], built by
//! [`build_pipeline`]) rather than a branch inside any one pass — that's
//! what lets `--profile dev` skip the model-assisted
//! `compress` pass *by construction*: [`profile_plan`] simply never lists
//! it for [`Profile::Dev`]. `budget` (issue #6) isn't implemented here;
//! [`PassContext::budget`] is the seam `lint`
//! needs to consume `budget`'s output once it exists, without this crate
//! depending on it existing yet.

mod compress;
mod dedupe;
mod lint;
mod lint_fast;
mod reorder;

pub use compress::{
    input_hash, Compress, CompressError, CompressMode, CompressionLock, CompressionProvider,
    LockedCompression,
};
pub use dedupe::Dedupe;
pub use lint::Lint;
pub use lint_fast::LintFast;
pub use reorder::Reorder;

use cybersin_ir::{BudgetArtifact, PromptIr};

/// Diagnostic severity. `Error` halts the pipeline before any later
/// (potentially paid) pass runs; `Warning` is advisory and never blocks
/// the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One finding from a pass: which pass raised it, how serious it is, and
/// a human-readable message. Deliberately flat (no structured payload) —
/// nothing downstream consumes these programmatically yet, only prints
/// them (`cybersin check`, a later CLI issue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub pass: &'static str,
    pub severity: Severity,
    pub message: String,
}

impl Diagnostic {
    pub fn error(pass: &'static str, message: impl Into<String>) -> Self {
        Self {
            pass,
            severity: Severity::Error,
            message: message.into(),
        }
    }

    pub fn warning(pass: &'static str, message: impl Into<String>) -> Self {
        Self {
            pass,
            severity: Severity::Warning,
            message: message.into(),
        }
    }
}

/// Shared state threaded through the pipeline: the IR each pass reads
/// and (for `dedupe`/`reorder`) rewrites, plus whatever a pass wants to
/// report.
///
/// `budget` is `None` until the `budget` pass (issue #6) has populated
/// it for this build. `lint`'s dead-section check is the only consumer
/// today; it treats absence as "nothing to check yet" rather than an
/// error, since `lint` runs in every profile (spec §6.2) including ones
/// where `budget` hasn't run.
pub struct PassContext {
    pub ir: PromptIr,
    pub budget: Option<BudgetArtifact>,
    pub diagnostics: Vec<Diagnostic>,
}

impl PassContext {
    pub fn new(ir: PromptIr) -> Self {
        Self {
            ir,
            budget: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn with_budget(mut self, budget: BudgetArtifact) -> Self {
        self.budget = Some(budget);
        self
    }

    pub(crate) fn push(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn has_error(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// A structural optimizer pass (spec §6.2). Passes either rewrite
/// `ctx.ir` in place (`dedupe`, `reorder`) or only inspect it and record
/// [`Diagnostic`]s (`lint-fast`, `lint`) — one trait for both because the
/// pipeline runs every pass the same way, and nothing about the shape
/// rules out a future pass doing both.
pub trait Pass {
    /// Stable identifier used in diagnostics and golden-test output,
    /// e.g. `"dedupe"`.
    fn name(&self) -> &'static str;

    fn run(&self, ctx: &mut PassContext);
}

/// Result of running a pipeline to completion (or to its first `Error`).
pub struct PipelineOutcome {
    pub ir: PromptIr,
    pub diagnostics: Vec<Diagnostic>,
}

impl PipelineOutcome {
    pub fn has_error(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// Run `pipeline` over `ir` in order, stopping before any pass that
/// would follow one that raised an `Error` — this is what makes
/// `lint-fast` running first meaningful (spec §6.2): a broken prompt
/// never reaches a later, potentially paid, pass.
pub fn run_pipeline(
    pipeline: &[Box<dyn Pass>],
    ir: PromptIr,
    budget: Option<BudgetArtifact>,
) -> PipelineOutcome {
    let mut ctx = PassContext {
        ir,
        budget,
        diagnostics: Vec::new(),
    };
    for pass in pipeline {
        pass.run(&mut ctx);
        if ctx.has_error() {
            break;
        }
    }
    PipelineOutcome {
        ir: ctx.ir,
        diagnostics: ctx.diagnostics,
    }
}

/// Build profile: which structural/paid passes a build wants (spec
/// §6.2). Only `Dev`/`Release` exist today; a profile is a request for
/// pass slots, independent of whether this crate has an impl for every
/// slot yet (see [`build_pipeline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Fast local loop: structural passes only, no paid passes.
    Dev,
    /// Full build: paid passes included.
    Release,
}

/// One slot in a profile's pass plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassSlot {
    LintFast,
    Dedupe,
    Compress,
    Reorder,
    Lint,
}

/// The ordered pass plan for `profile` (spec §6.2's pass order:
/// lint-fast, dedupe, compress, reorder, lint — `budget` runs between
/// `reorder` and `lint` once issue #6 lands, outside this crate). This
/// is the "pipeline is data" seam itself: `Profile::Dev`'s plan simply
/// never contains `PassSlot::Compress`, so `--profile dev` skips
/// compression by construction, not by a pass or caller checking the
/// profile.
pub fn profile_plan(profile: Profile) -> Vec<PassSlot> {
    match profile {
        Profile::Dev => vec![
            PassSlot::LintFast,
            PassSlot::Dedupe,
            PassSlot::Reorder,
            PassSlot::Lint,
        ],
        Profile::Release => vec![
            PassSlot::LintFast,
            PassSlot::Dedupe,
            PassSlot::Compress,
            PassSlot::Reorder,
            PassSlot::Lint,
        ],
    }
}

/// Build only the stateless structural passes for `profile`. Build
/// orchestration that supports model assistance should call
/// [`build_pipeline_with_compress`] instead.
pub fn build_pipeline(profile: Profile) -> Vec<Box<dyn Pass>> {
    profile_plan(profile)
        .into_iter()
        .filter_map(|slot| -> Option<Box<dyn Pass>> {
            match slot {
                PassSlot::LintFast => Some(Box::new(LintFast)),
                PassSlot::Dedupe => Some(Box::new(Dedupe)),
                PassSlot::Reorder => Some(Box::new(Reorder)),
                PassSlot::Lint => Some(Box::new(Lint)),
                PassSlot::Compress => None,
            }
        })
        .collect()
}

/// Resolve a profile to a runnable pipeline, including stateful
/// compression for release builds. Dev omits it by composition.
pub fn build_pipeline_with_compress(profile: Profile, compress: Compress) -> Vec<Box<dyn Pass>> {
    profile_plan(profile)
        .into_iter()
        .map(|slot| -> Box<dyn Pass> {
            match slot {
                PassSlot::LintFast => Box::new(LintFast),
                PassSlot::Dedupe => Box::new(Dedupe),
                PassSlot::Compress => Box::new(compress.clone()),
                PassSlot::Reorder => Box::new(Reorder),
                PassSlot::Lint => Box::new(Lint),
            }
        })
        .collect()
}
