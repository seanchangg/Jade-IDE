//! Bundled-font registration + the app's monospace family helper (Phase-5b).
//!
//! Every code/terminal surface renders through [`mono_family`] so the whole app
//! switches fonts together. Menlo is the guaranteed-present macOS fallback;
//! [`register_bundled_fonts`] flips the resolved family to "JetBrains Mono" at
//! startup *iff* the `.app` bundle shipped the TTFs in `Contents/Resources/fonts`.
//!
//! No network / install assumptions: the packaging script drops whatever TTFs it
//! finds into that dir, and this module loads them from the relocated bundle at
//! runtime (resolved relative to the executable, not `CARGO_MANIFEST_DIR`). A dev
//! `cargo run` finds no bundle dir and simply keeps Menlo — dropping
//! `JetBrainsMono-*.ttf` into the bundle's `Resources/fonts` is all it takes.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::App;

/// Guaranteed-present macOS monospace fallback.
const FALLBACK_MONO: &str = "Menlo";
/// Preferred family, used only once its TTFs are registered at startup.
const PREFERRED_MONO: &str = "JetBrains Mono";

/// Set by [`register_bundled_fonts`] when JetBrains Mono TTFs were loaded.
static JETBRAINS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// The monospace family name every `.font_family(..)` code/terminal call site
/// resolves through. Returns "JetBrains Mono" once the bundled TTFs are
/// registered, otherwise "Menlo".
pub fn mono_family() -> &'static str {
    if JETBRAINS_REGISTERED.load(Ordering::Relaxed) {
        PREFERRED_MONO
    } else {
        FALLBACK_MONO
    }
}

/// Font features for every window root: `calt` off, so JetBrains Mono never
/// merges `<=`, `->`, or `!=` into one ligature glyph. Code must show the
/// characters the file holds.
pub fn code_features() -> gpui::FontFeatures {
    gpui::FontFeatures::disable_ligatures()
}

/// Load every `*.ttf`/`*.otf` shipped in the app bundle's `Resources/fonts/` dir
/// and hand them to GPUI's text system. Call once, before the first window opens.
/// Outside a bundle (dev builds) or with an empty fonts dir this is a no-op and
/// the app keeps Menlo.
pub fn register_bundled_fonts(cx: &App) {
    let Some(dir) = bundle_fonts_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    let mut blobs: Vec<Cow<'static, [u8]>> = Vec::new();
    let mut has_jetbrains = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_font = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("otf"));
        if !is_font {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if name.contains("jetbrains") {
                    has_jetbrains = true;
                }
                blobs.push(Cow::Owned(bytes));
            }
            Err(e) => eprintln!("[jade] skipping font {}: {e}", path.display()),
        }
    }

    if blobs.is_empty() {
        return;
    }
    match cx.text_system().add_fonts(blobs) {
        Ok(()) => {
            if has_jetbrains {
                JETBRAINS_REGISTERED.store(true, Ordering::Relaxed);
            }
        }
        Err(e) => eprintln!("[jade] bundled font registration failed: {e}"),
    }
}

/// `.../Jade.app/Contents/MacOS/jade` → `.../Jade.app/Contents/Resources/fonts`.
/// Dev builds (`target/debug/jade`) fall back to the repo's packaging drop-in
/// dir `rust/scripts/resources/fonts` (exe → debug → target → rust), so
/// `cargo run` gets the same bundled family as the shipped .app. `None` when
/// neither exists or the exe path can't be resolved.
fn bundle_fonts_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // exe = Contents/MacOS/jade → parent(MacOS) → parent(Contents)
    let contents = exe.parent()?.parent()?;
    let dir = contents.join("Resources").join("fonts");
    if dir.is_dir() {
        return Some(dir);
    }
    // target/debug/jade → debug → target → rust → scripts/resources/fonts
    let dev = exe
        .parent()?
        .parent()?
        .parent()?
        .join("scripts")
        .join("resources")
        .join("fonts");
    dev.is_dir().then_some(dev)
}
