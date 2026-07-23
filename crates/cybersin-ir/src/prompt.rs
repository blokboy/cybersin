//! Prompt IR: the compiled representation of a `*.prompt.yaml` source
//! (spec §5.1, §6.1).
//!
//! The frontend resolves the `!include` graph and typechecks inputs before
//! emitting this shape, so everything here is already fully resolved —
//! there are no unresolved includes or unvalidated types left to chase
//! downstream. Optimizer passes (`cybersin-passes`) consume and produce
//! this same shape (IR → IR), and backends render it per model family.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The current IR schema generation. Bumped when the shape changes in a
/// way that isn't purely additive. IR v1 is a direct descendant of IR
/// schema v0 (spec §6.1).
pub const IR_VERSION: u32 = 1;

/// A fully-resolved, typechecked prompt, ready for the optimizer pipeline.
///
/// This is the shared contract described in spec §6.6: the compiler
/// writes it into `dist/prompts/`, and the runtime's context assembler
/// (§8.3a) reads it back with the same serde definitions on both sides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptIr {
    /// Schema generation this document was emitted under.
    pub ir_version: u32,
    /// Prompt name, e.g. `researcher`. Referenced by `llm.request` at
    /// runtime (spec §10) — the adapter protocol names a prompt, never a
    /// model.
    pub name: String,
    /// Requested quality tier, used by the router (§6.3) to pick a
    /// cascade starting point.
    pub quality: QualityTier,
    /// Typed inputs, validated at build time and at every runtime render
    /// (§5.1). Keyed by input name; `BTreeMap` keeps serialized output
    /// deterministic for byte-identical builds (§7).
    pub inputs: BTreeMap<String, InputType>,
    /// Tool names available to this prompt. Full tool policy (class,
    /// guards, approval) is agent-level config (§5.3), not part of the
    /// prompt IR.
    pub tools: Vec<String>,
    /// Sections in source order. Priority governs eviction order under a
    /// budget plan, cache-key granularity, and provider prefix-cache
    /// alignment (§5.1).
    pub sections: Vec<Section>,
    /// Optional structured-output contract.
    pub output_contract: Option<OutputContract>,
}

impl PromptIr {
    /// Convenience constructor stamping the current `IR_VERSION`.
    pub fn new(
        name: impl Into<String>,
        quality: QualityTier,
        inputs: BTreeMap<String, InputType>,
        tools: Vec<String>,
        sections: Vec<Section>,
        output_contract: Option<OutputContract>,
    ) -> Self {
        Self {
            ir_version: IR_VERSION,
            name: name.into(),
            quality,
            inputs,
            tools,
            sections,
            output_contract,
        }
    }
}

/// Quality tier requested for a prompt (spec §5.1's `quality: high`),
/// consumed by the router as a cascade starting point (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityTier {
    Low,
    Medium,
    High,
}

/// A typed prompt input (spec §5.1: `{ topic: string, depth: enum[quick,
/// thorough], documents: list[document] }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputType {
    String,
    Number,
    Bool,
    /// A retrieved/attached document (spec §5.1's `document` type).
    Document,
    /// A closed set of string variants, e.g. `enum[quick, thorough]`.
    Enum {
        variants: Vec<String>,
    },
    /// A homogeneous list of another input type, e.g. `list[document]`.
    List {
        of: Box<InputType>,
    },
}

/// One section of a prompt: the unit of budget eviction, cache-key
/// granularity, and provider prefix-cache alignment (spec §5.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// Stable identifier, referenced by budget plans (`budget::EvictionStep`)
    /// and dead-section lint (§6.2).
    pub id: String,
    /// Higher priority sections are kept longer under budget pressure and
    /// ordered earlier for prefix-cache alignment.
    pub priority: u32,
    /// Fully-resolved body text — `!include` targets already inlined by
    /// the frontend.
    pub body: String,
}

/// Structured-output contract (spec §5.1: `{ type: json_schema, schema:
/// !include ... }`). The schema is carried as its already-resolved raw
/// text rather than a parsed JSON value, keeping this crate's only
/// dependency on `serde` (spec §13's dependency discipline) — downstream
/// consumers that need a parsed schema decode it themselves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputContract {
    #[serde(rename = "type")]
    pub contract_type: String,
    pub schema: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_prompt_ir() -> PromptIr {
        let mut inputs = BTreeMap::new();
        inputs.insert("topic".to_string(), InputType::String);
        inputs.insert(
            "depth".to_string(),
            InputType::Enum {
                variants: vec!["quick".to_string(), "thorough".to_string()],
            },
        );
        inputs.insert(
            "documents".to_string(),
            InputType::List {
                of: Box::new(InputType::Document),
            },
        );

        PromptIr::new(
            "researcher",
            QualityTier::High,
            inputs,
            vec!["web_search".to_string(), "web_fetch".to_string()],
            vec![
                Section {
                    id: "role".to_string(),
                    priority: 100,
                    body: "You are a research analyst...".to_string(),
                },
                Section {
                    id: "instructions".to_string(),
                    priority: 90,
                    body: "Resolved fragment body.".to_string(),
                },
                Section {
                    id: "documents".to_string(),
                    priority: 50,
                    body: "{{#each documents}}...{{/each}}".to_string(),
                },
            ],
            Some(OutputContract {
                contract_type: "json_schema".to_string(),
                schema: r#"{"type":"object"}"#.to_string(),
            }),
        )
    }

    #[test]
    fn prompt_ir_round_trips_through_json() {
        let original = sample_prompt_ir();
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: PromptIr = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }

    #[test]
    fn prompt_ir_without_output_contract_round_trips() {
        let mut original = sample_prompt_ir();
        original.output_contract = None;
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: PromptIr = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }

    #[test]
    fn quality_tier_round_trips() {
        for tier in [QualityTier::Low, QualityTier::Medium, QualityTier::High] {
            let json = serde_json::to_string(&tier).expect("serialize");
            let restored: QualityTier = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(tier, restored);
        }
    }

    #[test]
    fn input_type_variants_round_trip() {
        let variants = vec![
            InputType::String,
            InputType::Number,
            InputType::Bool,
            InputType::Document,
            InputType::Enum {
                variants: vec!["quick".to_string(), "thorough".to_string()],
            },
            InputType::List {
                of: Box::new(InputType::List {
                    of: Box::new(InputType::String),
                }),
            },
        ];
        for original in variants {
            let json = serde_json::to_string(&original).expect("serialize");
            let restored: InputType = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, restored);
        }
    }

    #[test]
    fn section_round_trips() {
        let original = Section {
            id: "role".to_string(),
            priority: 100,
            body: "You are a research analyst...".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: Section = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }

    #[test]
    fn output_contract_round_trips() {
        let original = OutputContract {
            contract_type: "json_schema".to_string(),
            schema: r#"{"type":"object","properties":{}}"#.to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        assert!(json.contains("\"type\":\"json_schema\""));
        let restored: OutputContract = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }
}
