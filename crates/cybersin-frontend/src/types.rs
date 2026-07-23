//! Parses the `inputs:` type grammar (spec §5.1: `string`, `number`,
//! `bool`, `document`, `enum[a, b]`, `list[<type>]`, arbitrarily nested)
//! into [`cybersin_ir::InputType`].

use cybersin_ir::InputType;

/// Parse one type declaration string, e.g. `"list[document]"`.
pub(crate) fn parse_input_type(raw: &str) -> Option<InputType> {
    let s = raw.trim();
    match s {
        "string" => Some(InputType::String),
        "number" => Some(InputType::Number),
        "bool" => Some(InputType::Bool),
        "document" => Some(InputType::Document),
        _ => {
            if let Some(inner) = strip_wrapper(s, "enum[") {
                let variants: Vec<String> = inner
                    .split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect();
                if variants.is_empty() {
                    None
                } else {
                    Some(InputType::Enum { variants })
                }
            } else if let Some(inner) = strip_wrapper(s, "list[") {
                parse_input_type(inner).map(|of| InputType::List { of: Box::new(of) })
            } else {
                None
            }
        }
    }
}

fn strip_wrapper<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.starts_with(prefix) && s.ends_with(']') {
        Some(&s[prefix.len()..s.len() - 1])
    } else {
        None
    }
}

/// Render an [`InputType`] back to its `inputs:` grammar spelling, used in
/// typecheck error messages (e.g. "declared as list[document]").
pub(crate) fn type_name(t: &InputType) -> String {
    match t {
        InputType::String => "string".to_string(),
        InputType::Number => "number".to_string(),
        InputType::Bool => "bool".to_string(),
        InputType::Document => "document".to_string(),
        InputType::Enum { variants } => format!("enum[{}]", variants.join(", ")),
        InputType::List { of } => format!("list[{}]", type_name(of)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(parse_input_type("string"), Some(InputType::String));
        assert_eq!(parse_input_type(" number "), Some(InputType::Number));
        assert_eq!(parse_input_type("bool"), Some(InputType::Bool));
        assert_eq!(parse_input_type("document"), Some(InputType::Document));
    }

    #[test]
    fn parses_enum() {
        assert_eq!(
            parse_input_type("enum[quick, thorough]"),
            Some(InputType::Enum {
                variants: vec!["quick".to_string(), "thorough".to_string()]
            })
        );
    }

    #[test]
    fn parses_list_of_document() {
        assert_eq!(
            parse_input_type("list[document]"),
            Some(InputType::List {
                of: Box::new(InputType::Document)
            })
        );
    }

    #[test]
    fn parses_nested_list() {
        assert_eq!(
            parse_input_type("list[list[string]]"),
            Some(InputType::List {
                of: Box::new(InputType::List {
                    of: Box::new(InputType::String)
                })
            })
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_input_type("frobnicate"), None);
        assert_eq!(parse_input_type("list[]"), None);
        assert_eq!(parse_input_type("enum[]"), None);
    }

    #[test]
    fn type_name_round_trips_grammar() {
        let t = InputType::List {
            of: Box::new(InputType::Enum {
                variants: vec!["a".to_string(), "b".to_string()],
            }),
        };
        assert_eq!(type_name(&t), "list[enum[a, b]]");
    }
}
