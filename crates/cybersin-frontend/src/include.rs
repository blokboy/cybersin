//! Resolves the `!include` graph (spec §5.1, §6.1).
//!
//! `!include path/to/file` is a YAML custom tag. Wherever it appears in a
//! parsed source document, this module reads the referenced file (relative
//! to the including file's directory) and substitutes its contents as a
//! plain string, in place.
//!
//! Fragments can themselves chain to further fragments: this is what makes
//! `!include` a *graph* rather than a flat one-level substitution, and
//! what a cyclic include needs in order to exist at all. Since included
//! fragments are typically prose/markdown/JSON — not YAML documents in
//! their own right, and parsing arbitrary prose as YAML would be fragile —
//! nested includes are recognized textually: a fragment line whose
//! trimmed content is exactly `!include <path>` is itself resolved
//! (recursively, relative to *that* fragment's own directory) and spliced
//! in. Any file, directly or transitively, including itself is rejected
//! with the full include chain in the error.
use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use crate::error::FrontendError;

/// Walk `value` (the parsed but otherwise unprocessed YAML document for a
/// `*.prompt.yaml` source) and resolve every `!include` tag found,
/// relative to `base_dir` (the source file's own directory).
pub(crate) fn resolve_source_includes(
    value: Value,
    base_dir: &Path,
) -> Result<Value, FrontendError> {
    let mut stack = Vec::new();
    resolve_value(value, base_dir, &mut stack)
}

fn resolve_value(
    value: Value,
    base_dir: &Path,
    stack: &mut Vec<PathBuf>,
) -> Result<Value, FrontendError> {
    match value {
        Value::Tagged(tagged) => {
            let tag_name = tagged.tag.to_string();
            // serde_yaml's `Tag::to_string` includes the leading `!`.
            if tag_name != "!include" {
                return Err(FrontendError::UnsupportedTag {
                    tag: tag_name.trim_start_matches('!').to_string(),
                });
            }
            let path = match tagged.value {
                Value::String(s) => s,
                other => {
                    return Err(FrontendError::InvalidIncludeTarget {
                        found: describe(&other),
                    })
                }
            };
            let resolved_text = include_file(base_dir, &path, stack)?;
            Ok(Value::String(resolved_text))
        }
        Value::Mapping(map) => {
            let mut resolved = serde_yaml::Mapping::new();
            for (k, v) in map {
                resolved.insert(k, resolve_value(v, base_dir, stack)?);
            }
            Ok(Value::Mapping(resolved))
        }
        Value::Sequence(seq) => {
            let resolved = seq
                .into_iter()
                .map(|v| resolve_value(v, base_dir, stack))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Sequence(resolved))
        }
        other => Ok(other),
    }
}

fn describe(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("{s:?}"),
        Value::Sequence(_) => "a sequence".to_string(),
        Value::Mapping(_) => "a mapping".to_string(),
        Value::Tagged(t) => format!("a value tagged `{}`", t.tag),
    }
}

/// Read `rel_path` (relative to `base_dir`), recursively resolving any
/// nested `!include` lines within it, with cycle detection via `stack`.
fn include_file(
    base_dir: &Path,
    rel_path: &str,
    stack: &mut Vec<PathBuf>,
) -> Result<String, FrontendError> {
    let candidate = base_dir.join(rel_path);
    let canonical = candidate
        .canonicalize()
        .map_err(|source| FrontendError::Io {
            path: candidate.clone(),
            source,
        })?;

    if let Some(pos) = stack.iter().position(|p| p == &canonical) {
        let mut chain: Vec<PathBuf> = stack[pos..].to_vec();
        chain.push(canonical);
        return Err(FrontendError::IncludeCycle { chain });
    }

    let content = fs::read_to_string(&canonical).map_err(|source| FrontendError::Io {
        path: canonical.clone(),
        source,
    })?;

    stack.push(canonical.clone());
    let new_base = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let resolved = resolve_nested_includes(&content, &new_base, stack);
    stack.pop();
    resolved
}

/// Scan `content` line-by-line for nested `!include <path>` directives
/// (a line whose trimmed content is exactly that) and splice in their
/// resolved contents. Ordinary prose lines pass through unchanged.
fn resolve_nested_includes(
    content: &str,
    base_dir: &Path,
    stack: &mut Vec<PathBuf>,
) -> Result<String, FrontendError> {
    let mut out = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("!include ") {
            let nested = include_file(base_dir, rest.trim(), stack)?;
            out.push_str(&nested);
            if !nested.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn resolves_a_simple_include() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "fragments/method.md",
            "Do the research thing.\n",
        );

        let value = Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("include"),
            value: Value::String("fragments/method.md".to_string()),
        }));

        let resolved = resolve_source_includes(value, tmp.path()).unwrap();
        assert_eq!(
            resolved,
            Value::String("Do the research thing.\n".to_string())
        );
    }

    #[test]
    fn resolves_nested_includes() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "fragments/outer.md",
            "intro\n!include inner.md\noutro\n",
        );
        write(tmp.path(), "fragments/inner.md", "the inner content\n");

        let value = Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("include"),
            value: Value::String("fragments/outer.md".to_string()),
        }));

        let resolved = resolve_source_includes(value, tmp.path()).unwrap();
        assert_eq!(
            resolved,
            Value::String("intro\nthe inner content\noutro\n".to_string())
        );
    }

    #[test]
    fn detects_a_two_file_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "fragments/a.md", "!include b.md\n");
        write(tmp.path(), "fragments/b.md", "!include a.md\n");

        let value = Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("include"),
            value: Value::String("fragments/a.md".to_string()),
        }));

        let err = resolve_source_includes(value, tmp.path()).unwrap_err();
        match err {
            FrontendError::IncludeCycle { chain } => {
                assert!(chain.len() >= 2, "expected a chain, got {chain:?}");
            }
            other => panic!("expected IncludeCycle, got {other:?}"),
        }
    }

    #[test]
    fn detects_self_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "fragments/a.md", "!include a.md\n");

        let value = Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("include"),
            value: Value::String("fragments/a.md".to_string()),
        }));

        let err = resolve_source_includes(value, tmp.path()).unwrap_err();
        assert!(matches!(err, FrontendError::IncludeCycle { .. }));
    }

    #[test]
    fn rejects_unsupported_tags() {
        let value = Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("something_else"),
            value: Value::String("x".to_string()),
        }));
        let err = resolve_source_includes(value, Path::new(".")).unwrap_err();
        assert!(matches!(err, FrontendError::UnsupportedTag { .. }));
    }
}
