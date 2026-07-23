use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::ValueEnum;
use cybersin_frontend::{compile_prompt_source, discover_prompt_sources};
use cybersin_passes::{
    build_pipeline, build_pipeline_with_compress, run_pipeline, Compress, CompressError,
    CompressMode, CompressionLock, CompressionProvider, Profile,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BuildProfile {
    Dev,
    Release,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Lockfile {
    #[serde(default)]
    models: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    prices: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    passes: CompressionLock,
}

struct UnavailableProvider;

impl CompressionProvider for UnavailableProvider {
    fn model(&self) -> &str {
        "unconfigured"
    }

    fn compress(&self, _input: &str) -> Result<String, CompressError> {
        Err(CompressError(
            "no compression provider configured; pin outputs before building".into(),
        ))
    }
}

pub fn run(project: &Path, profile: BuildProfile, frozen: bool) -> Result<Option<String>, String> {
    let lock_path = project.join("cybersin.lock");
    let lock_text = fs::read_to_string(&lock_path)
        .map_err(|e| format!("error: failed to read {}: {e}", lock_path.display()))?;
    let mut lockfile: Lockfile = serde_yaml::from_str(&lock_text)
        .map_err(|e| format!("error: invalid {}: {e}", lock_path.display()))?;
    let compression_lock = Arc::new(Mutex::new(std::mem::take(&mut lockfile.passes)));
    let profile = match profile {
        BuildProfile::Dev => Profile::Dev,
        BuildProfile::Release => Profile::Release,
    };
    let pipeline = if profile == Profile::Dev {
        build_pipeline(profile)
    } else {
        build_pipeline_with_compress(
            profile,
            Compress::new(
                Arc::new(UnavailableProvider),
                Arc::clone(&compression_lock),
                if frozen {
                    CompressMode::Frozen
                } else {
                    CompressMode::Update
                },
            ),
        )
    };

    let sources = discover_prompt_sources(project)
        .map_err(|e| format!("error: failed to discover prompts: {e}"))?;
    if sources.is_empty() {
        return Err("error: no *.prompt.yaml sources found".into());
    }
    let prompt_dir = project.join("dist/prompts");
    fs::create_dir_all(&prompt_dir)
        .map_err(|e| format!("error: failed to create {}: {e}", prompt_dir.display()))?;
    for source in sources {
        let ir = compile_prompt_source(&source).map_err(|e| format!("error: {e}"))?;
        let outcome = run_pipeline(&pipeline, ir, None);
        if outcome.has_error() {
            let messages = outcome
                .diagnostics
                .iter()
                .map(|d| format!("{}: {}", d.pass, d.message))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!("error: build failed\n{messages}"));
        }
        let output = serde_json::to_string_pretty(&outcome.ir)
            .map_err(|e| format!("error: failed to serialize prompt: {e}"))?;
        fs::write(prompt_dir.join(format!("{}.json", outcome.ir.name)), output)
            .map_err(|e| format!("error: failed to write prompt: {e}"))?;
    }

    lockfile.passes = compression_lock.lock().unwrap().clone();
    if !frozen {
        let yaml = serde_yaml::to_string(&lockfile)
            .map_err(|e| format!("error: failed to serialize lockfile: {e}"))?;
        fs::write(&lock_path, yaml)
            .map_err(|e| format!("error: failed to write {}: {e}", lock_path.display()))?;
    }
    Ok(Some(format!("built {}", project.display())))
}
