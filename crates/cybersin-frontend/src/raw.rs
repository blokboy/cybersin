//! The as-written shape of a `*.prompt.yaml` source (spec §5.1), after
//! `!include` resolution but before typechecking or IR emission.
//!
//! Note on `inputs:` syntax: the spec's flow-mapping example —
//! `inputs: { topic: string, depth: enum[quick, thorough] }` — is not
//! valid strict YAML: inside a flow mapping (`{ }`), plain scalars may not
//! contain the flow indicator characters `,`, `[`, `]` unescaped, so
//! `enum[quick, thorough]` there needs quoting. This frontend accepts the
//! type grammar in either the block form (idiomatic, no quoting needed —
//! what `cybersin init` and `cybersin fmt` emit) or the flow form with the
//! value quoted; both deserialize to the same `String` here and are parsed
//! by [`crate::types::parse_input_type`].

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawSource {
    pub name: String,
    pub quality: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    #[serde(default)]
    pub tools: Vec<String>,
    pub sections: Vec<RawSection>,
    #[serde(default)]
    pub output_contract: Option<RawOutputContract>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawSection {
    pub id: String,
    pub priority: u32,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawOutputContract {
    #[serde(rename = "type")]
    pub contract_type: String,
    pub schema: String,
}
