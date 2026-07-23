//! `cybersin fmt <path>` (spec §11): canonicalizes a `*.prompt.yaml`
//! source's key order and indentation via `cybersin-frontend`.

use std::fs;
use std::path::Path;

pub fn run(path: &Path, check_only: bool) -> Result<Option<String>, String> {
    let formatted =
        cybersin_frontend::format_prompt_source(path).map_err(|e| format!("error: {e}"))?;

    if check_only {
        let original = fs::read_to_string(path)
            .map_err(|e| format!("error: failed to read {}: {e}", path.display()))?;
        return if original == formatted {
            Ok(Some(format!("{} is already formatted", path.display())))
        } else {
            Err(format!(
                "{} is not formatted; run `cybersin fmt {}` to fix",
                path.display(),
                path.display()
            ))
        };
    }

    fs::write(path, &formatted)
        .map_err(|e| format!("error: failed to write {}: {e}", path.display()))?;
    Ok(Some(format!("formatted {}", path.display())))
}
