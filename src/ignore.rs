//! Minimal `.gitignore`-subset matcher (spec §17). Supports comments, blank
//! lines, plain names, directory-only (`trailing /`) patterns, `*`/`?`
//! glob within one path component, and leading `!` negation. No `**`, no
//! character classes, no escapes, no nested ignore files — those are
//! plugin-owned per spec.

#[allow(dead_code)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

struct Pattern {
    negate: bool,
    dir_only: bool,
    anchored: bool,
    // path components of the pattern; len 1 for non-anchored patterns
    // (they match against any single component of the candidate path).
    parts: Vec<String>,
}

#[allow(dead_code)]
impl IgnoreRules {
    pub fn parse(text: &str) -> Self {
        let patterns = text
            .lines()
            .filter_map(|line| {
                let line = line.trim_end_matches(['\r', '\n']);
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    return None;
                }
                let (negate, rest) = match trimmed.strip_prefix('!') {
                    Some(r) => (true, r),
                    None => (false, trimmed),
                };
                let (dir_only, rest) = match rest.strip_suffix('/') {
                    Some(r) => (true, r),
                    None => (false, rest),
                };
                if rest.is_empty() {
                    return None;
                }
                let anchored = rest.contains('/');
                // a leading '/' is just an explicit anchor; drop it so
                // splitting doesn't produce a leading empty component
                let rest = rest.strip_prefix('/').unwrap_or(rest);
                let parts = if anchored {
                    rest.split('/').map(String::from).collect()
                } else {
                    vec![rest.to_string()]
                };
                Some(Pattern {
                    negate,
                    dir_only,
                    anchored,
                    parts,
                })
            })
            .collect();
        IgnoreRules { patterns }
    }

    /// rel_path: path relative to root, '/'-separated, no leading '/'
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        let components: Vec<&str> = rel_path.split('/').collect();
        let mut ignored = false;
        for pat in &self.patterns {
            if pat.matches(&components, is_dir) {
                ignored = !pat.negate;
            }
        }
        ignored
    }
}

impl Pattern {
    fn matches(&self, components: &[&str], is_dir: bool) -> bool {
        let m = components.len();
        if self.anchored {
            let n = self.parts.len();
            if n > m {
                return false;
            }
            if !(0..n).all(|i| glob_match(&self.parts[i], components[i])) {
                return false;
            }
            // exact-length match is a leaf: dir_only applies. A shorter
            // match means we matched an ancestor directory, which always
            // satisfies dir_only regardless of the leaf's own kind.
            n < m || !self.dir_only || is_dir
        } else {
            let pat = &self.parts[0];
            components.iter().enumerate().any(|(i, comp)| {
                if !glob_match(pat, comp) {
                    return false;
                }
                i < m - 1 || !self.dir_only || is_dir
            })
        }
    }
}

/// `*`/`?` glob match within a single path component (no `/` on either side).
fn glob_match(pat: &str, text: &str) -> bool {
    let pat = pat.as_bytes();
    let text = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;
    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blanks_ignored() {
        let r = IgnoreRules::parse("# comment\n\n   \nfoo\n");
        assert!(r.is_ignored("foo", false));
        assert!(!r.is_ignored("bar", false));
    }

    #[test]
    fn simple_name_matches_any_component() {
        let r = IgnoreRules::parse("target");
        assert!(r.is_ignored("target", true));
        assert!(r.is_ignored("target", false));
        assert!(r.is_ignored("a/target/b.rs", false));
        assert!(r.is_ignored("a/b/target", true));
        assert!(!r.is_ignored("targets", false));
    }

    #[test]
    fn dir_only_pattern() {
        let r = IgnoreRules::parse("build/");
        assert!(r.is_ignored("build", true));
        assert!(!r.is_ignored("build", false));
        // matching an ancestor component is always fine, even for a
        // dir-only pattern, since the ancestor is necessarily a directory
        assert!(r.is_ignored("build/out.txt", false));
    }

    #[test]
    fn star_within_component() {
        let r = IgnoreRules::parse("*.rs");
        assert!(r.is_ignored("main.rs", false));
        assert!(r.is_ignored("src/main.rs", false));
        assert!(!r.is_ignored("src/main.rs.bak", false));
    }

    #[test]
    fn star_does_not_cross_path_separator() {
        let r = IgnoreRules::parse("a*b");
        assert!(r.is_ignored("aXXXb", false));
        assert!(!r.is_ignored("a/b", false));
    }

    #[test]
    fn question_mark_single_char() {
        let r = IgnoreRules::parse("fil?.txt");
        assert!(r.is_ignored("file.txt", false));
        assert!(!r.is_ignored("fil.txt", false));
        assert!(!r.is_ignored("filee.txt", false));
    }

    #[test]
    fn negation_last_match_wins() {
        let r = IgnoreRules::parse("*.log\n!important.log\n");
        assert!(r.is_ignored("debug.log", false));
        assert!(!r.is_ignored("important.log", false));
    }

    #[test]
    fn negation_can_be_overridden_by_later_pattern() {
        let r = IgnoreRules::parse("*.log\n!important.log\nimportant.log\n");
        assert!(r.is_ignored("important.log", false));
    }

    #[test]
    fn root_anchored_pattern_matches_only_at_root() {
        let r = IgnoreRules::parse("/config.rs");
        assert!(r.is_ignored("config.rs", false));
        assert!(!r.is_ignored("src/config.rs", false));
    }

    #[test]
    fn interior_slash_anchors_to_root_path() {
        let r = IgnoreRules::parse("src/gen");
        assert!(r.is_ignored("src/gen", true));
        assert!(!r.is_ignored("gen", true));
        assert!(!r.is_ignored("other/src/gen", true));
        // a nested path under the anchored dir is still ignored, since a
        // walker would treat "src/gen" as an unrecurse-able directory
        assert!(r.is_ignored("src/gen/child.rs", false));
    }

    #[test]
    fn anchored_dir_only_leaf_requires_dir() {
        let r = IgnoreRules::parse("src/gen/");
        assert!(r.is_ignored("src/gen", true));
        assert!(!r.is_ignored("src/gen", false));
    }

    #[test]
    fn component_anchored_vs_root_anchored() {
        // no slash: matches "gen" anywhere; with slash: root-relative only
        let unanchored = IgnoreRules::parse("gen");
        let anchored = IgnoreRules::parse("/gen");
        assert!(unanchored.is_ignored("a/gen", true));
        assert!(!anchored.is_ignored("a/gen", true));
        assert!(anchored.is_ignored("gen", true));
    }

    #[test]
    fn empty_and_whitespace_only_lines_are_noop() {
        let r = IgnoreRules::parse("!\n/\n");
        assert!(!r.is_ignored("anything", false));
    }
}
