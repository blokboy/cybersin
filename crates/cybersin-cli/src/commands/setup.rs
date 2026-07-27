//! `cybersin setup`: write machine-local readiness config, then render
//! the same report as `cybersin doctor`.

use std::fs;
use std::path::Path;

use anyhow::Context;
use clap::Args;
use cybersin_runtime::allowlist::LOCAL_CONFIG_FILENAME;
use serde_yaml::{Mapping, Value};

use crate::commands::doctor;
use crate::project::discover_project_root;
use crate::readiness::{OPENROUTER_API_KEY_ENV, OPENROUTER_PROVIDER};

pub const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-4o-mini";

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Environment variable name to reference for the OpenRouter API key.
    #[arg(long, default_value = OPENROUTER_API_KEY_ENV)]
    pub openrouter_api_key_env: String,

    /// Default OpenRouter model to record when no local default model exists.
    #[arg(long, default_value = DEFAULT_OPENROUTER_MODEL)]
    pub model: String,

    /// Explicit raw-secret opt-in. Currently rejected because local config
    /// only supports environment-variable references.
    #[arg(long, hide_env_values = true)]
    pub raw_openrouter_api_key: Option<String>,
}

impl Default for SetupArgs {
    fn default() -> Self {
        Self {
            openrouter_api_key_env: OPENROUTER_API_KEY_ENV.to_string(),
            model: DEFAULT_OPENROUTER_MODEL.to_string(),
            raw_openrouter_api_key: None,
        }
    }
}

pub fn execute(project_start: &Path, args: SetupArgs) -> anyhow::Result<()> {
    if args.raw_openrouter_api_key.is_some() {
        anyhow::bail!(
            "--raw-openrouter-api-key is not supported by the current local config model; use --openrouter-api-key-env {OPENROUTER_API_KEY_ENV} and set that variable in .env or your shell"
        );
    }
    let env_var = normalize_env_var(&args.openrouter_api_key_env)?;
    let Some(project_root) = discover_project_root(project_start) else {
        anyhow::bail!(
            "no cybersin.yaml found in this directory or its parents; run `cybersin init .` first"
        );
    };

    let path = project_root.join(LOCAL_CONFIG_FILENAME);
    let mut root = if path.is_file() {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_yaml::from_str::<Value>(&text)
            .with_context(|| format!("parsing {}", path.display()))?
    } else {
        Value::Mapping(Mapping::new())
    };

    let mut changed = false;
    changed |= set_missing_string(
        &mut root,
        &["providers", OPENROUTER_PROVIDER, "availability"],
        "available",
    );
    changed |= set_missing_string(
        &mut root,
        &["providers", OPENROUTER_PROVIDER, "api_key"],
        &format!("${{{env_var}}}"),
    );
    changed |= set_missing_string(&mut root, &["defaults", "provider"], OPENROUTER_PROVIDER);
    changed |= set_missing_string(&mut root, &["defaults", "model"], &args.model);
    changed |= append_unique_string(
        &mut root,
        &["permissions", "routing", "allowed_providers"],
        OPENROUTER_PROVIDER,
    );

    if changed || !path.is_file() {
        let text = serde_yaml::to_string(&root).context("serializing local config")?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        println!("Updated {}", path.display());
    } else {
        println!("{} already up to date", path.display());
    }

    let report = doctor::diagnose(&project_root)?;
    println!();
    println!("{}", report.render());
    if report.ok {
        Ok(())
    } else {
        anyhow::bail!("project is not ready; see next actions above")
    }
}

fn normalize_env_var(raw: &str) -> anyhow::Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        anyhow::bail!("--openrouter-api-key-env cannot be empty");
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        anyhow::bail!(
            "--openrouter-api-key-env must be an environment variable name such as {OPENROUTER_API_KEY_ENV}"
        );
    }
    Ok(value.to_string())
}

fn set_missing_string(root: &mut Value, path: &[&str], value: &str) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let parent = ensure_mapping_path(root, parents);
    let key = Value::String((*last).to_string());
    if parent.contains_key(&key) {
        return false;
    }
    parent.insert(key, Value::String(value.to_string()));
    true
}

fn append_unique_string(root: &mut Value, path: &[&str], value: &str) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let parent = ensure_mapping_path(root, parents);
    let key = Value::String((*last).to_string());
    match parent.get_mut(&key) {
        Some(Value::Sequence(items)) => {
            if items.iter().any(|item| item.as_str() == Some(value)) {
                false
            } else {
                items.push(Value::String(value.to_string()));
                true
            }
        }
        Some(_) => false,
        None => {
            parent.insert(key, Value::Sequence(vec![Value::String(value.to_string())]));
            true
        }
    }
}

fn ensure_mapping_path<'a>(root: &'a mut Value, path: &[&str]) -> &'a mut Mapping {
    if !matches!(root, Value::Mapping(_)) {
        *root = Value::Mapping(Mapping::new());
    }
    let mut current = root.as_mapping_mut().expect("root is a mapping");
    for part in path {
        let key = Value::String((*part).to_string());
        let value = current
            .entry(key)
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if !matches!(value, Value::Mapping(_)) {
            *value = Value::Mapping(Mapping::new());
        }
        current = value.as_mapping_mut().expect("path value is a mapping");
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_existing_values_when_filling_defaults() {
        let mut value: Value = serde_yaml::from_str(
            "providers:\n  openrouter:\n    api_key: ${CUSTOM_KEY}\nunknown:\n  keep: true\n",
        )
        .unwrap();

        assert!(!set_missing_string(
            &mut value,
            &["providers", OPENROUTER_PROVIDER, "api_key"],
            "${OPENROUTER_API_KEY}",
        ));
        assert!(set_missing_string(
            &mut value,
            &["providers", OPENROUTER_PROVIDER, "availability"],
            "available",
        ));

        assert_eq!(
            value["providers"][OPENROUTER_PROVIDER]["api_key"].as_str(),
            Some("${CUSTOM_KEY}")
        );
        assert_eq!(value["unknown"]["keep"].as_bool(), Some(true));
    }
}
