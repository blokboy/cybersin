//! Project-root discovery for runtime commands (issue #50). Every runtime
//! subcommand (`run`/`trace`/`cost`/`explain`/`dlq`/`approve`/`deny`/
//! `sessions`/`notify`/`optimize`) resolves its `--db`/`--dist`/
//! `--sandbox-root`/`--sandbox-backend` defaults against the project it's
//! standing inside of — found by walking up from the current directory for
//! a `cybersin.yaml` — rather than only ever looking at plain CWD-relative
//! paths. An explicit CLI flag always overrides its corresponding default.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::ValueEnum;
use serde::Deserialize;

use crate::commands::sandbox::Backend;

/// Walk up from `start` (inclusive) looking for a directory containing
/// `cybersin.yaml`. Returns `None` if no ancestor — up to and including the
/// filesystem root — has one. `start` should be absolute (e.g.
/// `std::env::current_dir()`'s result) so the walk actually reaches the
/// real filesystem root rather than a relative path's synthetic one.
pub fn discover_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("cybersin.yaml").is_file())
        .map(Path::to_path_buf)
}

/// The subset of `cybersin.yaml` this module reads: `sandbox.backend`.
/// Every other top-level key (`name`, `targets`, `cost_model`, `storage`,
/// ...) is parsed by serde_yaml and dropped on the floor — no
/// `#[serde(deny_unknown_fields)]` — matching the convention already
/// established by `harness_config::AgentYaml` and
/// `cybersin_router::ProjectConfig`.
#[derive(Debug, Default, Deserialize)]
struct ProjectYaml {
    #[serde(default)]
    sandbox: SandboxSection,
}

#[derive(Debug, Default, Deserialize)]
struct SandboxSection {
    backend: Option<String>,
}

/// This invocation's resolved project context. `root` is the nearest
/// ancestor of the starting directory containing `cybersin.yaml`, or the
/// starting directory itself if none was found — so every `*_default()`
/// below degrades to today's plain CWD-relative behavior when no project is
/// discovered.
pub struct ProjectDefaults {
    root: PathBuf,
    discovered: bool,
    sandbox_backend: Option<Backend>,
}

impl ProjectDefaults {
    pub fn detect(cwd: &Path) -> anyhow::Result<Self> {
        match discover_project_root(cwd) {
            Some(root) => {
                let yaml_path = root.join("cybersin.yaml");
                let text = std::fs::read_to_string(&yaml_path)
                    .with_context(|| format!("reading {}", yaml_path.display()))?;
                let parsed: ProjectYaml = serde_yaml::from_str(&text)
                    .with_context(|| format!("invalid {}", yaml_path.display()))?;
                let sandbox_backend = parsed
                    .sandbox
                    .backend
                    .map(|s| {
                        Backend::from_str(&s, true).map_err(|e| {
                            anyhow::anyhow!(
                                "invalid sandbox.backend {s:?} in {}: {e}",
                                yaml_path.display()
                            )
                        })
                    })
                    .transpose()?;
                Ok(Self {
                    root,
                    discovered: true,
                    sandbox_backend,
                })
            }
            None => Ok(Self {
                root: cwd.to_path_buf(),
                discovered: false,
                sandbox_backend: None,
            }),
        }
    }

    pub fn db_default(&self) -> PathBuf {
        self.root.join(".cybersin/cybersin.db")
    }

    pub fn sandbox_root_default(&self) -> PathBuf {
        self.root.join(".cybersin/sandbox")
    }

    pub fn sandbox_backend_default(&self) -> Backend {
        self.sandbox_backend.unwrap_or(Backend::DockerGvisor)
    }

    /// Load `<project root>/.env` if present, without overriding values
    /// the caller's environment already set. This keeps existing
    /// env-var configuration authoritative while letting local projects
    /// provide optional readiness inputs before providers/tools inspect
    /// the environment.
    pub fn load_dotenv(&self) -> anyhow::Result<()> {
        let path = self.root.join(".env");
        if !path.is_file() {
            return Ok(());
        }
        for item in
            dotenvy::from_path_iter(&path).with_context(|| format!("reading {}", path.display()))?
        {
            let (key, value) = item.with_context(|| format!("parsing {}", path.display()))?;
            if std::env::var_os(&key).is_none() {
                std::env::set_var(key, value);
            }
        }
        Ok(())
    }

    /// `<root>/dist` if it's a real, already-built directory. Errors —
    /// rather than silently substituting the bundled stub fixture — when it
    /// isn't, whether or not a project root was actually discovered.
    pub fn dist_default(&self) -> anyhow::Result<PathBuf> {
        let candidate = self.root.join("dist");
        if candidate.is_dir() {
            Ok(candidate)
        } else if self.discovered {
            anyhow::bail!(
                "no dist/ found; run `cybersin build {}` first",
                self.root.display()
            )
        } else {
            anyhow::bail!("no dist/ found; run `cybersin build` first")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::write(path, "").unwrap();
    }

    #[test]
    fn finds_cybersin_yaml_in_start_dir() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("cybersin.yaml"));

        assert_eq!(
            discover_project_root(dir.path()),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn finds_cybersin_yaml_several_levels_up() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("cybersin.yaml"));
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            discover_project_root(&nested),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(discover_project_root(&nested), None);
    }

    #[test]
    fn defaults_fall_back_to_start_dir_when_undiscovered() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = ProjectDefaults::detect(dir.path()).unwrap();

        assert_eq!(
            defaults.db_default(),
            dir.path().join(".cybersin/cybersin.db")
        );
        assert_eq!(
            defaults.sandbox_root_default(),
            dir.path().join(".cybersin/sandbox")
        );
        assert!(matches!(
            defaults.sandbox_backend_default(),
            Backend::DockerGvisor
        ));
    }

    #[test]
    fn dist_default_errors_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("cybersin.yaml"));
        let defaults = ProjectDefaults::detect(dir.path()).unwrap();

        let err = defaults.dist_default().unwrap_err();
        assert!(err.to_string().contains("no dist/ found"));
        assert!(err.to_string().contains("cybersin build"));
    }

    #[test]
    fn dist_default_resolves_when_present() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("cybersin.yaml"));
        std::fs::create_dir(dir.path().join("dist")).unwrap();
        let defaults = ProjectDefaults::detect(dir.path()).unwrap();

        assert_eq!(defaults.dist_default().unwrap(), dir.path().join("dist"));
    }

    #[test]
    fn parses_sandbox_backend_from_yaml_both_spellings() {
        for spelling in ["docker-gvisor", "docker+gvisor"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("cybersin.yaml"),
                format!("sandbox:\n  backend: {spelling}\n"),
            )
            .unwrap();
            let defaults = ProjectDefaults::detect(dir.path()).unwrap();

            assert!(
                matches!(defaults.sandbox_backend_default(), Backend::DockerGvisor),
                "spelling {spelling:?} should parse as DockerGvisor"
            );
        }
    }

    #[test]
    fn plain_docker_backend_from_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cybersin.yaml"),
            "sandbox:\n  backend: docker\n",
        )
        .unwrap();
        let defaults = ProjectDefaults::detect(dir.path()).unwrap();

        assert!(matches!(
            defaults.sandbox_backend_default(),
            Backend::Docker
        ));
    }

    #[test]
    fn dotenv_loading_sets_missing_values_without_overriding_environment() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("cybersin.yaml"));
        std::fs::write(
            dir.path().join(".env"),
            "CYBERSIN_TEST_DOTENV_ONLY=from-dotenv\nCYBERSIN_TEST_DOTENV_KEEP=from-dotenv\n",
        )
        .unwrap();
        std::env::remove_var("CYBERSIN_TEST_DOTENV_ONLY");
        std::env::set_var("CYBERSIN_TEST_DOTENV_KEEP", "from-env");

        let defaults = ProjectDefaults::detect(dir.path()).unwrap();
        defaults.load_dotenv().unwrap();

        assert_eq!(
            std::env::var("CYBERSIN_TEST_DOTENV_ONLY").unwrap(),
            "from-dotenv"
        );
        assert_eq!(
            std::env::var("CYBERSIN_TEST_DOTENV_KEEP").unwrap(),
            "from-env"
        );

        std::env::remove_var("CYBERSIN_TEST_DOTENV_ONLY");
        std::env::remove_var("CYBERSIN_TEST_DOTENV_KEEP");
    }
}
