//! `cybersin doctor`: setup-readiness report for a local project.

use std::path::{Path, PathBuf};

use anyhow::Context;
use cybersin_runtime::{allowlist::LOCAL_CONFIG_FILENAME, Availability, LocalConfigFile};

use crate::project::{discover_project_root, ProjectDefaults};
use crate::readiness::{self, DotenvReadiness};

#[derive(Clone, Debug)]
pub struct DoctorReport {
    pub ok: bool,
    lines: Vec<String>,
}

impl DoctorReport {
    pub fn render(&self) -> String {
        self.lines.join("\n")
    }
}

pub fn execute(project_start: &Path) -> anyhow::Result<()> {
    let report = diagnose(project_start)?;
    println!("{}", report.render());
    if report.ok {
        Ok(())
    } else {
        anyhow::bail!("project is not ready; see next actions above")
    }
}

pub fn diagnose(project_start: &Path) -> anyhow::Result<DoctorReport> {
    let Some(project_root) = discover_project_root(project_start) else {
        let lines = vec![
            "Cybersin doctor".to_string(),
            format!(
                "project: missing (searched from {})",
                project_start.display()
            ),
            "".to_string(),
            "Project spine/config: missing".to_string(),
            "  [fail] no cybersin.yaml found in this directory or its parents".to_string(),
            "".to_string(),
            "Next actions:".to_string(),
            "  - Run `cybersin init .` from the directory that should become the project root."
                .to_string(),
        ];
        return Ok(DoctorReport { ok: false, lines });
    };

    let defaults = ProjectDefaults::detect(&project_root)?;
    defaults.load_dotenv()?;
    let dotenv = DotenvReadiness::load(&project_root)?;
    let local_config = LocalConfigFile::load_optional(&project_root).with_context(|| {
        format!(
            "reading {}",
            project_root.join(LOCAL_CONFIG_FILENAME).display()
        )
    })?;
    let openrouter = readiness::openrouter_readiness(local_config.as_ref(), &dotenv);

    let mut report = ReportBuilder::new(project_root.clone());
    report.section("Project spine/config");
    report.pass(format!("cybersin.yaml found at {}", project_root.display()));
    require_path(
        &mut report,
        &project_root,
        "cybersin.lock",
        "Run `cybersin init .` to scaffold the missing lockfile, or restore cybersin.lock.",
    );
    require_path(
        &mut report,
        &project_root,
        "prompts",
        "Run `cybersin init .` or create a prompts/ directory for prompt sources.",
    );
    require_path(
        &mut report,
        &project_root,
        "agents",
        "Run `cybersin init .` or create an agents/ directory for runtime harness specs.",
    );
    if project_root.join(LOCAL_CONFIG_FILENAME).is_file() {
        report.pass(format!("{LOCAL_CONFIG_FILENAME} present"));
    } else {
        report.warn(format!("{LOCAL_CONFIG_FILENAME} not present"));
        report.next(format!(
            "Copy cybersin.local.example.yaml to {LOCAL_CONFIG_FILENAME} when this machine needs provider, tool, sandbox, or routing overrides."
        ));
    }
    if dotenv.present {
        report.pass(format!(".env present at {}", dotenv.path.display()));
    } else {
        report.warn(".env not present");
        report.next("Create .env with OPENROUTER_API_KEY=... when you do not export provider keys in your shell.");
    }

    report.section("OpenRouter provider");
    match openrouter.availability {
        Availability::Available => report.pass("availability: available"),
        Availability::Auto => report.warn("availability: auto"),
        Availability::Unavailable => {
            report.fail("availability: unavailable");
            report.next(format!(
                "Set providers.{}.availability to available/auto in {LOCAL_CONFIG_FILENAME}, or remove the unavailable override.",
                openrouter.name
            ));
        }
    }
    if let Some(source) = &openrouter.api_key.source {
        report.pass(format!("api key: ready via {}", source.label()));
    } else if let Some(variable) = &openrouter.api_key.local_config_reference {
        report.fail(format!("api key: {variable} is referenced but not set"));
        report.next(format!(
            "Set {variable} in .env or export it before running live commands."
        ));
    } else {
        report.fail(format!(
            "api key: {} is not set",
            readiness::OPENROUTER_API_KEY_ENV
        ));
        report.next(format!(
            "Set {} in .env, export it, or reference it from providers.openrouter.api_key in {LOCAL_CONFIG_FILENAME}.",
            readiness::OPENROUTER_API_KEY_ENV
        ));
    }
    match &openrouter.base_url {
        Some(base_url) => report.pass(format!("base_url override: {base_url}")),
        None => report.pass("base_url: default OpenRouter endpoint"),
    }
    if openrouter.is_default_provider {
        report.pass("default provider: openrouter");
    } else {
        report.warn("default provider: not set to openrouter");
        report.next(format!(
            "Set defaults.provider: openrouter in {LOCAL_CONFIG_FILENAME} if OpenRouter should be the local default."
        ));
    }
    if let Some(model) = &openrouter.default_model {
        report.pass(format!("default model: {model}"));
    } else {
        report.warn("default model: not configured");
        report.next(format!(
            "Set defaults.model in {LOCAL_CONFIG_FILENAME} to the model this machine should prefer."
        ));
    }
    if openrouter.routing_provider_allowed {
        report.pass("routing permissions: openrouter provider allowed");
    } else {
        report.fail("routing permissions: openrouter provider is denied by local allowlist");
        report.next(format!(
            "Add openrouter to permissions.routing.allowed_providers in {LOCAL_CONFIG_FILENAME}, or remove the restrictive allowlist."
        ));
    }
    match (
        openrouter.default_model.as_ref(),
        openrouter.default_model_allowed,
    ) {
        (Some(model), Some(true)) => {
            report.pass(format!(
                "routing permissions: default model {model} allowed"
            ));
        }
        (Some(model), Some(false)) => {
            report.fail(format!(
                "routing permissions: default model {model} is denied by local allowlist"
            ));
            report.next(format!(
                "Add {model} under permissions.routing.allowed_models.openrouter in {LOCAL_CONFIG_FILENAME}, or choose an allowed defaults.model."
            ));
        }
        _ => {}
    }

    report.section("Tools");
    if let Some(config) = local_config.as_ref() {
        if config.tools.is_empty() {
            report.warn("no local tool readiness entries configured");
            report.next(format!(
                "Add tools.* entries to {LOCAL_CONFIG_FILENAME} for built-in tools that need API keys."
            ));
        } else {
            for (name, tool) in &config.tools {
                let status = match tool.availability {
                    Availability::Available => "available",
                    Availability::Auto => "auto",
                    Availability::Unavailable => "unavailable",
                };
                let key = tool
                    .api_key
                    .as_ref()
                    .map(|reference| {
                        if reference.read().is_some() {
                            format!("key {} set", reference.variable())
                        } else {
                            report.next(format!(
                                "Set {} in .env or export it for tool {name}.",
                                reference.variable()
                            ));
                            format!("key {} missing", reference.variable())
                        }
                    })
                    .unwrap_or_else(|| {
                        report.next(format!(
                            "Add tools.{name}.api_key in {LOCAL_CONFIG_FILENAME} if this tool requires a provider key."
                        ));
                        "no key reference".to_string()
                    });
                match tool.availability {
                    Availability::Unavailable => report.warn(format!("{name}: {status}, {key}")),
                    _ if key.contains("missing") || key == "no key reference" => {
                        report.warn(format!("{name}: {status}, {key}"))
                    }
                    _ => report.pass(format!("{name}: {status}, {key}")),
                }
            }
        }
        if !config.permissions.tools.allowed.is_empty() {
            report.pass(format!(
                "tool permissions allowed: {}",
                config.permissions.tools.allowed.join(", ")
            ));
        } else {
            report.warn("tool permissions: no explicit allowlist");
        }
        if !config.permissions.tools.denied.is_empty() {
            report.warn(format!(
                "tool permissions denied: {}",
                config.permissions.tools.denied.join(", ")
            ));
        }
    } else {
        report.warn("no cybersin.local.yaml; tool keys and permissions are env/default only");
    }

    report.section("Sandbox/container");
    report.pass(format!(
        "default sandbox root: {}",
        defaults.sandbox_root_default().display()
    ));
    report.pass(format!(
        "default sandbox backend: {:?}",
        defaults.sandbox_backend_default()
    ));
    if let Some(config) = local_config.as_ref() {
        if let Some(backend) = &config.sandbox.backend {
            report.pass(format!("local sandbox.backend: {backend}"));
        }
        if let Some(root) = &config.sandbox.root {
            report.pass(format!("local sandbox.root: {}", root.display()));
        }
        if let Some(scope) = &config.sandbox.scope {
            report.pass(format!("local sandbox.scope: {scope}"));
        }
        if let Some(runtime) = &config.sandbox.container_runtime {
            if runtime.read().is_some() {
                report.pass(format!("container runtime env {} set", runtime.variable()));
            } else {
                report.warn(format!(
                    "container runtime env {} referenced but not set",
                    runtime.variable()
                ));
                report.next(format!(
                    "Set {} or remove sandbox.container_runtime from {LOCAL_CONFIG_FILENAME}.",
                    runtime.variable()
                ));
            }
        } else {
            report.pass("container runtime: docker default or CYBERSIN_CONTAINER_RUNTIME");
        }
    }

    report.section("Build artifacts");
    if project_root.join("dist").is_dir() {
        report.pass("dist/ present");
    } else {
        report.warn("dist/ missing");
        report
            .next("Run `cybersin build . --profile dev --frozen` when you need runtime artifacts.");
    }

    Ok(report.finish())
}

fn require_path(report: &mut ReportBuilder, root: &Path, rel: &str, action: &str) {
    if root.join(rel).exists() {
        report.pass(format!("{rel} present"));
    } else {
        report.fail(format!("{rel} missing"));
        report.next(action);
    }
}

struct ReportBuilder {
    root: PathBuf,
    lines: Vec<String>,
    next_actions: Vec<String>,
    ok: bool,
}

impl ReportBuilder {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            lines: Vec::new(),
            next_actions: Vec::new(),
            ok: true,
        }
    }

    fn section(&mut self, title: &str) {
        if self.lines.is_empty() {
            self.lines.push("Cybersin doctor".to_string());
            self.lines.push(format!("project: {}", self.root.display()));
        }
        self.lines.push("".to_string());
        self.lines.push(format!("{title}:"));
    }

    fn pass(&mut self, text: impl Into<String>) {
        self.lines.push(format!("  [ok] {}", text.into()));
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.lines.push(format!("  [warn] {}", text.into()));
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.ok = false;
        self.lines.push(format!("  [fail] {}", text.into()));
    }

    fn next(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !self.next_actions.contains(&text) {
            self.next_actions.push(text);
        }
    }

    fn finish(mut self) -> DoctorReport {
        self.lines.push("".to_string());
        self.lines.push("Next actions:".to_string());
        if self.next_actions.is_empty() {
            self.lines.push("  - None. Setup looks ready.".to_string());
        } else {
            for action in &self.next_actions {
                self.lines.push(format!("  - {action}"));
            }
        }
        DoctorReport {
            ok: self.ok,
            lines: self.lines,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_spine(root: &Path) {
        std::fs::write(root.join("cybersin.yaml"), "name: test\n").unwrap();
        std::fs::write(root.join("cybersin.lock"), "models: {}\n").unwrap();
        std::fs::create_dir(root.join("prompts")).unwrap();
        std::fs::create_dir(root.join("agents")).unwrap();
    }

    #[test]
    fn missing_project_spine_is_a_failure_with_next_action() {
        let dir = tempfile::tempdir().unwrap();
        let report = diagnose(dir.path()).unwrap();

        assert!(!report.ok);
        let rendered = report.render();
        assert!(rendered.contains("no cybersin.yaml found"));
        assert!(rendered.contains("cybersin init ."));
    }

    #[test]
    fn dotenv_and_local_config_make_openrouter_ready_without_dist() {
        let dir = tempfile::tempdir().unwrap();
        write_spine(dir.path());
        std::fs::write(dir.path().join(".env"), "CYBERSIN_TEST_OR_KEY=test-key\n").unwrap();
        std::fs::write(
            dir.path().join(LOCAL_CONFIG_FILENAME),
            "providers:\n  openrouter:\n    availability: available\n    api_key: ${CYBERSIN_TEST_OR_KEY}\ndefaults:\n  provider: openrouter\n  model: openai/gpt-4o-mini\npermissions:\n  routing:\n    allowed_providers: [openrouter]\n",
        )
        .unwrap();
        std::env::remove_var("CYBERSIN_TEST_OR_KEY");

        let report = diagnose(dir.path()).unwrap();
        let rendered = report.render();

        assert!(report.ok, "{rendered}");
        assert!(rendered
            .contains("api key: ready via cybersin.local.yaml -> .env:CYBERSIN_TEST_OR_KEY"));
        assert!(rendered.contains("[warn] dist/ missing"));
        assert!(rendered.contains("cybersin build . --profile dev --frozen"));
        std::env::remove_var("CYBERSIN_TEST_OR_KEY");
    }

    #[test]
    fn routing_denial_is_distinct_from_key_availability() {
        let dir = tempfile::tempdir().unwrap();
        write_spine(dir.path());
        std::fs::write(dir.path().join(".env"), "OPENROUTER_API_KEY=test-key\n").unwrap();
        std::fs::write(
            dir.path().join(LOCAL_CONFIG_FILENAME),
            "permissions:\n  routing:\n    allowed_providers: [other]\n",
        )
        .unwrap();
        std::env::remove_var(readiness::OPENROUTER_API_KEY_ENV);

        let report = diagnose(dir.path()).unwrap();
        let rendered = report.render();

        assert!(!report.ok);
        assert!(rendered.contains("api key: ready via .env:OPENROUTER_API_KEY"));
        assert!(rendered.contains("routing permissions: openrouter provider is denied"));
        std::env::remove_var(readiness::OPENROUTER_API_KEY_ENV);
    }
}
