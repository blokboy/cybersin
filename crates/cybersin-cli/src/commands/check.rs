//! `cybersin check <path>` (spec §11): runs one prompt source, or every
//! `*.prompt.yaml` under a project, through `cybersin-frontend`'s
//! parse/resolve/typecheck/emit pipeline.

use std::path::Path;

pub fn run(path: &Path) -> Result<Option<String>, String> {
    let sources = cybersin_frontend::discover_prompt_sources(path)
        .map_err(|e| format!("error: could not read {}: {e}", path.display()))?;

    if sources.is_empty() {
        return Err(format!(
            "error: no *.prompt.yaml sources found at {}",
            path.display()
        ));
    }

    let mut failed = Vec::new();
    for source in &sources {
        match cybersin_frontend::compile_prompt_source(source) {
            Ok(_ir) => println!("ok    {}", source.display()),
            Err(e) => {
                eprintln!("FAIL  {}\n{e}\n", source.display());
                failed.push(source.clone());
            }
        }
    }

    if failed.is_empty() {
        Ok(Some(format!(
            "cybersin check: {} source(s) ok",
            sources.len()
        )))
    } else {
        Err(format!(
            "cybersin check failed: {} of {} source(s) had errors",
            failed.len(),
            sources.len()
        ))
    }
}
