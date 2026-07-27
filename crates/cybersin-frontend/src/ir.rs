//! Assembles a typechecked [`RawSource`] into [`cybersin_ir::PromptIr`]
//! (spec §6.1).

use cybersin_ir::{OutputContract, PromptIr, QualityTier, Section};

use crate::error::FrontendError;
use crate::raw::RawSource;
use crate::template;
use crate::typecheck;
use crate::types;

pub(crate) fn build_ir(raw: RawSource) -> Result<PromptIr, FrontendError> {
    let quality = parse_quality(&raw.quality)?;

    let (declared_inputs, grounded_tiers) =
        typecheck::typecheck(&raw, quality).map_err(FrontendError::Typecheck)?;

    let output_contract = raw.output_contract.as_ref().map(|oc| OutputContract {
        contract_type: oc.contract_type.clone(),
        schema: oc.schema.clone(),
    });

    let mut sections = Vec::with_capacity(raw.sections.len());
    for s in &raw.sections {
        let translated = template::translate_handlebars_each(&s.body);
        template::validate_syntax(&translated).map_err(|source| FrontendError::Template {
            section: s.id.clone(),
            source,
        })?;
        sections.push(Section {
            id: s.id.clone(),
            priority: s.priority,
            body: translated,
            dedup_ref: None,
        });
    }

    Ok(PromptIr::new(
        raw.name,
        quality,
        declared_inputs,
        raw.tools,
        sections,
        output_contract,
    )
    .with_grounded_tiers(grounded_tiers))
}

fn parse_quality(raw: &str) -> Result<QualityTier, FrontendError> {
    types::parse_quality_tier(raw).ok_or_else(|| FrontendError::InvalidQuality {
        raw: raw.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{RawOutputContract, RawSection};
    use cybersin_ir::InputType;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn builds_ir_from_valid_source() {
        let mut inputs = BTreeMap::new();
        inputs.insert("topic".to_string(), "string".to_string());
        inputs.insert("documents".to_string(), "list[document]".to_string());

        let raw = RawSource {
            name: "researcher".to_string(),
            quality: "high".to_string(),
            inputs,
            tools: vec!["web_search".to_string()],
            grounded_tiers: vec![],
            sections: vec![
                RawSection {
                    id: "role".to_string(),
                    priority: 100,
                    body: "About {{ topic }}".to_string(),
                },
                RawSection {
                    id: "docs".to_string(),
                    priority: 50,
                    body: "{{#each documents}}{{this.title}}{{/each}}".to_string(),
                },
            ],
            output_contract: Some(RawOutputContract {
                contract_type: "json_schema".to_string(),
                schema: r#"{"type":"object"}"#.to_string(),
            }),
        };

        let ir = build_ir(raw).expect("should build IR");
        assert_eq!(ir.name, "researcher");
        assert_eq!(ir.quality, QualityTier::High);
        assert_eq!(ir.inputs.get("topic"), Some(&InputType::String));
        assert_eq!(
            ir.sections[1].body,
            "{% for item in documents %}{{ item.title}}{% endfor %}"
        );
        assert!(ir.output_contract.is_some());
        assert!(ir.grounded_tiers.is_empty());
    }

    #[test]
    fn rejects_bad_quality() {
        let raw = RawSource {
            name: "x".to_string(),
            quality: "ultra".to_string(),
            inputs: BTreeMap::new(),
            tools: vec![],
            grounded_tiers: vec![],
            sections: vec![RawSection {
                id: "role".to_string(),
                priority: 1,
                body: "hi".to_string(),
            }],
            output_contract: None,
        };
        let err = build_ir(raw).unwrap_err();
        assert!(matches!(err, FrontendError::InvalidQuality { .. }));
    }

    #[test]
    fn builds_ir_with_grounded_tiers() {
        let mut inputs = BTreeMap::new();
        inputs.insert("topic".to_string(), "string".to_string());

        let raw = RawSource {
            name: "researcher".to_string(),
            quality: "high".to_string(),
            inputs,
            tools: vec!["web_search".to_string()],
            grounded_tiers: vec!["medium".to_string(), "high".to_string()],
            sections: vec![RawSection {
                id: "role".to_string(),
                priority: 100,
                body: "About {{ topic }}".to_string(),
            }],
            output_contract: None,
        };

        let ir = build_ir(raw).expect("should build IR");
        assert_eq!(
            ir.grounded_tiers,
            BTreeSet::from([QualityTier::Medium, QualityTier::High])
        );
    }

    #[test]
    fn rejects_grounded_tier_above_own_quality() {
        let mut inputs = BTreeMap::new();
        inputs.insert("topic".to_string(), "string".to_string());

        let raw = RawSource {
            name: "researcher".to_string(),
            quality: "medium".to_string(),
            inputs,
            tools: vec![],
            grounded_tiers: vec!["high".to_string()],
            sections: vec![RawSection {
                id: "role".to_string(),
                priority: 100,
                body: "About {{ topic }}".to_string(),
            }],
            output_contract: None,
        };

        let err = build_ir(raw).unwrap_err();
        match err {
            FrontendError::Typecheck(issues) => {
                assert!(issues.iter().any(|i| matches!(
                    i,
                    crate::error::TypecheckIssue::GroundedTierAboveQuality { tier, quality }
                        if tier == "high" && quality == "medium"
                )));
            }
            other => panic!("expected Typecheck error, got: {other:?}"),
        }
    }
}
