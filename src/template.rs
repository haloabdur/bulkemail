//! Tiny `{{variable}}` substitution for subjects and bodies.

use serde_json::{Map, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    None,
    Html,
}

fn is_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replaces every `{{ key }}` with its value from `vars`. Unknown keys become empty;
/// text that is not a valid placeholder is kept as is.
pub fn render(input: &str, vars: &Map<String, Value>, escape: Escape) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) if is_key(after[..end].trim()) => {
                let key = after[..end].trim();
                let value = vars.get(key).map(value_to_string).unwrap_or_default();
                match escape {
                    Escape::Html => out.push_str(&html_escape::encode_text(&value)),
                    Escape::None => out.push_str(&value),
                }
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Placeholders used in a template, in order of first appearance.
pub fn placeholders(input: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = after[..end].trim();
                if is_key(key) && !found.iter().any(|k| k == key) {
                    found.push(key.to_string());
                }
                rest = &after[end + 2..];
            }
            None => break,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn substitutes_and_escapes() {
        let v = vars(json!({"nama": "Andi <b>", "jam": 9}));
        assert_eq!(
            render("Halo {{nama}}, jam {{ jam }}", &v, Escape::None),
            "Halo Andi <b>, jam 9"
        );
        assert_eq!(
            render("<p>{{nama}}</p>", &v, Escape::Html),
            "<p>Andi &lt;b&gt;</p>"
        );
    }

    #[test]
    fn unknown_keys_become_empty_and_junk_is_kept() {
        let v = vars(json!({}));
        assert_eq!(render("a{{x}}b", &v, Escape::None), "ab");
        assert_eq!(
            render("{{ not a key }} {{", &v, Escape::None),
            "{{ not a key }} {{"
        );
    }

    #[test]
    fn lists_placeholders() {
        assert_eq!(placeholders("{{a}} {{ b }} {{a}}"), vec!["a", "b"]);
    }
}
