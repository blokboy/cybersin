//! Template handling for section bodies (spec §5.1, §6.1, §13).
//!
//! # Judgment call: `{{#each}}` sugar vs. native minijinja
//!
//! The spec's chosen template engine is **minijinja**, a Jinja2-compatible
//! engine that natively loops with `{% for x in list %}...{% endfor %}` —
//! not with the handlebars-style `{{#each documents}}...{{/each}}` shown
//! in the §5.1 example. Rather than treat that example as merely
//! illustrative and reject it, this frontend treats it as **sugar**:
//! `{{#each x}}...{{/each}}` sections (with `{{this}}` / `{{.}}` inside
//! referring to the current item) are recognized and mechanically
//! translated into native minijinja `{% for item in x %}...{% endfor %}`
//! (with `{{this}}`/`{{.}}` rewritten to `{{ item }}`) at compile time.
//!
//! This keeps the spec's example working verbatim while giving the IR a
//! single, uniform template dialect: [`cybersin_ir::Section::body`] always
//! holds plain minijinja source once the frontend is done with it, so the
//! runtime's context assembler (§8.3a) only ever needs to know about one
//! template engine, never about handlebars sugar.
//!
//! Native minijinja syntax (`{% for x in y %}`) is left untouched and
//! works exactly as minijinja documents it; both spellings can be mixed
//! across sections in the same prompt source.

use regex::Regex;

/// How a root input was referenced in a section body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefKind {
    /// `{{ name }}` — a plain interpolation.
    Plain,
    /// The collection driven by a loop: `{{#each name}}` or
    /// `{% for x in name %}`. Must be a `list[...]` input.
    Collection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VarRef {
    pub name: String,
    pub kind: RefKind,
}

/// Extract every root-input reference from a raw (untranslated) section
/// body, recognizing both the handlebars-sugar and native minijinja loop
/// spellings.
///
/// Loop bodies are stripped out (not scanned further) once their
/// collection reference is recorded: the loop-bound item (`this`, `.`, or
/// a native `for`'s bound name) is a local, not a root input, and
/// `cybersin-ir`'s `document` input type has no further substructure to
/// typecheck member access against.
pub(crate) fn extract_refs(body: &str) -> Vec<VarRef> {
    let mut refs = Vec::new();

    let each_re =
        Regex::new(r"(?s)\{\{\s*#each\s+([A-Za-z_][A-Za-z0-9_]*)\s*\}\}.*?\{\{\s*/each\s*\}\}")
            .unwrap();
    let after_each = each_re.replace_all(body, |caps: &regex::Captures| {
        refs.push(VarRef {
            name: caps[1].to_string(),
            kind: RefKind::Collection,
        });
        String::new()
    });

    let for_re = Regex::new(
        r"(?s)\{%-?\s*for\s+[A-Za-z_][A-Za-z0-9_]*\s+in\s+([A-Za-z_][A-Za-z0-9_]*)\s*-?%\}.*?\{%-?\s*endfor\s*-?%\}",
    )
    .unwrap();
    let after_for = for_re.replace_all(&after_each, |caps: &regex::Captures| {
        refs.push(VarRef {
            name: caps[1].to_string(),
            kind: RefKind::Collection,
        });
        String::new()
    });

    let plain_re = Regex::new(r"\{\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*\}\}").unwrap();
    for caps in plain_re.captures_iter(&after_for) {
        refs.push(VarRef {
            name: caps[1].to_string(),
            kind: RefKind::Plain,
        });
    }

    refs
}

/// Translate any `{{#each x}}...{{/each}}` blocks in `body` into native
/// minijinja `{% for item in x %}...{% endfor %}`, rewriting `{{this}}`,
/// `{{this.field}}`, and `{{.}}` inside the block to refer to the bound
/// `item`. Bodies with no handlebars sugar pass through unchanged.
pub(crate) fn translate_handlebars_each(body: &str) -> String {
    let each_re =
        Regex::new(r"(?s)\{\{\s*#each\s+([A-Za-z_][A-Za-z0-9_]*)\s*\}\}(.*?)\{\{\s*/each\s*\}\}")
            .unwrap();
    let this_field_re = Regex::new(r"\{\{\s*this\.").unwrap();
    let this_re = Regex::new(r"\{\{\s*this\s*\}\}").unwrap();
    let dot_re = Regex::new(r"\{\{\s*\.\s*\}\}").unwrap();

    each_re
        .replace_all(body, |caps: &regex::Captures| {
            let collection = &caps[1];
            let inner = &caps[2];
            let inner = this_field_re.replace_all(inner, "{{ item.");
            let inner = this_re.replace_all(&inner, "{{ item }}");
            let inner = dot_re.replace_all(&inner, "{{ item }}");
            format!("{{% for item in {collection} %}}{inner}{{% endfor %}}")
        })
        .into_owned()
}

/// Parse `body` as minijinja source without rendering it, to catch
/// malformed templates at compile time even though the concrete input
/// values only exist at runtime.
pub(crate) fn validate_syntax(body: &str) -> Result<(), minijinja::Error> {
    let env = minijinja::Environment::new();
    env.template_from_str(body).map(|_| ())
}

/// Render a section body (already `!include`-resolved; handlebars sugar
/// translated or native minijinja) against concrete input values. Exposed
/// for callers (tests, `cybersin explain` in a later issue) that want to
/// preview a rendered prompt; not invoked during `cybersin check` itself,
/// since compile time has no concrete input values to render with.
pub fn render(body: &str, values: &serde_json::Value) -> Result<String, minijinja::Error> {
    let translated = translate_handlebars_each(body);
    let env = minijinja::Environment::new();
    let tmpl = env.template_from_str(&translated)?;
    tmpl.render(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_var() {
        let refs = extract_refs("Topic: {{ topic }}");
        assert_eq!(
            refs,
            vec![VarRef {
                name: "topic".to_string(),
                kind: RefKind::Plain
            }]
        );
    }

    #[test]
    fn extracts_handlebars_each_collection_only() {
        let refs = extract_refs("{{#each documents}}{{this.title}}{{/each}}");
        assert_eq!(
            refs,
            vec![VarRef {
                name: "documents".to_string(),
                kind: RefKind::Collection
            }]
        );
    }

    #[test]
    fn extracts_native_for_collection_only() {
        let refs = extract_refs("{% for doc in documents %}{{ doc.title }}{% endfor %}");
        assert_eq!(
            refs,
            vec![VarRef {
                name: "documents".to_string(),
                kind: RefKind::Collection
            }]
        );
    }

    #[test]
    fn translates_each_sugar_to_minijinja_and_renders() {
        let translated =
            translate_handlebars_each("{{#each documents}}- {{this.title}}\n{{/each}}");
        assert_eq!(
            translated,
            "{% for item in documents %}- {{ item.title}}\n{% endfor %}"
        );

        let values = serde_json::json!({
            "documents": [{"title": "Doc A"}, {"title": "Doc B"}],
        });
        let rendered = render("{{#each documents}}- {{this.title}}\n{{/each}}", &values).unwrap();
        assert_eq!(rendered, "- Doc A\n- Doc B\n");
    }

    #[test]
    fn renders_plain_interpolation() {
        let values = serde_json::json!({"topic": "quantum computing"});
        let rendered = render("Topic: {{ topic }}", &values).unwrap();
        assert_eq!(rendered, "Topic: quantum computing");
    }

    #[test]
    fn renders_dot_shorthand_inside_each() {
        let values = serde_json::json!({"tags": ["a", "b", "c"]});
        let rendered = render("{{#each tags}}{{.}},{{/each}}", &values).unwrap();
        assert_eq!(rendered, "a,b,c,");
    }

    #[test]
    fn invalid_minijinja_syntax_fails_validation() {
        let err = validate_syntax("{% for x in y %} unterminated");
        assert!(err.is_err());
    }
}
