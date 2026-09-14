//! Bundled UI icons — hand-inlined lucide glyphs (https://lucide.dev, MIT),
//! ported from the Electron renderer's `src/renderer/ui/icons.ts`.
//!
//! Each glyph is a standalone 24×24 SVG under `assets/icons/<name>.svg`, wrapped
//! exactly like the TS `icon()` helper (fill none, stroke-width 2, round
//! caps/joins) — except the stroke is `#000`, not `currentColor`: GPUI rasterizes
//! an SVG into an alpha mask and tints it with the element's text color, and usvg
//! won't resolve `currentColor` (it would render nothing). The tint comes from the
//! inherited `text_color` at paint time (see [`ui_icon`]), so the app's existing
//! hover/active color logic keeps working in both the dark and light themes.
//!
//! The files are embedded at compile time via `include_bytes!` (behind the
//! [`icons!`] macro), so a mistyped name is a *build* error, not a silent
//! render-nothing at runtime. [`Assets`] exposes them to GPUI through
//! [`gpui::AssetSource`]; it's registered on the `Application` in `main.rs` so
//! `gpui::svg().path("icons/<name>.svg")` resolves.

use std::borrow::Cow;

use anyhow::{Context as _, Result};
use gpui::{prelude::*, px, svg, AssetSource, SharedString, Svg};

/// Embed every icon file at compile time as `("icons/<name>.svg", bytes)`.
/// `include_bytes!` turns a typo in a name into a build error.
macro_rules! icons {
    ($($name:literal),+ $(,)?) => {
        /// `(asset-path, bytes)` for every bundled icon.
        const ICONS: &[(&str, &[u8])] = &[
            $((
                concat!("icons/", $name, ".svg"),
                include_bytes!(concat!("../assets/icons/", $name, ".svg")),
            )),+
        ];
    };
}

icons![
    // playback / build
    "play", "square", "hammer", "bug", "timer",
    // files — one glyph per file kind (see `workspace_tree::FileKind`)
    "folder", "folder-open", "file", "file-code", "code",
    "file-text", "file-terminal", "braces", "image", "table", "package", "lock",
    // data / telemetry
    "activity", "cpu", "memory-stick", "gauge", "layers",
    "grid-3x3", "flame", "box", "list-tree", "sigma",
    // chrome
    "settings", "x", "chevron-right", "chevron-down", "terminal",
    "eye", "eye-off",
    // toolbar extras
    "panel-left", "panel-right", "moon", "sun", "house",
    "corner-down-right", "circle", "circle-x", "circle-check", "triangle-alert", "info",
    // AI
    "sparkles",
    // added beyond icons.ts (lucide.dev, MIT): +/- controls + debug step glyphs
    "plus", "minus", "arrow-down", "arrow-down-to-line", "arrow-up-from-line",
    // CLion hand-off (research-studio seam)
    "external-link",
    // Explain / Visualize cards (§4.14, §4.15): retry a failed request,
    // pause the playing clip (play is already registered).
    "rotate-ccw", "pause",
    // Split editor panes (window management): the split-right button.
    "columns-2",
    // Hardware mode (board panel): step one clock, flash the real board.
    "skip-forward", "zap",
];

/// GPUI asset source over the compile-time-embedded icon set. Registered on the
/// `Application` at startup (`main.rs`); paths are `icons/<name>.svg`.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        // Surface a missing asset loudly (Err) rather than as an Ok(None) the
        // svg renderer would silently drop — mirrors zed's own Assets impl.
        let bytes = ICONS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, b)| Cow::Borrowed(*b))
            .with_context(|| format!("no bundled asset at {path:?}"))?;
        Ok(Some(bytes))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}

/// A themed UI icon: the bundled lucide SVG `name`, sized `size`×`size` px,
/// tinted `color` (`0xRRGGBB`). The color MUST be set on the svg element
/// itself: gpui's Svg paints only when its OWN style has a text color
/// (`svg.rs: self.path.zip(style.text.color)`) — parent `.text_color(..)`
/// does NOT cascade into it, which is why icons rendered as blank space
/// before this took an explicit color. `name` is the bare glyph name
/// (e.g. `"play"`), not the asset path.
pub fn ui_icon(name: &str, size: f32, color: u32) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(size))
        .text_color(gpui::rgb(color))
        .flex_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name the UI asks for must resolve through the AssetSource, so a
    /// typo'd `ui_icon(..)` path fails here in CI instead of silently rendering
    /// nothing at paint time. Keep this list in sync with the call sites.
    const UI_ICON_NAMES: &[&str] = &[
        // action bar (app.rs)
        "panel-left", "terminal", "corner-down-right", "gauge",
        "code", "hammer", "play", "bug", "square",
        "sparkles", "eye", "eye-off", "moon", "sun",
        "circle-x", "circle-check", "triangle-alert", "info",
        // file tree (panels/file_tree.rs) — folder + one glyph per FileKind
        "folder", "folder-open", "file", "file-code", "minus",
        "code", "cpu", "file-terminal", "hammer", "settings",
        "braces", "table", "box", "file-text", "image", "package", "lock",
        // tab strip (panels/code_view.rs)
        "x",
        // panel headers
        "list-tree", "activity", "box",
        // debug controls (panels/debug_panel.rs)
        "arrow-down", "arrow-down-to-line", "arrow-up-from-line",
        // bottom panel (app.rs)
        "plus",
        // Explain card (panels/explain_card.rs)
        "external-link", "rotate-ccw",
        // Visualize card (panels/visualize_card.rs)
        "pause", "play",
        // Hardware mode: board panel + action bar (panels/board_view.rs)
        "skip-forward", "zap", "timer", "cpu",
    ];

    #[test]
    fn every_ui_icon_resolves() {
        let assets = Assets;
        for name in UI_ICON_NAMES {
            let path = format!("icons/{name}.svg");
            let loaded = assets
                .load(&path)
                .unwrap_or_else(|e| panic!("icon {name:?}: {e}"));
            let bytes = loaded.unwrap_or_else(|| panic!("icon {name:?} resolved to None"));
            assert!(
                bytes.starts_with(b"<svg"),
                "icon {name:?} is not an SVG document"
            );
        }
    }

    #[test]
    fn missing_icon_is_an_error() {
        assert!(Assets.load("icons/does-not-exist.svg").is_err());
    }
}
