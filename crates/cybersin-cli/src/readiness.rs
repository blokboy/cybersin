//! Shared setup-readiness primitives for provider and local secret
//! resolution. CLI commands can render this differently, but should agree
//! on where keys may come from: `cybersin.local.yaml` references,
//! project-root `.env`, and the ambient process environment.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use cybersin_runtime::{Availability, LocalConfigFile, ProviderConfig};

pub const OPENROUTER_PROVIDER: &str = "openrouter";
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
pub const OPENROUTER_BASE_URL_ENV: &str = "OPENROUTER_BASE_URL";

#[derive(Clone, Debug, Default)]
pub struct DotenvReadiness {
    pub path: PathBuf,
    pub present: bool,
    keys: BTreeSet<String>,
}

impl DotenvReadiness {
    pub fn load(project_root: &Path) -> anyhow::Result<Self> {
        let path = project_root.join(".env");
        if !path.is_file() {
            return Ok(Self {
                path,
                present: false,
                keys: BTreeSet::new(),
            });
        }
        let mut keys = BTreeSet::new();
        for item in
            dotenvy::from_path_iter(&path).with_context(|| format!("reading {}", path.display()))?
        {
            let (key, _) = item.with_context(|| format!("parsing {}", path.display()))?;
            keys.insert(key);
        }
        Ok(Self {
            path,
            present: true,
            keys,
        })
    }

    pub fn contains(&self, variable: &str) -> bool {
        self.keys.contains(variable)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeySource {
    LocalConfigDotenv { variable: String },
    LocalConfigEnvironment { variable: String },
    Dotenv { variable: String },
    Environment { variable: String },
}

impl KeySource {
    pub fn label(&self) -> String {
        match self {
            KeySource::LocalConfigDotenv { variable } => {
                format!("cybersin.local.yaml -> .env:{variable}")
            }
            KeySource::LocalConfigEnvironment { variable } => {
                format!("cybersin.local.yaml -> env:{variable}")
            }
            KeySource::Dotenv { variable } => format!(".env:{variable}"),
            KeySource::Environment { variable } => format!("env:{variable}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretReadiness {
    pub variable: String,
    pub source: Option<KeySource>,
    pub local_config_reference: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderReadiness {
    pub name: String,
    pub configured: bool,
    pub availability: Availability,
    pub api_key: SecretReadiness,
    pub base_url: Option<String>,
    pub is_default_provider: bool,
    pub default_model: Option<String>,
    pub routing_provider_allowed: bool,
    pub default_model_allowed: Option<bool>,
}

pub fn openrouter_readiness(
    local_config: Option<&LocalConfigFile>,
    dotenv: &DotenvReadiness,
) -> ProviderReadiness {
    provider_readiness(
        local_config,
        dotenv,
        OPENROUTER_PROVIDER,
        OPENROUTER_API_KEY_ENV,
    )
}

pub fn provider_readiness(
    local_config: Option<&LocalConfigFile>,
    dotenv: &DotenvReadiness,
    provider_name: &str,
    env_var: &str,
) -> ProviderReadiness {
    let provider = local_config.and_then(|config| config.provider(provider_name));
    let default_model = local_config.and_then(|config| config.defaults.model.clone());
    let (routing_provider_allowed, default_model_allowed) =
        routing_readiness(local_config, provider_name, default_model.as_deref());
    ProviderReadiness {
        name: provider_name.to_string(),
        configured: provider.is_some(),
        availability: provider
            .map(|provider| provider.availability)
            .unwrap_or(Availability::Auto),
        api_key: secret_readiness(provider, dotenv, env_var),
        base_url: provider.and_then(|provider| provider.base_url.clone()),
        is_default_provider: local_config.and_then(|config| config.defaults.provider.as_deref())
            == Some(provider_name),
        default_model,
        routing_provider_allowed,
        default_model_allowed,
    }
}

fn routing_readiness(
    local_config: Option<&LocalConfigFile>,
    provider_name: &str,
    default_model: Option<&str>,
) -> (bool, Option<bool>) {
    let Some(config) = local_config else {
        return (true, default_model.map(|_| true));
    };
    let routing = if config.permissions.routing.allowed_providers.is_empty()
        && config.permissions.routing.allowed_models.is_empty()
    {
        &config.routing
    } else {
        &config.permissions.routing
    };
    let provider_allowed = routing.allowed_providers.is_empty()
        || routing
            .allowed_providers
            .iter()
            .any(|provider| provider == provider_name);
    let model_allowed = default_model.map(|model| {
        provider_allowed
            && routing
                .allowed_models
                .get(provider_name)
                .map(|models| models.iter().any(|allowed| allowed == model))
                .unwrap_or(true)
    });
    (provider_allowed, model_allowed)
}

pub fn resolve_openrouter_api_key(local_config: Option<&LocalConfigFile>) -> Option<String> {
    local_config
        .and_then(|config| config.provider(OPENROUTER_PROVIDER))
        .and_then(|provider| provider.api_key.as_ref())
        .and_then(|reference| reference.read())
        .or_else(|| std::env::var(OPENROUTER_API_KEY_ENV).ok())
}

pub fn openrouter_key_reference(local_config: Option<&LocalConfigFile>) -> Option<String> {
    local_config
        .and_then(|config| config.provider(OPENROUTER_PROVIDER))
        .and_then(|provider| provider.api_key.as_ref())
        .map(|reference| reference.variable().to_string())
}

pub fn openrouter_base_url(local_config: Option<&LocalConfigFile>) -> Option<String> {
    local_config
        .and_then(|config| config.provider(OPENROUTER_PROVIDER))
        .and_then(|provider| provider.base_url.clone())
}

fn secret_readiness(
    provider: Option<&ProviderConfig>,
    dotenv: &DotenvReadiness,
    default_env_var: &str,
) -> SecretReadiness {
    if let Some(reference) = provider.and_then(|provider| provider.api_key.as_ref()) {
        let variable = reference.variable().to_string();
        let source = if reference.read().is_some() {
            if dotenv.contains(&variable) {
                Some(KeySource::LocalConfigDotenv {
                    variable: variable.clone(),
                })
            } else {
                Some(KeySource::LocalConfigEnvironment {
                    variable: variable.clone(),
                })
            }
        } else {
            None
        };
        return SecretReadiness {
            variable: variable.clone(),
            source,
            local_config_reference: Some(variable),
        };
    }

    let source = if std::env::var(default_env_var).is_ok() {
        if dotenv.contains(default_env_var) {
            Some(KeySource::Dotenv {
                variable: default_env_var.to_string(),
            })
        } else {
            Some(KeySource::Environment {
                variable: default_env_var.to_string(),
            })
        }
    } else {
        None
    };
    SecretReadiness {
        variable: default_env_var.to_string(),
        source,
        local_config_reference: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_openrouter_key_from_local_config_reference() {
        std::env::set_var("CYBERSIN_TEST_OPENROUTER_KEY", "test-key");
        let config: LocalConfigFile = serde_yaml::from_str(
            "providers:\n  openrouter:\n    api_key: ${CYBERSIN_TEST_OPENROUTER_KEY}\n",
        )
        .unwrap();

        assert_eq!(
            resolve_openrouter_api_key(Some(&config)).as_deref(),
            Some("test-key")
        );
        std::env::remove_var("CYBERSIN_TEST_OPENROUTER_KEY");
    }
}
