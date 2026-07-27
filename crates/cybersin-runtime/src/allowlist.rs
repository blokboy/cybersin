//! Environment-level local configuration: `cybersin.local.yaml`,
//! gitignored and separate from the portable project config and compiled
//! artifacts. Provider/tool availability, defaults, sandbox/container
//! settings, and routing/tool permissions are properties of this machine,
//! not of `cybersin.yaml`/`cybersin.lock`; routing permissions remain
//! enforced at call time by [`crate::route_executor::RouteExecutor`]
//! rather than filtered into the environment-agnostic build artifact.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cybersin_router::RouteModel;
use serde::Deserialize;

pub const LOCAL_CONFIG_FILENAME: &str = "cybersin.local.yaml";

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LocalConfigFile {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub availability: Availability,
    #[serde(default)]
    pub api_key: Option<EnvRef>,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ToolConfig {
    #[serde(default)]
    pub availability: Availability,
    #[serde(default)]
    pub api_key: Option<EnvRef>,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SandboxConfig {
    pub backend: Option<String>,
    pub root: Option<PathBuf>,
    pub scope: Option<String>,
    pub container_runtime: Option<EnvRef>,
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DefaultsConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub sandbox_backend: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub tools: ToolPermissions,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ToolPermissions {
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub denied: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    #[serde(default)]
    pub allowed_models: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Availability {
    Available,
    Unavailable,
    #[default]
    Auto,
}

/// A local secret reference. The default config shape stores the name of
/// an environment variable, not the secret's value, so local files can be
/// checked, shared, or templated without embedding credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRef {
    variable: String,
}

impl EnvRef {
    pub fn new(variable: impl Into<String>) -> Self {
        Self {
            variable: variable.into(),
        }
    }

    pub fn variable(&self) -> &str {
        &self.variable
    }

    pub fn read(&self) -> Option<String> {
        std::env::var(&self.variable).ok()
    }
}

impl<'de> Deserialize<'de> for EnvRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Name(String),
            Map { env: String },
        }

        let raw = Raw::deserialize(deserializer)?;
        let variable = match raw {
            Raw::Name(value) => parse_env_ref(&value).map_err(serde::de::Error::custom)?,
            Raw::Map { env } => env,
        };
        if variable.trim().is_empty() {
            return Err(serde::de::Error::custom(
                "environment variable reference cannot be empty",
            ));
        }
        Ok(Self::new(variable))
    }
}

fn parse_env_ref(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if let Some(variable) = trimmed
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        return Ok(variable.to_string());
    }
    if let Some(variable) = trimmed.strip_prefix("env:") {
        return Ok(variable.to_string());
    }
    if trimmed
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Ok(trimmed.to_string());
    }
    Err("secrets in local config must be environment-variable references (for example `${OPENROUTER_API_KEY}` or `{ env: OPENROUTER_API_KEY }`)"
        .to_string())
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
        let Some(file) = LocalConfigFile::load_optional(project_dir)? else {
            return Ok(Self::allow_all());
        };
        Ok(file.model_allowlist())
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

impl LocalConfigFile {
    /// Load `<project_dir>/cybersin.local.yaml`. A missing file returns
    /// `None` so callers can preserve env-only behavior without branching
    /// on filesystem errors themselves.
    pub fn load_optional(project_dir: impl AsRef<Path>) -> Result<Option<Self>, AllowlistError> {
        let path = project_dir.as_ref().join(LOCAL_CONFIG_FILENAME);
        if !path.is_file() {
            return Ok(None);
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
        Ok(Some(file))
    }

    /// Routing permissions, accepting both the new
    /// `permissions.routing.*` location and the legacy top-level
    /// `routing.*` location.
    pub fn model_allowlist(&self) -> ModelAllowlist {
        let routing = if self.permissions.routing.allowed_providers.is_empty()
            && self.permissions.routing.allowed_models.is_empty()
        {
            &self.routing
        } else {
            &self.permissions.routing
        };
        ModelAllowlist {
            allowed_providers: routing.allowed_providers.clone(),
            allowed_models: routing.allowed_models.clone(),
        }
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    pub fn tool(&self, name: &str) -> Option<&ToolConfig> {
        self.tools.get(name)
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
    fn parses_local_config_sections_separately() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(LOCAL_CONFIG_FILENAME),
            r#"
providers:
  openrouter:
    availability: available
    api_key: ${OPENROUTER_API_KEY}
    base_url: https://openrouter.test/api/v1
tools:
  tavily:
    availability: auto
    api_key:
      env: TAVILY_API_KEY
    base_url: https://tavily.test
sandbox:
  backend: docker
  root: .cybersin/sandbox-dev
  scope: session
  container_runtime: CYBERSIN_CONTAINER_RUNTIME
  settings:
    mem_mb: 256
defaults:
  provider: openrouter
  model: openai/gpt-4o-mini
  sandbox_backend: docker
  tools: [web_search, web_fetch]
permissions:
  routing:
    allowed_providers: [openrouter]
    allowed_models:
      openrouter: [openai/gpt-4o-mini]
  tools:
    allowed: [web_search]
    denied: [wire_transfer]
"#,
        )
        .unwrap();

        let config = LocalConfigFile::load_optional(dir.path()).unwrap().unwrap();
        let openrouter = config.provider("openrouter").unwrap();
        assert_eq!(openrouter.availability, Availability::Available);
        assert_eq!(
            openrouter.api_key.as_ref().unwrap().variable(),
            "OPENROUTER_API_KEY"
        );
        assert_eq!(
            config
                .tool("tavily")
                .unwrap()
                .api_key
                .as_ref()
                .unwrap()
                .variable(),
            "TAVILY_API_KEY"
        );
        assert_eq!(config.sandbox.backend.as_deref(), Some("docker"));
        assert_eq!(config.sandbox.scope.as_deref(), Some("session"));
        assert_eq!(
            config
                .sandbox
                .container_runtime
                .as_ref()
                .unwrap()
                .variable(),
            "CYBERSIN_CONTAINER_RUNTIME"
        );
        assert_eq!(config.defaults.provider.as_deref(), Some("openrouter"));
        assert_eq!(config.permissions.tools.allowed, vec!["web_search"]);

        let allowlist = config.model_allowlist();
        assert!(allowlist.allows(&model("openrouter", "openai/gpt-4o-mini")));
        assert!(!allowlist.allows(&model("openrouter", "anthropic/claude-3-haiku")));
    }

    #[test]
    fn rejects_raw_secret_values() {
        let error = serde_yaml::from_str::<LocalConfigFile>(
            "providers:\n  openrouter:\n    api_key: sk-or-raw-secret\n",
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("environment-variable references"));
    }

    #[test]
    fn legacy_routing_section_still_feeds_permissions() {
        let config: LocalConfigFile = serde_yaml::from_str(
            "routing:\n  allowed_providers: [openrouter]\n  allowed_models:\n    openrouter: [openai/gpt-4o-mini]\n",
        )
        .unwrap();
        let allowlist = config.model_allowlist();

        assert!(allowlist.allows(&model("openrouter", "openai/gpt-4o-mini")));
        assert!(!allowlist.allows(&model("anthropic", "claude-3-5-sonnet")));
    }

    #[test]
    fn missing_file_allows_everything() {
        let dir = tempfile::tempdir().unwrap();
        let allowlist = ModelAllowlist::load(dir.path()).unwrap();
        assert!(allowlist.allows(&model("anthropic", "claude-3-5-sonnet")));
    }
}
