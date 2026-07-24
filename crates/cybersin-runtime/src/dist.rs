//! A hand-written `dist/`-shaped fixture (spec §6.6), loaded from disk.
//!
//! Spec §14's M1 exit criterion is "stub agent runs on a hand-written
//! `dist/`", deliberately not real compiler output (`cybersin-frontend`,
//! `cybersin-passes`, `cybersin-router`, and `cybersin-backends` don't
//! exist yet). This module loads a directory laid out like the real
//! `dist/` (`manifest.json`, `prompts/<name>.json`, `routing.json`,
//! `budget/<name>.json`) but hand-authored, bundled at
//! `crates/cybersin-runtime/fixtures/dist/` and committed to the repo so
//! `cargo test`/`cargo run` reproduce the same fixture on any checkout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cybersin_ir::{BudgetArtifact, PromptIr};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum DistError {
    #[error("io error reading dist fixture at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path} as JSON: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("dist fixture has no routing entry for prompt {0:?}")]
    MissingRouting(String),
    #[error("dist fixture has no prompt named {0:?}")]
    MissingPrompt(String),
}

/// `dist/manifest.json` (spec §6.6): build hash, git SHA, lockfile hash,
/// per-artifact content hashes in the real compiler output. This
/// hand-written fixture only carries the identifying fields, since
/// nothing here needs to verify byte-identical rebuilds (that's `build
/// --frozen`, spec §7, a later issue).
#[derive(Debug, Clone, Deserialize)]
pub struct DistManifest {
    pub build_hash: String,
    pub git_sha: String,
}

/// Minimal per-prompt routing/pricing info the stub agent needs to price
/// one model call. **Not** the real `routing.json` shape spec §6.3/§8.3
/// describes (an ordered cache → cascade → fallback decision list per
/// prompt) — that belongs to `cybersin-router`, a later issue. This is
/// just enough for `RuntimeDaemon` to compute a `usd_cost`.
#[derive(Debug, Clone, Deserialize)]
pub struct RoutingEntry {
    pub model: String,
    pub usd_per_1k_prompt_tokens: f64,
    pub usd_per_1k_completion_tokens: f64,
    /// The stub agent never calls a real model, so completion length is a
    /// fixed fixture value rather than an observed one.
    pub completion_tokens_estimate: u32,
    /// Which `BudgetPlan::target` (spec §6.2/§6.5) this routing entry's
    /// model renders under, e.g. `"generic"` or a model-family name —
    /// the same naming convention `cybersin-backends`' `Backend::target()`
    /// will use once that crate exists (it's still a placeholder skeleton
    /// as of this issue). Threading the target through here, rather than
    /// hardcoding `"generic"` in the context assembler, is the least
    /// invasive way for `RuntimeDaemon::handle_llm_request` to know which
    /// `BudgetPlan` this specific call's assembler run should execute
    /// (spec §8.3a). Defaults to `"generic"` so fixtures/dist artifacts
    /// written before this field existed keep deserializing.
    #[serde(default = "default_target")]
    pub target: String,
}

fn default_target() -> String {
    "generic".to_string()
}

/// Per-tool policy (spec §8.2's approval-gate policy hook), fixture-driven
/// the same way pricing is: an optional `dist/tools.json`, `{tool:
/// ToolPolicy}`. Tools not listed default to no approval gate — this
/// issue's stub tool-call flow ran every call the same way before, so
/// "no entry" preserving that is what keeps the existing stub-agent
/// scenario (`web_search`, ungated) behaving identically.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolPolicy {
    #[serde(default = "default_retry_class")]
    pub retry_class: String,
    #[serde(default)]
    pub approval: Option<String>,
}

fn default_retry_class() -> String {
    "write".to_string()
}

impl ToolPolicy {
    /// spec §8.2 / this issue's acceptance criterion: "a critical-class
    /// call with `approval: required` parks the session".
    pub fn requires_approval(&self) -> bool {
        self.approval.as_deref() == Some("required")
    }
}

/// A loaded hand-written `dist/` fixture: everything
/// [`crate::session::RuntimeDaemon`] needs to drive one stub session.
#[derive(Debug, Clone)]
pub struct DistFixture {
    pub manifest: DistManifest,
    pub prompts: BTreeMap<String, PromptIr>,
    pub routing: BTreeMap<String, RoutingEntry>,
    pub budgets: BTreeMap<String, BudgetArtifact>,
    /// Approval-gate policy per tool name — see [`ToolPolicy`]. Empty for
    /// any fixture without a `tools.json`.
    pub tools: BTreeMap<String, ToolPolicy>,
    /// Cascade fallback steps per prompt name, ordered cheapest-first
    /// (spec §8.5's degrade target: "falls back to the cheapest cascade
    /// step") — **not** the real router's quality-tiered cascade
    /// (`cybersin-router`'s `CascadeStep`, a later issue's wiring); just
    /// enough alternate `RoutingEntry`s for the stub daemon's budget
    /// enforcement to re-route to a cheaper model. Empty for any prompt
    /// without a `cascade.json` entry.
    pub cascades: BTreeMap<String, Vec<RoutingEntry>>,
}

impl DistFixture {
    /// Load a `dist/`-shaped directory: `manifest.json`,
    /// `prompts/*.json` (each a [`PromptIr`], keyed by its own `name`
    /// field once loaded — filenames are just for human navigation),
    /// `routing.json` (a `{prompt_name: RoutingEntry}` map), and an
    /// optional `budget/*.json` (each a [`BudgetArtifact`], keyed by its
    /// own `prompt_name` field).
    pub fn load_dir(dir: impl AsRef<Path>) -> Result<Self, DistError> {
        let dir = dir.as_ref();
        let manifest: DistManifest = read_json(&dir.join("manifest.json"))?;
        let routing: BTreeMap<String, RoutingEntry> = read_json(&dir.join("routing.json"))?;

        let mut prompts = BTreeMap::new();
        for path in json_files_in(&dir.join("prompts"))? {
            let prompt: PromptIr = read_json(&path)?;
            prompts.insert(prompt.name.clone(), prompt);
        }

        let mut budgets = BTreeMap::new();
        let budget_dir = dir.join("budget");
        if budget_dir.is_dir() {
            for path in json_files_in(&budget_dir)? {
                let artifact: BudgetArtifact = read_json(&path)?;
                budgets.insert(artifact.prompt_name.clone(), artifact);
            }
        }

        // Both optional: a fixture (or a project's real dist/, later)
        // with no gated tools and no declared cascade fallbacks is
        // completely valid — budget enforcement just has nothing to do.
        let tools_path = dir.join("tools.json");
        let tools: BTreeMap<String, ToolPolicy> = if tools_path.is_file() {
            read_json(&tools_path)?
        } else {
            BTreeMap::new()
        };

        let cascade_path = dir.join("cascade.json");
        let cascades: BTreeMap<String, Vec<RoutingEntry>> = if cascade_path.is_file() {
            read_json(&cascade_path)?
        } else {
            BTreeMap::new()
        };

        Ok(Self {
            manifest,
            prompts,
            routing,
            budgets,
            tools,
            cascades,
        })
    }

    pub fn prompt(&self, name: &str) -> Result<&PromptIr, DistError> {
        self.prompts
            .get(name)
            .ok_or_else(|| DistError::MissingPrompt(name.to_string()))
    }

    pub fn routing(&self, name: &str) -> Result<&RoutingEntry, DistError> {
        self.routing
            .get(name)
            .ok_or_else(|| DistError::MissingRouting(name.to_string()))
    }

    pub fn budget(&self, name: &str) -> Option<&BudgetArtifact> {
        self.budgets.get(name)
    }

    pub fn tool_policy(&self, tool: &str) -> Option<&ToolPolicy> {
        self.tools.get(tool)
    }

    /// Cascade steps for `prompt_name`, ordered cheapest-first — `&[]` if
    /// this fixture declares none, so callers can fall back to
    /// [`DistFixture::routing`] without a special case.
    pub fn cascade(&self, prompt_name: &str) -> &[RoutingEntry] {
        self.cascades
            .get(prompt_name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// The `dist/` fixture bundled with this crate
/// (`fixtures/dist/`), resolved relative to the crate's own manifest
/// directory so it's found regardless of the caller's working directory.
pub fn bundled_stub_dist_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/dist")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, DistError> {
    let text = std::fs::read_to_string(path).map_err(|e| DistError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    serde_json::from_str(&text).map_err(|e| DistError::Json {
        path: path.display().to_string(),
        source: e,
    })
}

fn json_files_in(dir: &Path) -> Result<Vec<PathBuf>, DistError> {
    let entries = std::fs::read_dir(dir).map_err(|e| DistError::Io {
        path: dir.display().to_string(),
        source: e,
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| DistError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_bundled_stub_dist_fixture() {
        let dist = DistFixture::load_dir(bundled_stub_dist_dir()).expect("load bundled fixture");
        assert!(!dist.manifest.build_hash.is_empty());
        let prompt = dist.prompt("researcher").expect("researcher prompt");
        assert_eq!(prompt.name, "researcher");
        assert!(!prompt.sections.is_empty());

        let routing = dist.routing("researcher").expect("researcher routing");
        assert!(!routing.model.is_empty());
        assert!(routing.usd_per_1k_prompt_tokens > 0.0);

        let budget = dist.budget("researcher").expect("researcher budget");
        assert_eq!(budget.prompt_name, "researcher");
        assert!(!budget.plans.is_empty());
    }

    #[test]
    fn missing_prompt_and_routing_error_clearly() {
        let dist = DistFixture::load_dir(bundled_stub_dist_dir()).expect("load bundled fixture");
        assert!(matches!(
            dist.prompt("nope"),
            Err(DistError::MissingPrompt(_))
        ));
        assert!(matches!(
            dist.routing("nope"),
            Err(DistError::MissingRouting(_))
        ));
    }
}
