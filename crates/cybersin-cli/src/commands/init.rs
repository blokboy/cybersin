//! `cybersin init <dir>` (spec §11, §5): safely creates the basic project
//! spine: core config, lockfile, local-config example, ignore rules, and
//! empty source directories. Plain init deliberately avoids starter
//! prompt/eval/agent/harness/sample-input files and build output.

use std::fs;
use std::path::Path;

const CYBERSIN_YAML: &str = r#"# Cybersin project config (spec §5, §6.3 cost model).
name: myagent
targets:
  - generic
cost_model:
  # Cold-start cache-similarity threshold and judge-trigger band (spec
  # §6.3): no observed traces exist yet on a first build, so these start
  # at conservative static defaults, biased toward false cache-misses
  # (never false cache-hits). `cybersin optimize` tightens or loosens
  # them later from real trace data.
  cache_similarity_threshold: 0.97
  judge_trigger_band: [0.90, 0.97]
  judge_model: cache-judge
storage:
  backend: sqlite
sandbox:
  backend: docker+gvisor
"#;

const CYBERSIN_LOCK: &str = r#"# Pinned models, prices, embedding model, and model-assisted pass
# outputs (spec §7). Replace these stub pins with real model pins before
# shipping. `passes` stays empty until a release build runs a
# model-assisted pass or `cybersin lock update` pins one.
models:
  stub-medium:
    provider: stub
    quality: medium
prices:
  stub-medium:
    usd_per_1k_prompt_tokens: 1.0
    usd_per_1k_completion_tokens: 2.0
passes: {}
"#;

const CYBERSIN_LOCAL_EXAMPLE: &str = r#"# Copy to cybersin.local.yaml for machine-local runtime settings.
# This file is safe to commit; cybersin.local.yaml is ignored.
runtime: {}
"#;

const GITIGNORE: &str = r#"/.cybersin/
/cybersin.local.yaml
/dist/
"#;

#[derive(Clone, Copy, Debug, Default)]
pub struct InitOptions {
    pub force: bool,
    pub dry_run: bool,
}

enum ScaffoldEntry {
    File(&'static str, &'static str),
    Dir(&'static str),
}

impl ScaffoldEntry {
    fn rel(&self) -> &'static str {
        match self {
            ScaffoldEntry::File(rel, _) | ScaffoldEntry::Dir(rel) => rel,
        }
    }
}

const SCAFFOLD: &[ScaffoldEntry] = &[
    ScaffoldEntry::File("cybersin.yaml", CYBERSIN_YAML),
    ScaffoldEntry::File("cybersin.lock", CYBERSIN_LOCK),
    ScaffoldEntry::File("cybersin.local.example.yaml", CYBERSIN_LOCAL_EXAMPLE),
    ScaffoldEntry::File(".gitignore", GITIGNORE),
    ScaffoldEntry::Dir("prompts"),
    ScaffoldEntry::Dir("fragments"),
    ScaffoldEntry::Dir("evals"),
    ScaffoldEntry::Dir("agents"),
    ScaffoldEntry::Dir("tools"),
];

#[cfg(test)]
pub fn run(dir: &Path) -> Result<Option<String>, String> {
    run_with_options(dir, InitOptions::default())
}

pub fn run_with_options(dir: &Path, options: InitOptions) -> Result<Option<String>, String> {
    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for entry in SCAFFOLD {
        let rel = entry.rel();
        let path = dir.join(rel);
        let exists = path.exists();
        if exists && !options.force {
            skipped.push(rel);
            continue;
        }

        created.push(rel);
        if options.dry_run {
            continue;
        }

        match entry {
            ScaffoldEntry::File(_, contents) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        format!("error: failed to create {}: {e}", parent.display())
                    })?;
                }
                fs::write(&path, contents)
                    .map_err(|e| format!("error: failed to write {}: {e}", path.display()))?;
            }
            ScaffoldEntry::Dir(_) => {
                fs::create_dir_all(&path)
                    .map_err(|e| format!("error: failed to create {}: {e}", path.display()))?;
            }
        }
    }

    let verb = if options.dry_run {
        "would scaffold"
    } else {
        "scaffolded"
    };
    let mut message = format!("{verb} cybersin project spine at {}", dir.display());
    message.push_str("\ncreated:");
    if created.is_empty() {
        message.push_str(" none");
    } else {
        for rel in created {
            message.push_str(&format!("\n  {rel}"));
        }
    }
    message.push_str("\nskipped:");
    if skipped.is_empty() {
        message.push_str(" none");
    } else {
        for rel in skipped {
            message.push_str(&format!("\n  {rel}"));
        }
    }
    Ok(Some(message))
}
