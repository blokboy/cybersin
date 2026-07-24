//! `cybersin build [--profile dev|release] [--frozen] [--watch]` (spec
//! §6, §11): runs every `*.prompt.yaml` in a project through the
//! frontend + optimizer pipeline, the router, and the backends, and
//! writes the full `dist/` tree (spec §6.6): `manifest.json`,
//! `prompts/`, `routing.json`, `budget/`, `cache.json`, `evals/`.
//!
//! **`--frozen` is the CI mode (spec §7): it must fail before any pass
//! makes a network call.** Tracing what actually can: `compress` is the
//! only pass with a live provider call, gated by its existing
//! [`CompressMode::Frozen`]. `cybersin-router::compile_from_yaml` is a
//! pure function over already-in-memory YAML/IR (no network, no I/O
//! beyond what the caller already read) — it always runs with
//! `observed: None` here, since reading real trace statistics back into
//! routing is `cybersin optimize`'s job (a later issue), not a build-
//! time network call. `passes::Budget` is likewise pure IR math. So
//! `frozen` only needs to gate `Compress`'s mode; nothing else in this
//! pipeline can reach the network, and this file threads it nowhere
//! else.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use clap::ValueEnum;
use cybersin_backends::backend_for;
use cybersin_frontend::{compile_prompt_source, discover_prompt_sources};
use cybersin_ir::PromptIr;
use cybersin_passes::{
    build_pipeline_with_compress_and_targets, build_pipeline_with_targets, run_pipeline,
    write_budget_artifact, Compress, CompressError, CompressMode, CompressionLock,
    CompressionProvider, Profile, TargetBudget,
};
use cybersin_router::{compile_from_yaml, emit_routing_json, WorkloadEstimate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::git;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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

/// The subset of `cybersin.yaml` this command reads beyond what
/// `cybersin-router::compile_from_yaml` already parses (its
/// `cost_model`): which render targets a build produces backend output
/// for. Defaults to `["generic"]` — spec §6.5's "`--target generic`
/// retained for portability" — when a project hasn't declared any.
#[derive(Debug, Default, Deserialize)]
struct ProjectManifest {
    #[serde(default)]
    targets: Vec<String>,
}

/// Default per-target context budget (spec §6.2), used for every
/// configured render target until `cybersin.yaml` carries per-model
/// context windows (not yet — out of this issue's scope).
const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 128_000;
const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 4_096;

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

/// Build `project`'s `dist/` in place.
pub fn run(project: &Path, profile: BuildProfile, frozen: bool) -> Result<Option<String>, String> {
    run_into(project, &project.join("dist"), profile, frozen)
}

/// Build `project`'s sources into `dist_dir`, which need not be
/// `project/dist` — `cybersin diff` builds two git refs into throwaway
/// directories this way, to compare them without touching either
/// working tree's real `dist/`.
pub fn run_into(
    project: &Path,
    dist_dir: &Path,
    profile: BuildProfile,
    frozen: bool,
) -> Result<Option<String>, String> {
    let project_yaml_path = project.join("cybersin.yaml");
    let project_yaml = fs::read_to_string(&project_yaml_path)
        .map_err(|e| format!("error: failed to read {}: {e}", project_yaml_path.display()))?;
    let manifest_config: ProjectManifest = serde_yaml::from_str(&project_yaml)
        .map_err(|e| format!("error: invalid {}: {e}", project_yaml_path.display()))?;
    let targets = if manifest_config.targets.is_empty() {
        vec!["generic".to_string()]
    } else {
        manifest_config.targets
    };

    let lock_path = project.join("cybersin.lock");
    let lock_text = fs::read_to_string(&lock_path)
        .map_err(|e| format!("error: failed to read {}: {e}", lock_path.display()))?;
    let mut lockfile: Lockfile = serde_yaml::from_str(&lock_text)
        .map_err(|e| format!("error: invalid {}: {e}", lock_path.display()))?;
    let compression_lock = Arc::new(Mutex::new(std::mem::take(&mut lockfile.passes)));

    let target_budgets: Vec<TargetBudget> = targets
        .iter()
        .map(|target| {
            TargetBudget::new(
                target.clone(),
                DEFAULT_CONTEXT_WINDOW_TOKENS,
                DEFAULT_RESERVED_OUTPUT_TOKENS,
            )
        })
        .collect();

    let pass_profile = match profile {
        BuildProfile::Dev => Profile::Dev,
        BuildProfile::Release => Profile::Release,
    };
    let pipeline = if pass_profile == Profile::Dev {
        build_pipeline_with_targets(pass_profile, target_budgets)
    } else {
        build_pipeline_with_compress_and_targets(
            pass_profile,
            Compress::new(
                Arc::new(UnavailableProvider),
                Arc::clone(&compression_lock),
                if frozen {
                    CompressMode::Frozen
                } else {
                    CompressMode::Update
                },
            ),
            target_budgets,
        )
    };

    let sources = discover_prompt_sources(project)
        .map_err(|e| format!("error: failed to discover prompts: {e}"))?;
    if sources.is_empty() {
        return Err("error: no *.prompt.yaml sources found".into());
    }

    // Every build fully determines dist/ from sources + lockfile (spec
    // §7); starting from a clean directory means a renamed/removed
    // prompt, or a target dropped from `cybersin.yaml`, can't leave a
    // stale artifact behind.
    if dist_dir.exists() {
        fs::remove_dir_all(dist_dir)
            .map_err(|e| format!("error: failed to clear {}: {e}", dist_dir.display()))?;
    }
    let prompt_dir = dist_dir.join("prompts");
    fs::create_dir_all(&prompt_dir)
        .map_err(|e| format!("error: failed to create {}: {e}", prompt_dir.display()))?;

    let mut compiled: Vec<PromptIr> = Vec::new();
    for source in &sources {
        let ir = compile_prompt_source(source).map_err(|e| format!("error: {e}"))?;
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

        let prompt_json = to_pretty_json(&outcome.ir)?;
        fs::write(
            prompt_dir.join(format!("{}.json", outcome.ir.name)),
            &prompt_json,
        )
        .map_err(|e| format!("error: failed to write prompt: {e}"))?;

        if let Some(budget) = &outcome.budget {
            write_budget_artifact(dist_dir, budget)
                .map_err(|e| format!("error: failed to write budget artifact: {e}"))?;
        }

        // Per-target rendered output (spec §6.5) lives alongside the
        // canonical optimized IR: `prompts/<name>.json` stays the
        // model-agnostic compiled prompt, `prompts/<name>/<target>.json`
        // is what that one backend actually sends.
        let render_dir = prompt_dir.join(&outcome.ir.name);
        fs::create_dir_all(&render_dir)
            .map_err(|e| format!("error: failed to create {}: {e}", render_dir.display()))?;
        for target in &targets {
            let backend = backend_for(target).map_err(|e| format!("error: {e}"))?;
            let rendered = backend.render(&outcome.ir).map_err(|e| {
                format!(
                    "error: {target} backend rejected prompt {:?}: {e}",
                    outcome.ir.name
                )
            })?;
            let bytes = to_pretty_json(&rendered)?;
            fs::write(render_dir.join(format!("{target}.json")), &bytes)
                .map_err(|e| format!("error: failed to write rendered prompt: {e}"))?;
        }

        compiled.push(outcome.ir);
    }

    // Cold start: no observed trace statistics exist yet at compile
    // time (that's `cybersin optimize`'s job, reading real traces — a
    // later issue), so routing always compiles from declared
    // `cybersin.yaml` defaults.
    let routing = compile_from_yaml(
        &compiled,
        &project_yaml,
        &lock_text,
        None,
        WorkloadEstimate::default(),
    )
    .map_err(|e| format!("error: {e}"))?;
    emit_routing_json(&dist_dir.join("routing.json"), &routing)
        .map_err(|e| format!("error: failed to write routing.json: {e}"))?;

    write_cache_json(dist_dir)?;

    // Eval compilation is issue #21; an empty directory is enough to
    // round out spec §6.6's dist/ shape for now.
    let evals_dir = dist_dir.join("evals");
    fs::create_dir_all(&evals_dir)
        .map_err(|e| format!("error: failed to create {}: {e}", evals_dir.display()))?;

    write_manifest(project, dist_dir, &project_yaml, &lock_text, &sources)?;

    lockfile.passes = compression_lock.lock().unwrap().clone();
    if !frozen {
        let yaml = serde_yaml::to_string(&lockfile)
            .map_err(|e| format!("error: failed to serialize lockfile: {e}"))?;
        fs::write(&lock_path, yaml)
            .map_err(|e| format!("error: failed to write {}: {e}", lock_path.display()))?;
    }
    Ok(Some(format!("built {}", project.display())))
}

fn to_pretty_json<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|e| format!("error: failed to serialize: {e}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// `dist/cache.json`'s minimal seed shape (spec §6.6: "`cache.json`
/// (with `namespace_version`)"). Real entries come from the runtime
/// route/cache executor at call time (issue #15, out of this issue's
/// scope), not from the compiler — a build always re-seeds an empty
/// cache rather than merging with whatever the runtime last wrote,
/// keeping `dist/` fully a deterministic function of sources + lockfile
/// (spec §7).
#[derive(Serialize)]
struct CacheSeed {
    schema_version: u32,
    namespace_version: u32,
    entries: BTreeMap<String, Value>,
}

fn write_cache_json(dist_dir: &Path) -> Result<(), String> {
    let seed = CacheSeed {
        schema_version: 1,
        namespace_version: 1,
        entries: BTreeMap::new(),
    };
    let bytes = to_pretty_json(&seed)?;
    fs::write(dist_dir.join("cache.json"), bytes)
        .map_err(|e| format!("error: failed to write cache.json: {e}"))
}

/// `dist/manifest.json` (spec §6.6): build hash, git SHA, lockfile
/// hash, per-artifact content hashes.
#[derive(Serialize)]
struct Manifest {
    schema_version: u32,
    build_hash: String,
    git_sha: String,
    lockfile_hash: String,
    /// Every other file under `dist/`, keyed by its path relative to
    /// `dist/` (forward-slash separated regardless of platform), so
    /// `cybersin diff`-style comparisons can spot a changed artifact by
    /// hash alone. `BTreeMap` keeps this sorted for byte-identical
    /// rebuilds (spec §7).
    artifacts: BTreeMap<String, String>,
}

fn write_manifest(
    project: &Path,
    dist_dir: &Path,
    project_yaml: &str,
    lock_text: &str,
    sources: &[PathBuf],
) -> Result<(), String> {
    let artifacts = hash_dist_tree(dist_dir)?;
    let manifest = Manifest {
        schema_version: 1,
        build_hash: compute_build_hash(project, project_yaml, lock_text, sources)?,
        git_sha: git::sha(project),
        lockfile_hash: hex_sha256(lock_text.as_bytes()),
        artifacts,
    };
    let bytes = to_pretty_json(&manifest)?;
    fs::write(dist_dir.join("manifest.json"), bytes)
        .map_err(|e| format!("error: failed to write manifest.json: {e}"))
}

/// A deterministic hash of the build's actual inputs — `cybersin.yaml`,
/// `cybersin.lock`, and every discovered `*.prompt.yaml` source, by
/// content — rather than a random id or wall-clock timestamp, so two
/// builds of the same sources+lockfile produce the same `build_hash`
/// (spec §7's byte-identical rebuild guarantee).
fn compute_build_hash(
    project: &Path,
    project_yaml: &str,
    lock_text: &str,
    sources: &[PathBuf],
) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"cybersin.build.v1\0");
    hasher.update(project_yaml.as_bytes());
    hasher.update([0u8]);
    hasher.update(lock_text.as_bytes());
    // `discover_prompt_sources` already returns these sorted.
    for source in sources {
        let bytes = fs::read(source)
            .map_err(|e| format!("error: failed to read {}: {e}", source.display()))?;
        let relative = source.strip_prefix(project).unwrap_or(source);
        hasher.update([0u8]);
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0u8]);
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn hash_dist_tree(dist_dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut artifacts = BTreeMap::new();
    walk_hash(dist_dir, dist_dir, &mut artifacts)?;
    Ok(artifacts)
}

fn walk_hash(
    base: &Path,
    dir: &Path,
    artifacts: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("error: failed to read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("error: failed to read {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            walk_hash(base, &path, artifacts)?;
        } else {
            let bytes = fs::read(&path)
                .map_err(|e| format!("error: failed to read {}: {e}", path.display()))?;
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            artifacts.insert(relative, hex_sha256(&bytes));
        }
    }
    Ok(())
}

/// Poll interval for `--watch`'s mtime-based change detection. No
/// file-watcher crate (e.g. `notify`) is a workspace dependency today;
/// polling every 300ms is simple, portable, and fast enough for a local
/// dev loop, without adding a dependency for one feature (CLAUDE.md: no
/// speculative generality).
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Rebuild `project` once, then again every time a watched source
/// changes, until `max_builds` rebuilds have run (`None` means forever
/// — real CLI usage, stopped by the user killing the process; a finite
/// bound is what makes this testable). Each build's outcome is reported
/// through `on_result` as it happens rather than collected and
/// returned, since a `--watch` invocation may run arbitrarily many
/// builds.
pub fn watch(
    project: &Path,
    profile: BuildProfile,
    frozen: bool,
    max_builds: Option<u32>,
    mut on_result: impl FnMut(Result<Option<String>, String>),
) -> Result<(), String> {
    let mut last = watched_snapshot(project)?;
    on_result(run(project, profile, frozen));
    let mut builds: u32 = 1;
    loop {
        if let Some(max) = max_builds {
            if builds >= max {
                return Ok(());
            }
        }
        std::thread::sleep(WATCH_POLL_INTERVAL);
        let snapshot = watched_snapshot(project)?;
        if snapshot != last {
            last = snapshot;
            on_result(run(project, profile, frozen));
            builds += 1;
        }
    }
}

/// `cybersin build --watch`'s CLI entry point: watches forever, printing
/// each build's result as it happens, matching `run`'s `Ok(message)`/
/// `Err(message)` convention for the one-shot build above it.
pub fn watch_cli(
    project: &Path,
    profile: BuildProfile,
    frozen: bool,
) -> Result<Option<String>, String> {
    watch(project, profile, frozen, None, |result| match result {
        Ok(Some(message)) => println!("{message}"),
        Ok(None) => {}
        Err(message) => eprintln!("{message}"),
    })?;
    Ok(None)
}

/// mtimes of everything `--watch` rebuilds on: `cybersin.yaml`,
/// `cybersin.lock`, and every discovered `*.prompt.yaml` source (spec
/// §11's `--watch`).
fn watched_snapshot(project: &Path) -> Result<BTreeMap<PathBuf, SystemTime>, String> {
    let mut snapshot = BTreeMap::new();
    for name in ["cybersin.yaml", "cybersin.lock"] {
        let path = project.join(name);
        if let Ok(metadata) = fs::metadata(&path) {
            let modified = metadata
                .modified()
                .map_err(|e| format!("error: failed to stat {name}: {e}"))?;
            snapshot.insert(path, modified);
        }
    }
    for source in discover_prompt_sources(project)
        .map_err(|e| format!("error: failed to discover prompts: {e}"))?
    {
        let metadata = fs::metadata(&source)
            .map_err(|e| format!("error: failed to stat {}: {e}", source.display()))?;
        let modified = metadata
            .modified()
            .map_err(|e| format!("error: failed to stat {}: {e}", source.display()))?;
        snapshot.insert(source, modified);
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn init_project(dir: &Path) {
        crate::commands::init::run(dir).expect("init");
    }

    #[test]
    fn dev_build_writes_the_full_dist_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        init_project(&project);

        run(&project, BuildProfile::Dev, true).expect("build");

        let dist = project.join("dist");
        assert!(dist.join("manifest.json").exists());
        assert!(dist.join("routing.json").exists());
        assert!(dist.join("cache.json").exists());
        assert!(dist.join("evals").is_dir());
        assert!(dist.join("prompts/hello.json").exists());
        assert!(dist.join("prompts/hello/generic.json").exists());
        assert!(dist.join("budget/hello.json").exists());

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dist.join("manifest.json")).unwrap()).unwrap();
        assert!(!manifest["build_hash"].as_str().unwrap().is_empty());
        assert!(manifest["artifacts"]
            .as_object()
            .unwrap()
            .contains_key("routing.json"));
    }

    #[test]
    fn two_builds_of_the_same_sources_and_lockfile_are_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        init_project(&project);

        let dist_a = tmp.path().join("dist_a");
        let dist_b = tmp.path().join("dist_b");
        run_into(&project, &dist_a, BuildProfile::Dev, true).expect("build a");
        run_into(&project, &dist_b, BuildProfile::Dev, true).expect("build b");

        let files_a = collect_relative_files(&dist_a);
        let files_b = collect_relative_files(&dist_b);
        assert_eq!(
            files_a.keys().collect::<Vec<_>>(),
            files_b.keys().collect::<Vec<_>>()
        );
        for (path, bytes_a) in &files_a {
            let bytes_b = &files_b[path];
            assert_eq!(bytes_a, bytes_b, "{path} differs between builds");
        }
    }

    fn collect_relative_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect(dir, dir, &mut files);
        files
    }

    fn collect(base: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(base, &path, files);
            } else {
                let relative = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, fs::read(&path).unwrap());
            }
        }
    }

    #[test]
    fn frozen_release_build_fails_without_a_pinned_compression_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        init_project(&project);

        let error = run(&project, BuildProfile::Release, true).unwrap_err();
        assert!(
            error.contains("would require a network call"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn watch_rebuilds_when_a_prompt_source_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        init_project(&project);
        let prompt_path = project.join("prompts/hello.prompt.yaml");

        let (tx, rx) = mpsc::channel();
        let watched_project = project.clone();
        let handle = std::thread::spawn(move || {
            watch(
                &watched_project,
                BuildProfile::Dev,
                true,
                Some(2),
                |result| {
                    tx.send(result).unwrap();
                },
            )
        });

        // Give the watcher time to take its first snapshot before the
        // source changes, then touch it with new, observable content.
        std::thread::sleep(Duration::from_millis(100));
        let updated = fs::read_to_string(&prompt_path)
            .unwrap()
            .replace("warmly", "enthusiastically");
        fs::write(&prompt_path, updated).unwrap();

        let first = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("initial build result");
        assert!(first.is_ok());
        let second = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("rebuild result after source change");
        assert!(second.is_ok());
        handle.join().unwrap().expect("watch loop");

        let rendered = fs::read_to_string(project.join("dist/prompts/hello/generic.json")).unwrap();
        assert!(rendered.contains("enthusiastically"));
    }
}
