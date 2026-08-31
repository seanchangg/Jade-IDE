//! Global AI preferences — the model tier and multi-line ghost mode.
//!
//! Old Jade persisted `aiModel` and `aiMultiline` as **global** preferences
//! (`app.ts:296-302`, `jade.state` load/save), distinct from the per-workspace
//! `aiCompletionEnabled` that lives in each workspace's `ui` blob
//! ([`crate::workspace_state`]). This module mirrors that split: one JSON file at
//! `~/.config/jade/ai.json`, loaded once at startup and rewritten whenever the
//! user picks a tier or toggles multi-line in the sparkle AI menu.
//!
//! Read errors are swallowed to defaults (Fast tier, single-line), same forgiving
//! behavior as [`crate::prefs`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use jade_ai::AiModelId;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AiPrefs {
    /// Managed-model tier (`aiModel`): serialized lowercase — `"fast"` /
    /// `"balanced"` / `"best"` (see `AiModelId`'s `serde(rename_all)`).
    #[serde(default)]
    pub model: AiModelId,
    /// Multi-line ghost mode (`aiMultiline`): 6-line blocks vs single line.
    #[serde(default)]
    pub multiline: bool,
    /// Visualize (§4.15) consent. Defaults to FALSE on purpose: the feature
    /// executes model-authored Python locally, and the first ⌘⇧M shows a
    /// one-time consent card instead of a request. Explain works without it.
    #[serde(default)]
    pub visualize_enabled: bool,

    #[serde(skip)]
    path: Option<PathBuf>,
}

impl AiPrefs {
    /// Default path: `~/.config/jade/ai.json`.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".config/jade/ai.json"))
    }

    /// Load from the default path, swallowing all errors to defaults.
    pub fn load() -> AiPrefs {
        match Self::default_path() {
            Some(path) => Self::load_from(&path),
            None => AiPrefs::default(),
        }
    }

    /// Load from an explicit path (test seam). Missing/corrupt → defaults, but
    /// the path is retained so a later [`save`](Self::save) writes it back.
    pub fn load_from(path: &PathBuf) -> AiPrefs {
        let mut prefs = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<AiPrefs>(&s).ok())
            .unwrap_or_default();
        prefs.path = Some(path.clone());
        prefs
    }

    /// Persist to the configured path (creating parent dirs). Errors swallowed.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_model_and_multiline() {
        let dir = std::env::temp_dir().join(format!("jade-aiprefs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("ai.json");

        // Fresh (missing file) → defaults: Fast tier, single-line, and —
        // security-relevant — Visualize OFF until consented.
        let mut p = AiPrefs::load_from(&path);
        assert_eq!(p.model, AiModelId::Fast);
        assert!(!p.multiline);
        assert!(!p.visualize_enabled, "visualize must default to off");

        p.model = AiModelId::Balanced;
        p.multiline = true;
        p.visualize_enabled = true;
        p.save();

        let loaded = AiPrefs::load_from(&path);
        assert_eq!(loaded.model, AiModelId::Balanced);
        assert!(loaded.multiline);
        assert!(loaded.visualize_enabled);

        // Tier serializes as the lowercase tag the old Jade wrote.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"balanced\""), "got: {raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
