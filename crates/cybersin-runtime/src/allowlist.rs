//! Environment-level provider/model allowlist (issue #35 Phase 1):
//! `cybersin.local.yaml`, gitignored and separate from the project-level
//! candidate set compiled into `dist/routing.json`. Which providers/models
//! *this* environment is allowed to route to is a property of which keys
//! are configured here, not of the project — so it's deliberately not part
//! of `cybersin.yaml`/`cybersin.lock`, and it's enforced at call time by
//! [`crate::route_executor::RouteExecutor`] rather than filtered into the
//! (portable, environment-agnostic) build artifact.

use std::collections::BTreeMap;
use std::path::Path;

use cybersin_router::RouteModel;
use serde::Deserialize;

pub const LOCAL_CONFIG_FILENAME: &str = "cybersin.local.yaml";

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
struct LocalConfigFile {
    #[serde(default)]
    routing: RoutingConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
struct RoutingConfig {
    #[serde(default)]
    allowed_providers: Vec<String>,
    #[serde(default)]
    allowed_models: BTreeMap<String, Vec<String>>,
}

/// Which providers/models this environment may route to. Default (no
/// `cybersin.local.yaml`, or an empty `allowed_providers`) is "everything
/// allowed" — every caller that predates this config keeps working
/// unchanged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelAllowlist {
    allowed_providers: Vec<String>,
    allowed_models: BTreeMap<String, Vec<String>>,
}

impl ModelAllowlist {
    /// No restriction at all — every candidate is allowed.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Construct an allowlist directly (as opposed to loading one from
    /// `cybersin.local.yaml`) — e.g. for programmatic setup, or tests
    /// exercising `RouteExecutor`'s enforcement without a file on disk.
    pub fn new(
        allowed_providers: Vec<String>,
        allowed_models: BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            allowed_providers,
            allowed_models,
        }
    }

    /// Load `<project_dir>/cybersin.local.yaml`. A missing file is not an
    /// error — it means "no restriction".
    pub fn load(project_dir: impl AsRef<Path>) -> Result<Self, AllowlistError> {
        let path = project_dir.as_ref().join(LOCAL_CONFIG_FILENAME);
        if !path.is_file() {
            return Ok(Self::allow_all());
        }
        let bytes = std::fs::read(&path).map_err(|source| AllowlistError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let file: LocalConfigFile =
            serde_yaml::from_slice(&bytes).map_err(|source| AllowlistError::Yaml {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Self {
            allowed_providers: file.routing.allowed_providers,
            allowed_models: file.routing.allowed_models,
        })
    }

    /// Whether `model` may be routed to in this environment.
    ///
    /// - No `allowed_providers` declared: every provider is allowed.
    /// - `allowed_providers` declared: only listed providers are allowed.
    /// - A provider with an `allowed_models` entry: only those model names
    ///   are allowed for it, even though the provider itself is allowed. A
    ///   provider with no `allowed_models` entry has every one of its
    ///   models allowed.
    pub fn allows(&self, model: &RouteModel) -> bool {
        if !self.allowed_providers.is_empty()
            && !self.allowed_providers.iter().any(|p| p == &model.provider)
        {
            return false;
        }
        match self.allowed_models.get(&model.provider) {
            Some(models) => models.iter().any(|name| name == &model.name),
            None => true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AllowlistError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use cybersin_ir::QualityTier;
    use cybersin_router::ModelKind;

    fn model(provider: &str, name: &str) -> RouteModel {
        RouteModel {
            name: name.into(),
            provider: provider.into(),
            quality: QualityTier::High,
            estimated_cost_usd: 0.01,
            model_kind: ModelKind::Provider,
        }
    }

    #[test]
    fn allow_all_allows_everything() {
        let allowlist = ModelAllowlist::allow_all();
        assert!(allowlist.allows(&model("anthropic", "claude-3-5-sonnet")));
        assert!(allowlist.allows(&model("openai", "gpt-4o-mini")));
    }

    #[test]
    fn restricts_by_provider() {
        let allowlist = ModelAllowlist {
            allowed_providers: vec!["anthropic".into()],
            allowed_models: BTreeMap::new(),
        };
        assert!(allowlist.allows(&model("anthropic", "claude-3-5-sonnet")));
        assert!(!allowlist.allows(&model("openai", "gpt-4o-mini")));
    }

    #[test]
    fn restricts_by_model_within_an_allowed_provider() {
        let mut allowed_models = BTreeMap::new();
        allowed_models.insert(
            "anthropic".to_string(),
            vec!["claude-3-5-haiku".to_string()],
        );
        let allowlist = ModelAllowlist {
            allowed_providers: vec!["anthropic".into()],
            allowed_models,
        };
        assert!(allowlist.allows(&model("anthropic", "claude-3-5-haiku")));
        assert!(!allowlist.allows(&model("anthropic", "claude-3-5-sonnet")));
    }

    #[test]
    fn loads_from_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(LOCAL_CONFIG_FILENAME),
            "routing:\n  allowed_providers: [anthropic]\n  allowed_models:\n    anthropic: [claude-3-5-haiku]\n",
        )
        .unwrap();
        let allowlist = ModelAllowlist::load(dir.path()).unwrap();
        assert!(allowlist.allows(&model("anthropic", "claude-3-5-haiku")));
        assert!(!allowlist.allows(&model("anthropic", "claude-3-5-sonnet")));
    }

    #[test]
    fn missing_file_allows_everything() {
        let dir = tempfile::tempdir().unwrap();
        let allowlist = ModelAllowlist::load(dir.path()).unwrap();
        assert!(allowlist.allows(&model("anthropic", "claude-3-5-sonnet")));
    }
}
