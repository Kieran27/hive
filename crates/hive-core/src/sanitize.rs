//! Branch-name helpers, ported from portal-worktree-tui (`sanitize.ts`,
//! `profiles.ts`).

/// Turn a branch name into a folder-safe name.
pub fn sanitize_branch_name(branch: &str) -> String {
    let b = normalize_branch(branch);
    let mut out = String::with_capacity(b.len());
    let mut last_dash = false;
    for c in b.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            if c == '-' {
                if !last_dash {
                    out.push('-');
                }
                last_dash = true;
            } else {
                out.push(c);
                last_dash = false;
            }
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(90).collect()
}

/// Strip `refs/heads/` and `origin/` prefixes.
pub fn normalize_branch(branch: &str) -> &str {
    let b = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    b.strip_prefix("origin/").unwrap_or(b)
}

/// `*` matches any run of characters (including `/`); everything else is literal.
pub fn match_pattern(value: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return value == pattern;
    }
    let mut rest = value;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_like_portal_wt() {
        assert_eq!(
            sanitize_branch_name("feature/login-page"),
            "feature-login-page"
        );
        assert_eq!(sanitize_branch_name("origin/fix/odds"), "fix-odds");
        assert_eq!(sanitize_branch_name("refs/heads/a//b"), "a-b");
        assert_eq!(sanitize_branch_name("--x y--"), "x-y");
        assert_eq!(sanitize_branch_name(&"a".repeat(120)).len(), 90);
    }

    #[test]
    fn patterns() {
        assert!(match_pattern("feature/x", "feature/*"));
        assert!(match_pattern("native-foo", "native-*"));
        assert!(!match_pattern("feature/x", "fix/*"));
        assert!(match_pattern("master", "master"));
        assert!(!match_pattern("main-2", "main"));
        assert!(match_pattern("anything", "*"));
        assert!(match_pattern("a/b/c", "a/*/c"));
        assert!(!match_pattern("a/b/d", "a/*/c"));
    }
}
