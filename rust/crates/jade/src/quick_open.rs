//! Quick Open matcher + input state (feature inventory §5.7).
//!
//! Pure logic, no GPUI: the ⌘P overlay in `panels/quick_open.rs` is a thin
//! renderer over this. Mirrors the Electron `app.ts:863-1002` behaviour exactly —
//! **substring** filename match, **case-insensitive**, **10 results max**, a
//! relative-path hint shown **only when it differs from the bare name**, and a
//! file list cached per workspace (the cache lives in `JadeApp`; this module just
//! flattens a scanned tree and filters it).
//!
//! Keyboard handling ([`QuickOpenState::on_key`]) is here too, so the printable-
//! char/backspace/arrow/enter/escape logic is unit-tested without a window: the
//! overlay is a captured-keystroke buffer (no full text-input widget).

use std::path::{Path, PathBuf};

use crate::workspace_tree::{FileNode, FileTree};

/// One openable file in the workspace (directories excluded). `name` is the bare
/// filename; `path` is absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
}

/// A filtered result: the file plus an optional relative-path hint (present only
/// when it differs from the bare name, per §5.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub name: String,
    pub path: PathBuf,
    pub hint: Option<String>,
}

/// Max results shown (§5.7: "10 results max").
pub const MAX_RESULTS: usize = 10;

/// Flatten a scanned [`FileTree`] into the cacheable file list (files only,
/// directories skipped), mirroring the TS `flattenTree`. Because the tree is
/// lazily loaded, unexpanded directories contribute only what was scanned; the
/// cache is rebuilt from a fresh full-ish scan by the caller when needed.
pub fn flatten(tree: &FileTree) -> Vec<FileEntry> {
    let mut out = Vec::new();
    flatten_nodes(&tree.roots, &mut out);
    out
}

fn flatten_nodes(nodes: &[FileNode], out: &mut Vec<FileEntry>) {
    for n in nodes {
        // The per-workspace state dir is Jade's own, never something to open.
        if n.is_dir && n.name == ".jade" {
            continue;
        }
        if n.is_dir {
            if let Some(children) = &n.children {
                flatten_nodes(children, out);
            }
        } else {
            out.push(FileEntry {
                name: n.name.clone(),
                path: n.path.clone(),
            });
        }
    }
}

/// The relative-path hint for a file under `root`: the path with the root prefix
/// and any leading separator stripped. `None` when it equals the bare name (a
/// root-level file), matching `relPath !== file.name` in the TS.
fn hint_for(entry: &FileEntry, root: &Path) -> Option<String> {
    let rel = entry
        .path
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| entry.path.to_string_lossy().into_owned());
    let rel = rel.trim_start_matches(std::path::MAIN_SEPARATOR).to_string();
    if rel == entry.name || rel.is_empty() {
        None
    } else {
        Some(rel)
    }
}

/// Filter the cached list by `query` (substring, case-insensitive), capped at
/// [`MAX_RESULTS`]. An empty query returns the first `MAX_RESULTS` files (the TS
/// `cachedFileList.slice(0, 10)` behaviour).
pub fn filter(files: &[FileEntry], query: &str, root: &Path) -> Vec<Match> {
    let q = query.to_lowercase();
    files
        .iter()
        .filter(|f| q.is_empty() || f.name.to_lowercase().contains(&q))
        .take(MAX_RESULTS)
        .map(|f| Match {
            name: f.name.clone(),
            path: f.path.clone(),
            hint: hint_for(f, root),
        })
        .collect()
}

/// Transient input state for the overlay (query buffer + selected row). Held as
/// `Option<QuickOpenState>` on `JadeApp`; `Some` == overlay open.
#[derive(Debug, Clone, Default)]
pub struct QuickOpenState {
    pub query: String,
    pub selected: usize,
}

/// What the caller should do after a keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Re-render (query/selection changed, or nothing to do).
    None,
    /// Close the overlay (Escape).
    Close,
    /// Open this file and close the overlay (Enter / click).
    Open(PathBuf),
}

impl QuickOpenState {
    /// Apply one captured keystroke. `key` is the GPUI `Keystroke::key` (e.g.
    /// "escape", "enter", "up", "down", "backspace", "a"); `key_char` is the
    /// printable character if any (`None` for modified/navigation keys). `results`
    /// is the currently-filtered match list (so Enter/selection clamp correctly).
    ///
    /// A printable char is appended only when `key_char` is a single printable
    /// character and no command/control/alt modifier is set (`printable`), so
    /// ⌘P bubbles past the overlay to the global toggle instead of typing "p".
    pub fn on_key(&mut self, key: &str, key_char: Option<&str>, printable: bool, results: &[Match]) -> KeyAction {
        match key {
            "escape" => KeyAction::Close,
            "enter" => {
                if let Some(m) = results.get(self.selected) {
                    KeyAction::Open(m.path.clone())
                } else {
                    KeyAction::None
                }
            }
            "down" => {
                if !results.is_empty() {
                    self.selected = (self.selected + 1).min(results.len() - 1);
                }
                KeyAction::None
            }
            "up" => {
                self.selected = self.selected.saturating_sub(1);
                KeyAction::None
            }
            "backspace" => {
                self.query.pop();
                self.selected = 0;
                KeyAction::None
            }
            _ => {
                if printable {
                    if let Some(ch) = key_char {
                        if ch.chars().count() == 1 && !ch.chars().any(|c| c.is_control()) {
                            self.query.push_str(ch);
                            self.selected = 0;
                        }
                    }
                }
                KeyAction::None
            }
        }
    }

    /// Clamp the selection into `[0, len)` after a re-filter (query narrowed).
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<FileEntry> {
        vec![
            FileEntry { name: "main.cpp".into(), path: "/ws/main.cpp".into() },
            FileEntry { name: "Widget.cpp".into(), path: "/ws/src/Widget.cpp".into() },
            FileEntry { name: "widget.h".into(), path: "/ws/src/widget.h".into() },
            FileEntry { name: "notes.txt".into(), path: "/ws/docs/notes.txt".into() },
        ]
    }

    #[test]
    fn substring_case_insensitive() {
        let e = entries();
        let root = Path::new("/ws");
        let m = filter(&e, "widget", root);
        // Matches both Widget.cpp and widget.h (case-insensitive substring).
        let names: Vec<&str> = m.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["Widget.cpp", "widget.h"]);
    }

    #[test]
    fn empty_query_returns_all_capped() {
        let e = entries();
        let m = filter(&e, "", Path::new("/ws"));
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn ten_result_cap() {
        let mut e = Vec::new();
        for i in 0..25 {
            e.push(FileEntry {
                name: format!("file{i}.cpp"),
                path: PathBuf::from(format!("/ws/file{i}.cpp")),
            });
        }
        assert_eq!(filter(&e, "file", Path::new("/ws")).len(), MAX_RESULTS);
        assert_eq!(filter(&e, "", Path::new("/ws")).len(), MAX_RESULTS);
    }

    #[test]
    fn path_hint_only_when_differs_from_name() {
        let e = entries();
        let root = Path::new("/ws");
        // Root-level file: hint suppressed (rel == name).
        let main = filter(&e, "main", root);
        assert_eq!(main[0].hint, None);
        // Nested file: hint is the relative path.
        let w = filter(&e, "Widget.cpp", root);
        assert_eq!(w[0].hint.as_deref(), Some("src/Widget.cpp"));
    }

    #[test]
    fn keys_type_navigate_open_close() {
        let e = entries();
        let root = Path::new("/ws");
        let mut st = QuickOpenState::default();

        // Type "w" → query updates, appends printable.
        assert_eq!(st.on_key("w", Some("w"), true, &[]), KeyAction::None);
        assert_eq!(st.query, "w");

        let results = filter(&e, &st.query, root);
        // Arrow down moves selection, clamped to results.
        st.on_key("down", None, false, &results);
        assert_eq!(st.selected, 1);
        st.on_key("down", None, false, &results); // clamp at len-1
        assert_eq!(st.selected, 1);
        st.on_key("up", None, false, &results);
        assert_eq!(st.selected, 0);

        // Enter opens the selected result.
        match st.on_key("enter", None, false, &results) {
            KeyAction::Open(p) => assert_eq!(p, PathBuf::from("/ws/src/Widget.cpp")),
            other => panic!("expected Open, got {other:?}"),
        }

        // Backspace pops; Escape closes.
        st.on_key("backspace", None, false, &results);
        assert_eq!(st.query, "");
        assert_eq!(st.on_key("escape", None, false, &results), KeyAction::Close);
    }

    #[test]
    fn modified_keys_do_not_type() {
        let mut st = QuickOpenState::default();
        // ⌘P arrives as key "p" with printable=false (cmd modifier) → no typing.
        st.on_key("p", None, false, &[]);
        assert_eq!(st.query, "");
    }
}
