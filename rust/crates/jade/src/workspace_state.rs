//! Per-workspace UI persistence (feature inventory §1.2).
//!
//! Jade bundles its UI state into the single `ui` key of
//! `<workspaceRoot>/.jade/workspace.json` — the **same file and key** the
//! Electron app used (`state.ts:96-139`), so an existing workspace opens with its
//! tabs, breakpoints, and benchmarks intact.
//!
//! `PERSISTED_KEYS` here = the Electron list minus `stickyNotes` (owned by
//! [`crate::notes`], which read-modify-writes the same file) and the dead
//! `fileTreeWidth`: `openTabs, activeTabIndex, fileTreeVisible, terminalVisible,
//! memoryBarVisible, terminalHeight, breakpoints, benchmarks, aiCompletionEnabled`.
//!
//! [`save_to`] is a **read-modify-write that merges** — it writes only Jade's
//! keys into the existing `ui` object, so `stickyNotes` (and any unknown Electron
//! key) survive. The app arms a 1500ms debounce around it (`state.ts:130`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::benchmark::Benchmark;

/// One persisted open tab (Electron `TabState`, `types.ts:23-27`). `viewState`
/// (Monaco-specific) is intentionally ignored on load and omitted on save.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabState {
    pub path: String,
    #[serde(default)]
    pub is_dirty: bool,
}

/// The subset of the `ui` blob Jade owns (§1.2). Every field is optional /
/// defaulted so a partial or Electron-written file loads cleanly, and the
/// `skip_serializing_if` on the toggles means a `None` never clobbers an existing
/// value on merge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceUi {
    #[serde(default)]
    pub open_tabs: Vec<TabState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_tree_visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bar_visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_height: Option<f64>,
    #[serde(default)]
    pub breakpoints: HashMap<String, Vec<u32>>,
    #[serde(default)]
    pub benchmarks: Vec<Benchmark>,
    /// CPU counter events chosen in the RUNTIME panel's COUNTERS section.
    /// Empty means the defaults.
    #[serde(default)]
    pub counter_events: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_completion_enabled: Option<bool>,
    /// Timer bundles (synthetic summed series, e.g. "Forward") — the member
    /// kernel names are project-specific, so definitions live per-workspace.
    /// ALWAYS serialized (never skip-if-empty): the merge in [`save_to`]
    /// removes absent owned keys, so a skipped empty vec would erase persisted
    /// bundles any time a save ran while none were loaded. Explicit `[]` means
    /// "user has no bundles"; absence must only ever mean "older writer".
    #[serde(default)]
    pub timer_groups: Vec<crate::timer_groups::TimerGroupDef>,
    /// App mode for this workspace: `"software"` or `"hardware"` (hardware
    /// mode, §B8). Absent = software.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Board DIP-switch positions (hardware mode), LSB first. `true` = ON.
    #[serde(default)]
    pub dip_switches: Vec<bool>,
    /// Board drawer width in px (hardware mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_width: Option<f64>,
    /// Whether the Markdown preview panel is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown_visible: Option<bool>,
    /// Markdown preview panel width in px.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown_width: Option<f64>,
    /// Split editor panes in display order (window management). Absent or
    /// shorter than two = one pane, which `open_tabs` describes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneState>,
    /// Index into `panes` of the pane that has the keyboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane: Option<i64>,
}

/// One split pane: its tab set, active tab, and Markdown view mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneState {
    #[serde(default)]
    pub open_tabs: Vec<TabState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab_index: Option<i64>,
    /// A Markdown tab shows as formatted blocks (true) or raw source (false).
    #[serde(default = "default_true")]
    pub md_rendered: bool,
    /// Width share of the pane row (flex grow); 1.0 = an equal share.
    #[serde(default = "default_weight")]
    pub weight: f64,
}

fn default_true() -> bool {
    true
}

fn default_weight() -> f64 {
    1.0
}

/// The keys Jade writes into the `ui` object (everything except `stickyNotes`,
/// which [`crate::notes`] owns). Used by the merge in [`save_to`].
const OWNED_KEYS: &[&str] = &[
    "openTabs",
    "activeTabIndex",
    "fileTreeVisible",
    "terminalVisible",
    "memoryBarVisible",
    "terminalHeight",
    "breakpoints",
    "benchmarks",
    "counterEvents",
    "aiCompletionEnabled",
    "timerGroups",
    "mode",
    "dipSwitches",
    "boardWidth",
    "markdownVisible",
    "markdownWidth",
    "panes",
    "focusedPane",
];

/// Load Jade's UI state for a workspace (`ui` key), or the default on any error.
/// `<workspace>/.jade/workspace.json` — the shared per-workspace state file
/// (same file the Electron app writes; saves merge-preserve foreign keys).
fn workspace_json_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".jade").join("workspace.json")
}

pub fn load(workspace_root: &Path) -> WorkspaceUi {
    load_from(&workspace_json_path(workspace_root))
}

/// Load from an explicit `workspace.json` path (test seam).
pub fn load_from(path: &Path) -> WorkspaceUi {
    let Ok(s) = std::fs::read_to_string(path) else {
        return WorkspaceUi::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
        return WorkspaceUi::default();
    };
    v.get("ui")
        .and_then(|ui| serde_json::from_value::<WorkspaceUi>(ui.clone()).ok())
        .unwrap_or_default()
}

/// Persist Jade's UI state under `ui`, **merging** — only Jade's keys are
/// written, so `stickyNotes` and any other key already in the file survive.
pub fn save(workspace_root: &Path, ui: &WorkspaceUi) {
    save_to(&workspace_json_path(workspace_root), ui);
}

/// Save to an explicit path (test seam). Errors swallowed (like the notes path).
pub fn save_to(path: &Path, ui: &WorkspaceUi) {
    let mut root: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let ui_val = obj.entry("ui").or_insert_with(|| serde_json::json!({}));
    if !ui_val.is_object() {
        *ui_val = serde_json::json!({});
    }
    let ui_obj = ui_val.as_object_mut().unwrap();

    // Serialize our struct and copy only our owned keys into the existing object,
    // leaving stickyNotes / unknown keys untouched.
    if let Ok(serde_json::Value::Object(mine)) = serde_json::to_value(ui) {
        for key in OWNED_KEYS {
            if let Some(v) = mine.get(*key) {
                ui_obj.insert((*key).to_string(), v.clone());
            } else {
                // A skipped Option (None) — remove any stale value we own so the
                // file reflects "unset" rather than an old toggle.
                ui_obj.remove(*key);
            }
        }
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(&root) {
        let _ = std::fs::write(path, s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jade-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(".jade").join("workspace.json")
    }

    #[test]
    fn roundtrip_full_state() {
        let path = tmp("rt");
        let ui = WorkspaceUi {
            counter_events: Vec::new(),
            open_tabs: vec![
                TabState { path: "/p/a.cpp".into(), is_dirty: false },
                TabState { path: "/p/b.cpp".into(), is_dirty: true },
            ],
            active_tab_index: Some(1),
            file_tree_visible: Some(true),
            terminal_visible: Some(false),
            memory_bar_visible: Some(true),
            terminal_height: Some(240.0),
            breakpoints: HashMap::from([("/p/a.cpp".to_string(), vec![10u32, 20])]),
            benchmarks: vec![Benchmark {
                name: "#1 -O3".into(),
                flags: "-O3".into(),
                duration: 123.0,
                peak_allocation: 4096,
                alloc_count: 7,
                timestamp: 1700000000000,
            }],
            ai_completion_enabled: Some(false),
            timer_groups: vec![crate::timer_groups::TimerGroupDef {
                name: "Forward".into(),
                members: vec!["embedForward".into(), "linearForward".into()],
            }],
            mode: Some("hardware".into()),
            dip_switches: vec![true, false, true, false, false],
            board_width: Some(440.0),
            markdown_visible: Some(true),
            markdown_width: Some(420.0),
            panes: vec![
                PaneState {
                    open_tabs: vec![TabState {
                        path: "/p/a.cpp".into(),
                        is_dirty: false,
                    }],
                    active_tab_index: Some(0),
                    md_rendered: true,
                    weight: 1.5,
                },
                PaneState {
                    open_tabs: vec![TabState {
                        path: "/p/README.md".into(),
                        is_dirty: true,
                    }],
                    active_tab_index: Some(0),
                    md_rendered: false,
                    weight: 0.5,
                },
            ],
            focused_pane: Some(1),
        };
        save_to(&path, &ui);
        assert_eq!(load_from(&path), ui);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn save_merges_and_preserves_sticky_notes() {
        let path = tmp("merge");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A file the notes module already wrote (stickyNotes + an unknown key).
        std::fs::write(
            &path,
            r#"{ "ui": { "stickyNotes": [{"id":"n1"}], "someFutureKey": 42 } }"#,
        )
        .unwrap();

        let ui = WorkspaceUi {
            open_tabs: vec![TabState { path: "/x.cpp".into(), is_dirty: false }],
            active_tab_index: Some(0),
            ..Default::default()
        };
        save_to(&path, &ui);

        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Our keys were written…
        assert_eq!(v["ui"]["openTabs"][0]["path"], serde_json::json!("/x.cpp"));
        assert_eq!(v["ui"]["activeTabIndex"], serde_json::json!(0));
        // …and the notes + unknown keys survived the merge.
        assert_eq!(v["ui"]["stickyNotes"][0]["id"], serde_json::json!("n1"));
        assert_eq!(v["ui"]["someFutureKey"], serde_json::json!(42));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    /// Migrate a full Electron-written workspace.json (camelCase, with the exact
    /// `openTabs`/`breakpoints`/`benchmarks` shapes from `shared/types.ts` +
    /// `runtime-panel.ts`, including a Monaco `viewState` we ignore).
    #[test]
    fn migrates_full_electron_workspace() {
        let path = tmp("mig");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r##"{
              "ui": {
                "fileTreeVisible": true,
                "terminalVisible": true,
                "memoryBarVisible": false,
                "fileTreeWidth": 260,
                "terminalHeight": 280,
                "stickyNotes": [],
                "openTabs": [
                  { "path": "/proj/main.cpp", "viewState": { "cursorState": [] }, "isDirty": false },
                  { "path": "/proj/util.hpp", "isDirty": true }
                ],
                "activeTabIndex": 1,
                "breakpoints": { "/proj/main.cpp": [12, 40], "/proj/util.hpp": [5] },
                "benchmarks": [
                  {
                    "name": "#3 -O3 -march=native",
                    "flags": "-O3 -march=native",
                    "duration": 88.5,
                    "peakAllocation": 1048576,
                    "allocCount": 42,
                    "timestamp": 1700000000000
                  }
                ],
                "aiCompletionEnabled": false
              }
            }"##,
        )
        .unwrap();

        let ui = load_from(&path);
        assert_eq!(ui.open_tabs.len(), 2);
        assert_eq!(ui.open_tabs[0].path, "/proj/main.cpp");
        assert!(!ui.open_tabs[0].is_dirty);
        assert!(ui.open_tabs[1].is_dirty);
        assert_eq!(ui.active_tab_index, Some(1));
        assert_eq!(ui.file_tree_visible, Some(true));
        assert_eq!(ui.memory_bar_visible, Some(false));
        assert_eq!(ui.terminal_height, Some(280.0));
        assert_eq!(ui.breakpoints["/proj/main.cpp"], vec![12, 40]);
        assert_eq!(ui.breakpoints["/proj/util.hpp"], vec![5]);
        assert_eq!(ui.benchmarks.len(), 1);
        assert_eq!(ui.benchmarks[0].name, "#3 -O3 -march=native");
        assert_eq!(ui.benchmarks[0].duration, 88.5);
        assert_eq!(ui.benchmarks[0].peak_allocation, 1048576);
        assert_eq!(ui.ai_completion_enabled, Some(false));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn missing_file_is_default() {
        let ui = load_from(Path::new("/nonexistent/jade/workspace.json"));
        assert_eq!(ui, WorkspaceUi::default());
        assert!(ui.open_tabs.is_empty());
    }

    /// Hardware-mode fields round-trip; a file without them defaults to
    /// software with no DIP state (§B8).
    #[test]
    fn mode_and_dips_roundtrip_and_default_absent() {
        let path = tmp("hw");
        let ui = WorkspaceUi {
            mode: Some("hardware".into()),
            dip_switches: vec![false, true, false, false, true],
            board_width: Some(512.0),
            ..Default::default()
        };
        save_to(&path, &ui);
        let back = load_from(&path);
        assert_eq!(back.mode.as_deref(), Some("hardware"));
        assert_eq!(back.dip_switches, vec![false, true, false, false, true]);
        assert_eq!(back.board_width, Some(512.0));

        let fresh = WorkspaceUi::default();
        assert_eq!(fresh.mode, None);
        assert!(fresh.dip_switches.is_empty());
        assert_eq!(fresh.board_width, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
