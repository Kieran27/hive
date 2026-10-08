//! `{placeholder}` substitution for run/setup commands, plus shell quoting.
//!
//! Only `{ident}` with ident in `[A-Za-z0-9_.-]` is a placeholder, and `${…}`
//! is left alone, so ordinary shell syntax passes through untouched.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

pub type Vars = BTreeMap<String, String>;

pub fn render(template: &str, vars: &Vars) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut unknown = Vec::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'{' && (i == 0 || bytes[i - 1] != b'$') {
            if let Some(len) = template[i + 1..].find('}') {
                let key = &template[i + 1..i + 1 + len];
                if !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
                {
                    match vars.get(key) {
                        Some(v) => out.push_str(v),
                        None => unknown.push(key.to_string()),
                    }
                    i += len + 2;
                    continue;
                }
            }
        }
        // Push the whole UTF-8 char starting here.
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    if !unknown.is_empty() {
        bail!(
            "unknown placeholder(s): {}",
            unknown
                .iter()
                .map(|k| format!("{{{k}}}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(out)
}

/// Placeholders a template references, in first-seen order.
pub fn placeholders(template: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let bytes = template.as_bytes();
    for (i, _) in template.match_indices('{') {
        if i > 0 && bytes[i - 1] == b'$' {
            continue;
        }
        if let Some(len) = template[i + 1..].find('}') {
            let key = &template[i + 1..i + 1 + len];
            if !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
                && !found.iter().any(|k| k == key)
            {
                found.push(key.to_string());
            }
        }
    }
    found
}

/// POSIX single-quote a string for `sh -c`.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@%+".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn shell_join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|a| shell_quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(kv: &[(&str, &str)]) -> Vars {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn renders_ports_and_keeps_shell_syntax() {
        let v = vars(&[("port", "3010"), ("port.api", "4010")]);
        assert_eq!(
            render(
                "PORT={port} URL=http://localhost:{port.api} ${HOME} {a,b} x",
                &v
            )
            .unwrap(),
            "PORT=3010 URL=http://localhost:4010 ${HOME} {a,b} x"
        );
    }

    #[test]
    fn unknown_is_error() {
        let e = render("yarn env:{env} {device}", &Vars::new())
            .unwrap_err()
            .to_string();
        assert!(e.contains("{env}") && e.contains("{device}"));
        assert_eq!(
            placeholders("a {env} {device} {env}"),
            vec!["env", "device"]
        );
    }

    #[test]
    fn unicode_passthrough() {
        assert_eq!(
            render("échο {x} ✓", &vars(&[("x", "1")])).unwrap(),
            "échο 1 ✓"
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_join(&["notify=[\"x\"]", "-c"]), "'notify=[\"x\"]' -c");
    }
}
