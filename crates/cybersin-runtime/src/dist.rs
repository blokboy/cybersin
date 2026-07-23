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
}

/// A loaded hand-written `dist/` fixture: everything
/// [`crate::session::RuntimeDaemon`] needs to drive one stub session.
#[derive(Debug, Clone)]
pub struct DistFixture {
    pub manifest: DistManifest,
    pub prompts: BTreeMap<String, PromptIr>,
    pub routing: BTreeMap<String, RoutingEntry>,
    pub budgets: BTreeMap<String, BudgetArtifact>,
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

        Ok(Self {
            manifest,
            prompts,
            routing,
            budgets,
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
