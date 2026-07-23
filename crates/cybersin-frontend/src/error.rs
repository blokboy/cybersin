//! Frontend error types (spec §6.1).
//!
//! Every failure mode a source can hit during `cybersin check` — bad YAML,
//! an unresolvable or cyclic `!include`, an invalid type declaration, or a
//! typecheck problem (undeclared input, type mismatch, unused input) —
//! comes back as a variant here with a human-readable [`std::fmt::Display`]
//! message, so the CLI can print it directly and exit nonzero.

use std::path::PathBuf;

use thiserror::Error;

/// Everything that can go wrong compiling a `*.prompt.yaml` source.
#[derive(Debug, Error)]
pub enum FrontendError {
    /// Reading the source file, or a file it `!include`s, failed.
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The source (or a resolved include) is not well-formed YAML.
    #[error("failed to parse YAML in {path}: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    /// A YAML tag other than `!include` was used. This frontend only
    /// understands one custom tag (spec §5.1, §6.1).
    #[error("unsupported YAML tag `!{tag}`; only `!include` is understood")]
    UnsupportedTag { tag: String },

    /// `!include` was applied to something other than a plain string path.
    #[error("`!include` requires a plain string path, found: {found}")]
    InvalidIncludeTarget { found: String },

    /// Following `!include` directives (directly or transitively, since a
    /// fragment can itself start with `!include another-fragment`) led
    /// back to a file already being resolved.
    #[error(
        "include cycle detected: {}",
        chain
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" -> ")
    )]
    IncludeCycle { chain: Vec<PathBuf> },

    /// `quality:` was not one of `low`, `medium`, `high`.
    #[error("invalid quality tier `{raw}` (expected one of: low, medium, high)")]
    InvalidQuality { raw: String },

    /// A section body (after `!include` resolution and handlebars-sugar
    /// translation, spec §6.1/§13) is not valid minijinja syntax.
    #[error("template error in section `{section}`: {source}")]
    Template {
        section: String,
        #[source]
        source: minijinja::Error,
    },

    /// One or more typecheck problems (spec §6.1: "typecheck inputs
    /// against template usage"). Collected together so a single `cybersin
    /// check` run reports every problem at once rather than one-at-a-time.
    #[error(
        "typecheck failed:\n{}",
        .0.iter().map(|i| format!("  - {i}")).collect::<Vec<_>>().join("\n")
    )]
    Typecheck(Vec<TypecheckIssue>),
}

/// A single typecheck problem found while validating a prompt source
/// against its declared `inputs` map (spec §5.1, §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TypecheckIssue {
    /// `inputs:` declared a type string this frontend doesn't recognize
    /// (spec §5.1's grammar: `string`, `number`, `bool`, `document`,
    /// `enum[a, b]`, `list[<type>]`).
    #[error("input `{name}` has an invalid type declaration `{raw}`")]
    InvalidInputType { name: String, raw: String },

    /// A section body referenced `{{ name }}` (or looped over `name` via
    /// `{{#each name}}` / `{% for x in name %}`) but `name` is not in the
    /// declared `inputs` map.
    #[error("section `{location}` references undeclared input `{name}`")]
    UndeclaredInput { location: String, name: String },

    /// A section used an input in a way incompatible with its declared
    /// type — looping over a non-list, or printing a list directly instead
    /// of iterating it.
    #[error("section `{location}` uses `{name}` as {expected}, but it is declared as {found}")]
    TypeMismatch {
        location: String,
        name: String,
        expected: String,
        found: String,
    },

    /// An input was declared in `inputs:` but never referenced by any
    /// section body — dead input the author should remove or use.
    #[error("input `{name}` is declared but never referenced in any section body")]
    UnusedInput { name: String },
}
