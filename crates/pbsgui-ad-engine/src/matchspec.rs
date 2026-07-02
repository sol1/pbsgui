//! Pure helpers for resolving VSS writer file descriptors: `%VAR%` expansion and
//! Windows wildcard filespec matching. Kept platform-neutral so they are unit
//! tested on every target, while their only caller (`crate::vss`) is
//! Windows-only; off Windows they are exercised by the tests alone.
#![cfg_attr(not(windows), allow(dead_code))]

/// Expand `%VAR%` references using `lookup` (case-insensitive on Windows, where
/// the environment itself is case-insensitive). An unknown variable is left as
/// written, which mirrors `ExpandEnvironmentStrings`.
pub fn expand_env(path: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match lookup(name) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                // A lone or trailing '%': keep it literally and stop scanning.
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Match a file NAME against a VSS filespec (`*` and `?` wildcards, no path
/// separators), case-insensitively as Windows filenames compare.
pub fn filespec_matches(spec: &str, name: &str) -> bool {
    let spec: Vec<char> = spec.to_lowercase().chars().collect();
    let name: Vec<char> = name.to_lowercase().chars().collect();
    glob_match(&spec, &name)
}

/// Classic backtracking wildcard match (`*` = any run, `?` = any one char).
fn glob_match(pat: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut star_t) = (None::<usize>, 0usize);
    while t < text.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star {
            // Backtrack: let the last '*' swallow one more character.
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name.to_ascii_lowercase().as_str() {
            "systemroot" => Some("C:\\Windows".to_string()),
            "programdata" => Some("C:\\ProgramData".to_string()),
            _ => None,
        }
    }

    #[test]
    fn expands_known_vars_and_keeps_unknown() {
        assert_eq!(expand_env("%SystemRoot%\\NTDS", env), "C:\\Windows\\NTDS");
        assert_eq!(expand_env("%NoSuchVar%\\x", env), "%NoSuchVar%\\x");
        assert_eq!(expand_env("plain\\path", env), "plain\\path");
        // A trailing lone percent stays literal.
        assert_eq!(expand_env("50%", env), "50%");
    }

    #[test]
    fn filespec_wildcards_match_windows_style() {
        assert!(filespec_matches("*", "ntds.dit"));
        assert!(filespec_matches("ntds.dit", "NTDS.DIT")); // case-insensitive
        assert!(filespec_matches("edb*.log", "edb00042.log"));
        assert!(filespec_matches("edb*.log", "edb.log"));
        assert!(!filespec_matches("edb*.log", "edb.jrs"));
        assert!(filespec_matches("?tds.dit", "ntds.dit"));
        assert!(!filespec_matches("?tds.dit", "nntds.dit"));
        assert!(filespec_matches("*.*", "a.b"));
    }
}
