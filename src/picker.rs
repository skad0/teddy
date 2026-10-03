//! Lazy tree file picker (spec §17): only expanded branches are read,
//! typing switches to a bounded cooperative expand-on-search walk, and a
//! minimal root-.gitignore subset plus global globs filter everything.

use crate::ignore::IgnoreRules;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

pub struct Entry {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
    /// Display text: file name in tree view, root-relative path in search.
    pub name: String,
}

pub struct Picker {
    pub root: PathBuf,
    pub filter: String,
    pub entries: Vec<Entry>,
    pub sel: usize,
    expanded: HashSet<PathBuf>,
    ignore: IgnoreRules,
    walk: Option<Walk>,
}

struct Walk {
    queue: VecDeque<(PathBuf, usize)>,
    visited: usize,
}

/// ponytail: expand-on-search visits at most this many directories; a
/// full recursive index is explicitly out of core scope (spec §17).
const WALK_CAP: usize = 10_000;

impl Picker {
    pub fn new(root: PathBuf) -> Picker {
        // one root .gitignore + always-on .git + $TEDDY_IGNORE globs
        // (ponytail: the env var stands in for the deferred config file)
        let mut text = String::from(".git/\n");
        if let Ok(extra) = std::env::var("TEDDY_IGNORE") {
            for g in extra.split(':') {
                text.push_str(g);
                text.push('\n');
            }
        }
        if let Ok(gi) = std::fs::read_to_string(root.join(".gitignore")) {
            text.push_str(&gi);
        }
        let mut p = Picker {
            root,
            filter: String::new(),
            entries: Vec::new(),
            sel: 0,
            expanded: HashSet::new(),
            ignore: IgnoreRules::parse(&text),
            walk: None,
        };
        p.refresh();
        p
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
    }

    /// Non-ignored children of `dir`, dirs first, name-sorted.
    fn list_dir(&self, dir: &Path) -> Vec<(PathBuf, bool)> {
        let mut v: Vec<(PathBuf, bool)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let is_dir = e.file_type().ok()?.is_dir();
                let path = e.path();
                if self.ignore.is_ignored(&self.rel(&path), is_dir) {
                    return None;
                }
                Some((path, is_dir))
            })
            .collect();
        v.sort_by(|a, b| {
            (!a.1, a.0.file_name().map(|n| n.to_owned()))
                .cmp(&(!b.1, b.0.file_name().map(|n| n.to_owned())))
        });
        v
    }

    /// Rebuild `entries` for the current filter/expansion state. With a
    /// filter, results arrive progressively via `step`.
    pub fn refresh(&mut self) {
        self.entries.clear();
        if self.filter.is_empty() {
            self.walk = None;
            let root = self.root.clone();
            self.push_tree(&root, 0);
        } else {
            self.walk = Some(Walk {
                queue: VecDeque::from([(self.root.clone(), 0)]),
                visited: 0,
            });
        }
        self.sel = self.sel.min(self.entries.len().saturating_sub(1));
    }

    fn push_tree(&mut self, dir: &Path, depth: usize) {
        for (path, is_dir) in self.list_dir(dir) {
            let name = crate::buffer::display_name(&path);
            let expanded = is_dir && self.expanded.contains(&path);
            self.entries.push(Entry {
                path: path.clone(),
                depth,
                is_dir,
                name,
            });
            if expanded {
                self.push_tree(&path, depth + 1);
            }
        }
    }

    /// The filter text changed: restart the search walk.
    pub fn filter_changed(&mut self) {
        self.sel = 0;
        self.refresh();
    }

    pub fn wants_step(&self) -> bool {
        self.walk.as_ref().is_some_and(|w| !w.queue.is_empty())
    }

    /// One cooperative expand-on-search slice: visit up to `dirs` directories,
    /// appending matches to `entries`. Returns true when the walk finished.
    pub fn step(&mut self, dirs: usize) -> bool {
        let Some(mut w) = self.walk.take() else {
            return true;
        };
        let needle = self.filter.to_lowercase();
        for _ in 0..dirs {
            let Some((dir, depth)) = w.queue.pop_front() else {
                break;
            };
            for (path, is_dir) in self.list_dir(&dir) {
                let rel = self.rel(&path);
                if rel.to_lowercase().contains(&needle) {
                    self.entries.push(Entry {
                        path: path.clone(),
                        depth: 0,
                        is_dir,
                        name: chrome_rel(&self.root, &path),
                    });
                }
                if is_dir && w.visited < WALK_CAP {
                    w.visited += 1;
                    w.queue.push_back((path, depth + 1));
                }
            }
        }
        let done = w.queue.is_empty();
        self.walk = Some(w);
        done
    }

    /// Enter on the selection: toggles a directory (tree view only) or
    /// returns the file to open.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let e = self.entries.get(self.sel)?;
        if e.is_dir {
            let p = e.path.clone();
            if !self.expanded.remove(&p) {
                self.expanded.insert(p);
            }
            if self.filter.is_empty() {
                self.refresh();
            }
            None
        } else {
            Some(e.path.clone())
        }
    }
}

/// Search-result label: the root-relative path with control and invalid
/// bytes as `\xNN`, matching the tab name the file gets once opened.
fn chrome_rel(root: &Path, p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    crate::render::escape_chrome(p.strip_prefix(root).unwrap_or(p).as_os_str().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("teddy-picker-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("sub/b.txt"), b"b").unwrap();
        std::fs::write(root.join("sub/c.log"), b"c").unwrap();
        std::fs::write(root.join("sub/deep/needle.rs"), b"n").unwrap();
        std::fs::write(root.join(".git/x"), b"x").unwrap();
        std::fs::write(root.join(".gitignore"), b"*.log\n").unwrap();
        root
    }

    #[test]
    fn lazy_tree_expand_and_ignores() {
        let root = tree("tree");
        let mut p = Picker::new(root.clone());
        let names: Vec<&str> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["sub", ".gitignore", "a.txt"],
            "dirs first, .git ignored"
        );
        // expand sub: b.txt appears, c.log stays ignored, deep not read yet
        p.sel = 0;
        assert!(p.activate().is_none());
        let names: Vec<&str> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sub", "deep", "b.txt", ".gitignore", "a.txt"]);
        // picking a file returns it
        p.sel = 2;
        assert_eq!(p.activate(), Some(root.join("sub/b.txt")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn expand_on_search_finds_nested() {
        let root = tree("search");
        let mut p = Picker::new(root.clone());
        p.filter = "needle".into();
        p.filter_changed();
        while !p.step(4) {}
        let names: Vec<&str> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sub/deep/needle.rs"]);
        // ignored entries never match
        p.filter = "log".into();
        p.filter_changed();
        while !p.step(4) {}
        assert!(p.entries.is_empty(), "c.log is gitignored");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hostile_names_are_escaped_like_tabs() {
        use std::os::unix::ffi::OsStrExt;
        let root = tree("hostile");
        let evil = std::ffi::OsStr::from_bytes(b"ev\x1b[2J\xc2\x9b.txt");
        std::fs::write(root.join("sub").join(evil), b"e").unwrap();
        let mut p = Picker::new(root.clone());
        p.sel = 0;
        p.activate();
        assert!(p
            .entries
            .iter()
            .any(|e| e.name == "ev\\x1B[2J\\xC2\\x9B.txt"));
        p.filter = "ev".into();
        p.filter_changed();
        while !p.step(4) {}
        let names: Vec<&str> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sub/ev\\x1B[2J\\xC2\\x9B.txt"]);
        // filtering matches raw names, not their escaped display text
        p.filter = "x1b".into();
        p.filter_changed();
        while !p.step(4) {}
        assert!(p.entries.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
