//! Data-first capability metadata and execution events.
//!
//! The capability layer is intentionally separate from `commands::*`: command
//! modules are CLI adapters, while these types describe the shared product
//! surface that CLI and TUI adapters will eventually invoke.

use serde_json::{json, Value};

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

/// Data-only catalog of Cybersin's current user-facing product surface.
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

    pub fn cli_operations(&self) -> Vec<&str> {
        self.specs
            .iter()
            .filter_map(|spec| match spec.adapters.cli {
                AdapterSupport::Available => Some(spec.id.as_str()),
                _ => None,
            })
            .collect()
    }
}

pub fn registry() -> CapabilityRegistry {
    CapabilityRegistry::new(vec![
        spec(
            "compile.build",
            "Build project",
            "Compile a project into dist artifacts.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(
                NetworkRequirement::Optional,
                LongRunningBehavior::StreamsUntilComplete,
            ),
            cli(),
        ),
        spec(
            "compile.build.watch",
            "Watch build",
            "Compile once, then rebuild when project sources change.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(
                NetworkRequirement::Optional,
                LongRunningBehavior::WatchesUntilInterrupted,
            ),
            cli(),
        ),
        spec(
            "compile.diff",
            "Diff builds",
            "Compare build artifacts produced from the current tree and another git ref.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Text],
            read_only(),
            cli(),
        ),
        spec(
            "compile.check",
            "Check prompt sources",
            "Parse, include-resolve, typecheck, and emit prompt IR.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text, OutputMode::Json],
            read_only(),
            cli(),
        ),
        spec(
            "compile.convert",
            "Convert prompt",
            "Turn raw natural-language prompt text into a buildable prompt source.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(NetworkRequirement::Required, LongRunningBehavior::Finite),
            cli(),
        ),
        spec(
            "workflow.init",
            "Initialize project",
            "Scaffold a new Cybersin project layout.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(NetworkRequirement::None, LongRunningBehavior::Finite),
            cli(),
        ),
        spec(
            "compile.fmt",
            "Format prompt source",
            "Normalize one prompt source file's formatting.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text],
            writes_project_files(NetworkRequirement::None, LongRunningBehavior::Finite),
            cli(),
        ),
        spec(
            "compile.fmt.check",
            "Check prompt formatting",
            "Report whether one prompt source file is already canonically formatted.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text],
            read_only(),
            cli(),
        ),
        spec(
            "runtime.run.stub",
            "Run stub agent",
            "Run the built-in stub agent against a compiled dist fixture.",
            CapabilityCategory::Runtime,
            vec![OutputMode::Text, OutputMode::Json],
            runtime_writes(
                ProcessLifecycle::None,
                LongRunningBehavior::StreamsUntilComplete,
                NetworkRequirement::None,
            ),
            cli(),
        ),
        spec(
            "runtime.run.agent",
            "Run agent",
            "Spawn an agent harness and drive a live runtime session.",
            CapabilityCategory::Runtime,
            vec![OutputMode::Text, OutputMode::Json],
            runtime_writes(
                ProcessLifecycle::StartsChildProcess,
                LongRunningBehavior::StreamsUntilComplete,
                NetworkRequirement::Required,
            ),
            cli(),
        ),
        spec(
            "inspection.trace.ls",
            "List traces",
            "List recorded spans from the trace store.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            cli(),
        ),
        spec(
            "inspection.trace.show",
            "Show trace",
            "Show one recorded span as JSON.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Json],
            runtime_read(),
            cli(),
        ),
        spec(
            "inspection.trace.sample",
            "Sample trace",
            "Promote one LLM span into a portable eval fixture.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Text, OutputMode::Artifact],
            SafetyProfile {
                file_mutation: MutationLevel::WritesProjectFiles,
                ..runtime_read()
            },
            cli(),
        ),
        spec(
            "inspection.cost",
            "Roll up cost",
            "Group recorded cost by session, agent, model, tool, or day.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            cli(),
        ),
        spec(
            "workflow.eval.run",
            "Run evals",
            "Execute eval suites and print score distributions.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text],
            SafetyProfile {
                process_lifecycle: ProcessLifecycle::StartsChildProcess,
                network: NetworkRequirement::Optional,
                long_running: LongRunningBehavior::StreamsUntilComplete,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "workflow.eval.gate",
            "Gate evals",
            "Execute eval suites and fail if assertions regress.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text],
            SafetyProfile {
                process_lifecycle: ProcessLifecycle::StartsChildProcess,
                network: NetworkRequirement::Optional,
                long_running: LongRunningBehavior::StreamsUntilComplete,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "inspection.explain",
            "Explain prompt",
            "Explain compiled prompt tokens, routing, costs, sessions, traces, and tools.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Text, OutputMode::Tui],
            runtime_read(),
            cli(),
        ),
        spec(
            "control.ops",
            "Open ops",
            "Inspect and interact with project sessions, traces, costs, approvals, and builds.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Tui],
            SafetyProfile {
                runtime_state_mutation: MutationLevel::WritesRuntimeState,
                process_lifecycle: ProcessLifecycle::ControlsExistingProcess,
                network: NetworkRequirement::Optional,
                long_running: LongRunningBehavior::WatchesUntilInterrupted,
                confirmation: ConfirmationPolicy::Recommended,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "control.daemon.server",
            "Run daemon server",
            "Run Postgres-backed TCP and mTLS multi-worker daemon mode.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            SafetyProfile {
                runtime_state_mutation: MutationLevel::WritesRuntimeState,
                process_lifecycle: ProcessLifecycle::StartsDaemon,
                network: NetworkRequirement::Required,
                long_running: LongRunningBehavior::Daemon,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "control.dlq.ls",
            "List dead letters",
            "List failed tool calls that have not been acknowledged.",
            CapabilityCategory::Control,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            cli(),
        ),
        spec(
            "control.dlq.show",
            "Show dead letter",
            "Show one dead-lettered tool call as JSON.",
            CapabilityCategory::Control,
            vec![OutputMode::Json],
            runtime_read(),
            cli(),
        ),
        spec(
            "control.dlq.retry",
            "Retry dead letter",
            "Reopen and rerun one dead-lettered tool call.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Json],
            tool_execution(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.dlq.drop",
            "Drop dead letter",
            "Acknowledge and hide one dead-lettered tool call without deleting its audit row.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.approve",
            "Approve parked call",
            "Resume an approval-gated parked tool call.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Json],
            tool_execution(ConfirmationPolicy::Required {
                reason: "runs a previously parked tool call".to_string(),
            }),
            cli(),
        ),
        spec(
            "control.deny",
            "Deny parked call",
            "Resolve a parked tool call as denied.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Required {
                reason: "changes a parked call outcome".to_string(),
            }),
            cli(),
        ),
        spec(
            "control.sessions.ls",
            "List sessions",
            "List durable runtime sessions.",
            CapabilityCategory::Control,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            cli(),
        ),
        spec(
            "control.sessions.show",
            "Show session",
            "Show one session, its events, state, and latest checkpoint.",
            CapabilityCategory::Control,
            vec![OutputMode::Json],
            runtime_read(),
            cli(),
        ),
        spec(
            "control.sessions.resume",
            "Resume session",
            "Resume a durable session against a config hash.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Json],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.sessions.kill",
            "Kill session",
            "Mark a durable session as killed.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Required {
                reason: "stops a durable runtime session".to_string(),
            }),
            cli(),
        ),
        spec(
            "control.sessions.migrate",
            "Migrate session",
            "Move a durable session to a new config hash.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.notify",
            "Notify session",
            "Deliver a durable steering signal to a runtime session.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "sandbox.exec",
            "Execute sandbox command",
            "Run a command in an isolated call or session workspace.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            sandbox_exec(),
            cli(),
        ),
        spec(
            "sandbox.snapshot",
            "Snapshot sandbox",
            "Snapshot a persistent session workspace checkpoint.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            sandbox_lifecycle(MutationLevel::WritesRuntimeState, ConfirmationPolicy::None),
            cli(),
        ),
        spec(
            "sandbox.diff",
            "Diff sandbox",
            "Show workspace changes relative to a checkpoint snapshot.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            SafetyProfile::read_only(),
            cli(),
        ),
        spec(
            "sandbox.restore",
            "Restore sandbox",
            "Restore a persistent session workspace to a checkpoint snapshot.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            sandbox_lifecycle(
                MutationLevel::Destructive,
                ConfirmationPolicy::Required {
                    reason: "replaces workspace files with checkpoint contents".to_string(),
                },
            ),
            cli(),
        ),
        spec(
            "workflow.optimize",
            "Optimize project",
            "Analyze traces, emit an optimization report, and rebuild with observed routing stats.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(
                NetworkRequirement::Optional,
                LongRunningBehavior::StreamsUntilComplete,
            ),
            cli(),
        ),
        spec(
            "workflow.scaffold-build",
            "Scaffold and build prompt source",
            "Create the TUI's prompt-source scaffold and immediately build it.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(
                NetworkRequirement::Optional,
                LongRunningBehavior::StreamsUntilComplete,
            ),
            unavailable("private TUI workflow is not exposed through the capability adapter yet"),
        ),
    ])
}

fn spec(
    id: &str,
    title: &str,
    summary: &str,
    category: CapabilityCategory,
    output_modes: Vec<OutputMode>,
    safety: SafetyProfile,
    adapters: AdapterCoverage,
) -> CapabilitySpec {
    CapabilitySpec {
        id: CapabilityId::new(id),
        title: title.to_string(),
        summary: summary.to_string(),
        category,
        input_schema: json!({
            "type": "object",
            "additionalProperties": true
        }),
        output_modes,
        safety,
        adapters,
    }
}

fn cli() -> AdapterCoverage {
    AdapterCoverage {
        cli: AdapterSupport::Available,
        tui: AdapterSupport::Unavailable {
            reason: "capability invocation is not wired into the TUI adapter yet".to_string(),
        },
    }
}

fn unavailable(reason: &str) -> AdapterCoverage {
    AdapterCoverage {
        cli: AdapterSupport::Unavailable {
            reason: reason.to_string(),
        },
        tui: AdapterSupport::Unavailable {
            reason: reason.to_string(),
        },
    }
}

fn read_only() -> SafetyProfile {
    SafetyProfile::read_only()
}

fn runtime_read() -> SafetyProfile {
    SafetyProfile {
        process_lifecycle: ProcessLifecycle::ControlsExistingProcess,
        ..SafetyProfile::read_only()
    }
}

fn runtime_writes(
    process_lifecycle: ProcessLifecycle,
    long_running: LongRunningBehavior,
    network: NetworkRequirement,
) -> SafetyProfile {
    SafetyProfile {
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle,
        network,
        long_running,
        ..SafetyProfile::read_only()
    }
}

fn runtime_mutation(confirmation: ConfirmationPolicy) -> SafetyProfile {
    SafetyProfile {
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle: ProcessLifecycle::ControlsExistingProcess,
        confirmation,
        ..SafetyProfile::read_only()
    }
}

fn writes_project_files(
    network: NetworkRequirement,
    long_running: LongRunningBehavior,
) -> SafetyProfile {
    SafetyProfile {
        file_mutation: MutationLevel::WritesProjectFiles,
        network,
        long_running,
        ..SafetyProfile::read_only()
    }
}

fn tool_execution(confirmation: ConfirmationPolicy) -> SafetyProfile {
    SafetyProfile {
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle: ProcessLifecycle::StartsChildProcess,
        network: NetworkRequirement::Optional,
        long_running: LongRunningBehavior::StreamsUntilComplete,
        confirmation,
        ..SafetyProfile::read_only()
    }
}

fn sandbox_exec() -> SafetyProfile {
    SafetyProfile {
        file_mutation: MutationLevel::WritesRuntimeState,
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle: ProcessLifecycle::StartsChildProcess,
        long_running: LongRunningBehavior::StreamsUntilComplete,
        confirmation: ConfirmationPolicy::Recommended,
        ..SafetyProfile::read_only()
    }
}

fn sandbox_lifecycle(
    file_mutation: MutationLevel,
    confirmation: ConfirmationPolicy,
) -> SafetyProfile {
    SafetyProfile {
        file_mutation,
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        confirmation,
        ..SafetyProfile::read_only()
    }
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
    fn registry_contains_current_catalog_with_unique_ids() {
        use std::collections::BTreeSet;

        let registry = registry();
        let mut ids = BTreeSet::new();
        for spec in registry.specs() {
            assert!(
                ids.insert(spec.id.as_str()),
                "duplicate capability id {}",
                spec.id.as_str()
            );
            assert!(
                spec.input_schema.is_object(),
                "{} should declare a structured input schema",
                spec.id.as_str()
            );
            assert!(
                !spec.output_modes.is_empty(),
                "{} should declare at least one output mode",
                spec.id.as_str()
            );
            match &spec.adapters.tui {
                AdapterSupport::Unavailable { reason } => assert!(
                    !reason.is_empty(),
                    "{} should explain why TUI capability invocation is unavailable",
                    spec.id.as_str()
                ),
                other => panic!(
                    "{} unexpectedly declares TUI adapter support: {other:?}",
                    spec.id.as_str()
                ),
            }
        }

        assert!(registry.get("compile.build").is_some());
        assert!(registry.get("runtime.run.stub").is_some());
        assert!(registry.get("runtime.run.agent").is_some());
        assert!(registry.get("sandbox.restore").is_some());
        assert_eq!(
            registry
                .get("workflow.scaffold-build")
                .map(|spec| &spec.adapters.cli),
            Some(&AdapterSupport::Unavailable {
                reason: "private TUI workflow is not exposed through the capability adapter yet"
                    .to_string()
            })
        );
        assert!(registry.get("missing").is_none());
    }
}
