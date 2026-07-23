//! `cybersin-ir`: the internal contract between the compiler and the
//! runtime (spec §13).
//!
//! These types are the only thing that crosses the compiler/runtime
//! boundary: the frontend emits [`prompt::PromptIr`] (§6.1), the `budget`
//! optimizer pass emits [`budget::BudgetArtifact`] (§6.2), both get
//! written into `dist/` as JSON (§6.6), and the runtime's route/cache
//! executor and context assembler (§8.3, §8.3a) read them back with the
//! same serde definitions — no cross-language contract, no codegen, no
//! version skew, because the compiler and runtime ship in one binary.
//!
//! Per spec §13's dependency discipline, this crate depends on nothing
//! but `serde`.

pub mod budget;
pub mod prompt;

pub use budget::{BudgetArtifact, BudgetPlan, EvictionStep};
pub use prompt::{InputType, OutputContract, PromptIr, QualityTier, Section, IR_VERSION};
