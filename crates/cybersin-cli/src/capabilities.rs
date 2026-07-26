//! Data-first capability metadata and execution events.
//!
//! The capability layer is intentionally separate from `commands::*`: command
//! modules are CLI adapters, while these types describe the shared product
//! surface that CLI and TUI adapters will eventually invoke.

use serde_json::Value;

/// A user-facing operation that can be invoked through one or more adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySpec {
    pub id: CapabilityId,
    pub title: String,
    pub summary: String,
    pub category: CapabilityCategory,
    pub input_schema: Value,
    pub output_modes: Vec<OutputMode>,
    pub safety: SafetyProfile,
    pub adapters: AdapterCoverage,
}

/// Stable, catalog-facing identifier for a capability.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CapabilityId(String);

impl CapabilityId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Broad area of the Cybersin product surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityCategory {
    Compile,
    Runtime,
    Inspection,
    Control,
    Sandbox,
    Workflow,
}

/// Rendering styles a capability can emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
    Table,
    Tui,
    Artifact,
}

/// Safety metadata shared by CLI and TUI adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafetyProfile {
    pub file_mutation: MutationLevel,
    pub runtime_state_mutation: MutationLevel,
    pub process_lifecycle: ProcessLifecycle,
    pub network: NetworkRequirement,
    pub long_running: LongRunningBehavior,
    pub confirmation: ConfirmationPolicy,
}

impl SafetyProfile {
    pub fn read_only() -> Self {
        Self {
            file_mutation: MutationLevel::None,
            runtime_state_mutation: MutationLevel::None,
            process_lifecycle: ProcessLifecycle::None,
            network: NetworkRequirement::None,
            long_running: LongRunningBehavior::Finite,
            confirmation: ConfirmationPolicy::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationLevel {
    None,
    WritesProjectFiles,
    WritesRuntimeState,
    Destructive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessLifecycle {
    None,
    StartsChildProcess,
    StartsDaemon,
    ControlsExistingProcess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkRequirement {
    None,
    Optional,
    Required,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LongRunningBehavior {
    Finite,
    StreamsUntilComplete,
    WatchesUntilInterrupted,
    Daemon,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmationPolicy {
    None,
    Recommended,
    Required { reason: String },
}

/// Adapter availability for a capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCoverage {
    pub cli: AdapterSupport,
    pub tui: AdapterSupport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterSupport {
    Available,
    Generic,
    Custom,
    Unavailable { reason: String },
}

/// Event stream emitted by capability execution.
#[derive(Clone, Debug, PartialEq)]
pub enum CapabilityEvent {
    Started {
        capability_id: CapabilityId,
    },
    Progress {
        message: String,
        current: Option<u64>,
        total: Option<u64>,
    },
    Prompt {
        id: String,
        message: String,
        confirmation: ConfirmationPolicy,
    },
    Output {
        mode: OutputMode,
        value: Value,
    },
    Completed {
        value: Option<Value>,
    },
    Failed {
        message: String,
    },
}

/// Empty catalog spine; later tickets register real capabilities here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityRegistry {
    specs: Vec<CapabilitySpec>,
}

impl CapabilityRegistry {
    pub fn new(specs: Vec<CapabilitySpec>) -> Self {
        Self { specs }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn specs(&self) -> &[CapabilitySpec] {
        &self.specs
    }

    pub fn get(&self, id: &str) -> Option<&CapabilitySpec> {
        self.specs.iter().find(|spec| spec.id.as_str() == id)
    }
}

pub fn registry() -> CapabilityRegistry {
    CapabilityRegistry::empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn constructs_capability_spec_metadata() {
        let spec = CapabilitySpec {
            id: CapabilityId::new("compile.check"),
            title: "Check prompt sources".to_string(),
            summary: "Parse, include-resolve, and typecheck prompt sources.".to_string(),
            category: CapabilityCategory::Compile,
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" }
                }
            }),
            output_modes: vec![OutputMode::Text, OutputMode::Json],
            safety: SafetyProfile::read_only(),
            adapters: AdapterCoverage {
                cli: AdapterSupport::Available,
                tui: AdapterSupport::Generic,
            },
        };

        assert_eq!(spec.id.as_str(), "compile.check");
        assert_eq!(spec.category, CapabilityCategory::Compile);
        assert_eq!(spec.input_schema["required"][0], "path");
        assert_eq!(spec.output_modes, vec![OutputMode::Text, OutputMode::Json]);
        assert_eq!(spec.safety.file_mutation, MutationLevel::None);
        assert_eq!(spec.adapters.tui, AdapterSupport::Generic);
    }

    #[test]
    fn safety_profile_covers_shared_policy_axes() {
        let safety = SafetyProfile {
            file_mutation: MutationLevel::Destructive,
            runtime_state_mutation: MutationLevel::WritesRuntimeState,
            process_lifecycle: ProcessLifecycle::StartsDaemon,
            network: NetworkRequirement::Required,
            long_running: LongRunningBehavior::Daemon,
            confirmation: ConfirmationPolicy::Required {
                reason: "drops durable runtime state".to_string(),
            },
        };

        assert_eq!(safety.file_mutation, MutationLevel::Destructive);
        assert_eq!(
            safety.runtime_state_mutation,
            MutationLevel::WritesRuntimeState
        );
        assert_eq!(safety.process_lifecycle, ProcessLifecycle::StartsDaemon);
        assert_eq!(safety.network, NetworkRequirement::Required);
        assert_eq!(safety.long_running, LongRunningBehavior::Daemon);
        assert_eq!(
            safety.confirmation,
            ConfirmationPolicy::Required {
                reason: "drops durable runtime state".to_string()
            }
        );
    }

    #[test]
    fn constructs_execution_events() {
        let capability_id = CapabilityId::new("runtime.trace.ls");
        let events = vec![
            CapabilityEvent::Started {
                capability_id: capability_id.clone(),
            },
            CapabilityEvent::Progress {
                message: "loading spans".to_string(),
                current: Some(1),
                total: Some(2),
            },
            CapabilityEvent::Prompt {
                id: "confirm-drop".to_string(),
                message: "Drop selected rows?".to_string(),
                confirmation: ConfirmationPolicy::Required {
                    reason: "destructive action".to_string(),
                },
            },
            CapabilityEvent::Output {
                mode: OutputMode::Table,
                value: json!([{ "session": "abc", "spans": 3 }]),
            },
            CapabilityEvent::Completed {
                value: Some(json!({ "rows": 1 })),
            },
            CapabilityEvent::Failed {
                message: "storage unavailable".to_string(),
            },
        ];

        assert!(matches!(
            events[0],
            CapabilityEvent::Started {
                ref capability_id
            } if capability_id.as_str() == "runtime.trace.ls"
        ));
        assert!(matches!(
            events[1],
            CapabilityEvent::Progress {
                current: Some(1),
                total: Some(2),
                ..
            }
        ));
        assert!(matches!(
            events[2],
            CapabilityEvent::Prompt {
                confirmation: ConfirmationPolicy::Required { .. },
                ..
            }
        ));
        assert!(matches!(
            events[3],
            CapabilityEvent::Output {
                mode: OutputMode::Table,
                ..
            }
        ));
        assert!(matches!(events[4], CapabilityEvent::Completed { .. }));
        assert!(matches!(events[5], CapabilityEvent::Failed { .. }));
    }

    #[test]
    fn registry_starts_empty_but_can_hold_specs() {
        assert!(registry().specs().is_empty());

        let spec = CapabilitySpec {
            id: CapabilityId::new("workflow.scaffold-build"),
            title: "Scaffold and build".to_string(),
            summary: "Create missing starter files and run build.".to_string(),
            category: CapabilityCategory::Workflow,
            input_schema: json!({ "type": "object" }),
            output_modes: vec![OutputMode::Text],
            safety: SafetyProfile {
                file_mutation: MutationLevel::WritesProjectFiles,
                runtime_state_mutation: MutationLevel::None,
                process_lifecycle: ProcessLifecycle::None,
                network: NetworkRequirement::Optional,
                long_running: LongRunningBehavior::StreamsUntilComplete,
                confirmation: ConfirmationPolicy::Recommended,
            },
            adapters: AdapterCoverage {
                cli: AdapterSupport::Unavailable {
                    reason: "not yet exposed as a CLI adapter".to_string(),
                },
                tui: AdapterSupport::Custom,
            },
        };
        let registry = CapabilityRegistry::new(vec![spec]);

        assert_eq!(registry.specs().len(), 1);
        assert_eq!(
            registry
                .get("workflow.scaffold-build")
                .map(|spec| &spec.title),
            Some(&"Scaffold and build".to_string())
        );
        assert!(registry.get("missing").is_none());
    }
}
