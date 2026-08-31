//! Visualize (inventory §4.15): the pure logic behind ⌘⇧M.
//!
//! The user selects code and presses ⌘⇧M. A card appears. The model writes a
//! Manim scene as one schema-validated JSON response; `jade-build` renders it
//! to an mp4 in a sandbox; the card plays it.
//!
//! Like [`crate::explain`], everything here is pure: no gpui, no tokio, no
//! I/O. The card's element tree lives in `panels::visualize_card` and the
//! orchestration in `app.rs`. The context builder is shared with Explain
//! ([`crate::explain::build_user_message`]) — same selection, same
//! declarations, same outline; only the system prompt and the schema differ.
//!
//! Two gates keep the feature off code that has nothing to animate:
//!
//!   1. [`precheck`] — a free, local shape check. It rejects non-code
//!      languages and selections with no executable content before a request
//!      is spent.
//!   2. The model itself — the schema carries a required `suitable` field,
//!      and the prompt tells the model to decline a fragment whose animation
//!      would only decorate the code. The card then shows the reason instead
//!      of a pointless clip.

use std::ops::Range;
use std::path::PathBuf;

use jade_ai::{ChatDelta, ChatError, ChatRequest, Effort, StopReason};

use crate::explain::{build_user_message, Selected};

// ── The local gate ───────────────────────────────────────────────────────────

/// Why [`precheck`] refused a selection. Each maps to one toast line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrecheckReject {
    /// The file is data or prose, not a program.
    NotCode,
    /// Comments and blank lines only.
    NoExecutableCode,
}

impl PrecheckReject {
    pub fn message(self) -> &'static str {
        match self {
            PrecheckReject::NotCode => {
                "This file is data, not code. Select a code fragment to visualize."
            }
            PrecheckReject::NoExecutableCode => {
                "That selection has no executable code to visualize."
            }
        }
    }
}

/// Languages [`crate::explain::language_name`] can return that hold data or
/// prose. An animation of a TOML table is exactly the "unnecessary
/// visualizer" this gate exists to prevent.
const DATA_LANGUAGES: &[&str] = &["json", "toml", "markdown", "text"];

/// The free gate, run before any request is sent.
///
/// Rejects data languages, and selections whose every line is blank, a
/// comment, or bare punctuation. Anything that plausibly computes passes —
/// the model's `suitable` verdict is the judge of whether it is WORTH
/// animating; this only refuses what certainly is not.
pub fn precheck(language: &str, text: &str) -> Result<(), PrecheckReject> {
    if DATA_LANGUAGES.contains(&language) {
        return Err(PrecheckReject::NotCode);
    }
    let mut executable = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Comments: C-family, Python/shell, and block-comment interiors.
        if t.starts_with("//") || t.starts_with('#') && !t.starts_with("#include") {
            continue;
        }
        if t.starts_with("/*") || t.starts_with('*') || t.starts_with("*/") {
            continue;
        }
        // Bare punctuation: closing braces, semicolons, include guards' ends.
        if t.chars().all(|c| !c.is_alphanumeric()) {
            continue;
        }
        // `#include <x>` resolves names; it does not compute.
        if t.starts_with("#include") || t.starts_with("#pragma") {
            continue;
        }
        executable = true;
        break;
    }
    if executable {
        Ok(())
    } else {
        Err(PrecheckReject::NoExecutableCode)
    }
}

// ── The request ──────────────────────────────────────────────────────────────

/// Held byte-stable on purpose: it is the cached prefix (see
/// [`crate::explain::EXPLAIN_SYSTEM`]).
pub const VISUALIZE_SYSTEM: &str = "\
You write a short Manim animation that shows what a fragment of source code \
does, for the developer who is reading it in their editor.

First decide whether the fragment is WORTH animating. An animation is worth \
making only when it can show data that moves or control that flows: values \
that change, elements that shuffle, pointers that walk, branches that select, \
memory that grows. If the fragment only declares, configures, imports, or \
names things — declarations, constants, struct fields, includes, boilerplate, \
plain assignments with no consequence — set \"suitable\" to false, put one \
short sentence in \"reason\" saying why, set \"title\" and \"script\" to empty \
strings, set \"duration_s\" to 0, and stop. Do not force an animation onto a \
fragment that has nothing to show. A caption over a screenshot of the code is \
not an animation.

If the fragment is suitable, set \"suitable\" to true, put one short sentence \
in \"reason\" saying what the animation shows, give a title of at most six \
words, and write the scene.

Rules for the script — every one is enforced by a validator, and a violation \
is discarded unrun:
- Target the Manim Community API, v0.19 or later.
- The only import is `from manim import *`, on the first line.
- Define exactly one class: `class JadeScene(Scene)`. No other class, no \
top-level statement, no other import anywhere, at any indent.
- Never use os, sys, subprocess, socket, shutil, pathlib, __import__, open, \
eval, exec, or compile. The script cannot read the network or the disk.
- Use only these mobjects: Text, Paragraph, Code, Square, Rectangle, \
RoundedRectangle, Circle, Dot, Line, Arrow, DoubleArrow, CurvedArrow, Brace, \
NumberLine, Axes, Table, VGroup, SurroundingRectangle, DashedLine, Polygon, \
Angle, Arc, MathTex, Tex.
- Use only these animations: Create, Write, FadeIn, FadeOut, Transform, \
ReplacementTransform, TransformMatchingShapes, MoveToTarget, Indicate, \
Circumscribe, Flash, GrowArrow, GrowFromCenter, LaggedStart, AnimationGroup, \
Succession, Wiggle, ApplyWave, and the .animate syntax.
- Prefer Text over MathTex; use MathTex only when the mathematics itself is \
the point.
- The animation must run 6 to 12 seconds. Put the total in \"duration_s\".
- Keep everything inside the 16:9 frame: config.frame_width is 14.22 units \
and config.frame_height is 8. Scale or arrange groups so nothing leaves it.
- The animation must show the data moving or the control flowing. Show the \
values the fragment transforms, step by step, in the order the code runs. Do \
not decorate the code with arrows; animate what the code does to its data.
- Use concrete example values that make the behavior visible.
- Keep the scene simple: at most ten mobjects on screen at once.

Return ONLY the JSON object the schema describes.";

pub const VISUALIZE_MAX_TOKENS: u32 = 8000;

/// The schema of the one JSON response. `suitable` is the model-side gate:
/// required, so the model must decide before it writes a line of the scene.
pub fn visualize_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["suitable", "reason", "title", "script", "duration_s"],
        "properties": {
            "suitable":   { "type": "boolean" },
            "reason":     { "type": "string" },
            "title":      { "type": "string" },
            "script":     { "type": "string" },
            "duration_s": { "type": "number" }
        }
    })
}

/// Build the Visualize request. One request, `Lane::Visualize`.
pub fn visualize_request(sel: &Selected<'_>) -> ChatRequest {
    ChatRequest {
        system: VISUALIZE_SYSTEM.to_string(),
        user: build_user_message(sel),
        max_tokens: VISUALIZE_MAX_TOKENS,
        // Writing a scene is a design task, not a lookup.
        effort: Effort::Medium,
        json_schema: Some(visualize_schema()),
        // Thinking time and a few hundred lines of Python both count
        // against this.
        timeout: std::time::Duration::from_secs(150),
    }
}

// ── The response ─────────────────────────────────────────────────────────────

/// The parsed response.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ScenePlan {
    pub suitable: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub script: String,
    #[serde(default)]
    pub duration_s: f64,
}

/// Parse the accumulated JSON. Because Visualize is schema-forced, the
/// response is ONLY JSON — no fences, no prose tail, no partial-JSON
/// scanning.
pub fn parse_scene_plan(json: &str) -> Result<ScenePlan, String> {
    serde_json::from_str::<ScenePlan>(json.trim())
        .map_err(|e| format!("the response was not the expected JSON: {e}"))
}

// ── The card ─────────────────────────────────────────────────────────────────

/// What broke, when something did. Carried in the phase so the card can word
/// the banner.
#[derive(Debug, Clone, PartialEq)]
pub enum VisualizeError {
    /// The request itself failed.
    Chat(ChatError),
    /// The response hit the token cap. Unlike Explain, a truncated script
    /// cannot run, so this is a failure and not a result.
    Truncated,
    /// The model returned JSON we could not read, or a script the gate
    /// refused.
    BadScript(String),
    /// Manim failed. Carries manim's own last line.
    Render(String),
}

impl VisualizeError {
    pub fn headline(&self) -> &'static str {
        match self {
            VisualizeError::Chat(e) => e.headline(),
            VisualizeError::Truncated => "The scene was cut off",
            VisualizeError::BadScript(_) => "The scene failed validation",
            VisualizeError::Render(_) => "The render failed",
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            VisualizeError::Chat(e) => e.detail(),
            VisualizeError::Truncated => {
                Some("The response hit the token cap. Retry, or select a smaller fragment.".into())
            }
            VisualizeError::BadScript(d) | VisualizeError::Render(d) => Some(d.clone()),
        }
    }

    /// Whether a retry can plausibly succeed. A fresh sample can fix a bad
    /// script or a flaky render; a refusal or missing key cannot.
    pub fn retryable(&self) -> bool {
        match self {
            VisualizeError::Chat(e) => e.retryable(),
            VisualizeError::Truncated => true,
            VisualizeError::BadScript(_) => true,
            VisualizeError::Render(_) => true,
        }
    }
}

/// Where a Visualize request has got to.
#[derive(Debug, Clone, PartialEq)]
pub enum VisualizePhase {
    /// `visualize_enabled` is off; the card asks for one-time consent.
    Consent,
    /// Sent, nothing back yet.
    Requesting,
    /// The model is thinking; no text yet.
    Thinking,
    /// JSON is arriving.
    Writing,
    /// The script passed the gate; manim is running.
    Rendering,
    /// The mp4 is on disk and playable.
    Ready,
    /// The model judged the fragment not worth animating. Terminal, benign.
    Unsuitable,
    Failed(VisualizeError),
}

/// The Visualize card's whole state. Owned by `JadeApp`; the pop-out window
/// reads the same value. The video player itself lives beside this in the
/// app — it is a platform object, and this struct stays pure.
#[derive(Debug, Clone)]
pub struct VisualizeCard {
    /// Bumped per request; deltas from an older generation are dropped.
    pub generation: u64,
    pub path: PathBuf,
    pub language: &'static str,
    /// 0-based, inclusive.
    pub start_row: usize,
    pub end_row: usize,
    pub sel_range: Range<usize>,
    /// The accumulating JSON response.
    pub json: String,
    /// Parsed once the response completes.
    pub plan: Option<ScenePlan>,
    /// The rendered mp4, once Ready.
    pub video: Option<PathBuf>,
    /// The last meaningful manim log line, shown while Rendering.
    pub render_line: String,
    pub phase: VisualizePhase,
    pub stale: bool,
    pub popped_out: bool,
    /// `now_ms()` when the card was opened; drives the loader and shimmer.
    pub started_ms: u64,
}

impl VisualizeCard {
    pub fn new(generation: u64, sel: &Selected<'_>, sel_range: Range<usize>) -> Self {
        VisualizeCard {
            generation,
            path: sel.path.to_path_buf(),
            language: crate::explain::language_name(sel.path),
            start_row: sel.start_row,
            end_row: sel.end_row,
            sel_range,
            json: String::new(),
            plan: None,
            video: None,
            render_line: String::new(),
            phase: VisualizePhase::Requesting,
            stale: false,
            popped_out: false,
            started_ms: 0,
        }
    }

    /// Stamp the animation clock. Separate from `new` so the reducer stays
    /// pure and the tests need no clock.
    pub fn started_at(mut self, now_ms: u64) -> Self {
        self.started_ms = now_ms;
        self
    }

    pub fn elapsed(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.started_ms)
    }

    /// Whether this card is parked on the consent question.
    pub fn awaiting_consent(&self) -> bool {
        self.phase == VisualizePhase::Consent
    }

    /// Apply one chat delta. Terminal phases absorb later deltas, exactly as
    /// the Explain reducer does — an aborted task can still have queued text
    /// behind its terminal event.
    pub fn apply_chat(&mut self, delta: ChatDelta) {
        if self.terminal() || self.phase == VisualizePhase::Rendering {
            return;
        }
        match delta {
            ChatDelta::Started(_) => {
                if self.phase == VisualizePhase::Requesting {
                    self.phase = VisualizePhase::Thinking;
                }
            }
            ChatDelta::Thinking => {
                if self.phase != VisualizePhase::Writing {
                    self.phase = VisualizePhase::Thinking;
                }
            }
            ChatDelta::Text(t) => {
                self.json.push_str(&t);
                self.phase = VisualizePhase::Writing;
            }
            ChatDelta::Done { stop_reason } => {
                // A truncated script cannot run: max_tokens is a failure
                // here, unlike in Explain.
                if stop_reason == StopReason::MaxTokens {
                    self.phase = VisualizePhase::Failed(VisualizeError::Truncated);
                    return;
                }
                match parse_scene_plan(&self.json) {
                    Err(e) => {
                        self.phase = VisualizePhase::Failed(VisualizeError::BadScript(e));
                    }
                    Ok(plan) if !plan.suitable => {
                        self.plan = Some(plan);
                        self.phase = VisualizePhase::Unsuitable;
                    }
                    Ok(plan) => {
                        // The gate before spawn. jade-build re-checks; this
                        // check is what the card reports.
                        match jade_build::manim::validate_scene_script(&plan.script) {
                            Err(e) => {
                                self.phase =
                                    VisualizePhase::Failed(VisualizeError::BadScript(e));
                            }
                            Ok(()) => {
                                self.plan = Some(plan);
                                self.phase = VisualizePhase::Rendering;
                            }
                        }
                    }
                }
            }
            ChatDelta::Failed(e) => {
                self.phase = VisualizePhase::Failed(VisualizeError::Chat(e));
            }
        }
    }

    /// Apply one render event.
    pub fn apply_render(&mut self, ev: jade_build::manim::RenderEvent) {
        use jade_build::manim::RenderEvent;
        if self.terminal() {
            return;
        }
        match ev {
            RenderEvent::Log(line) => {
                let t = clean_log_line(&line);
                if !t.is_empty() {
                    self.render_line = t;
                }
            }
            RenderEvent::Done { video } => {
                self.video = Some(video);
                self.phase = VisualizePhase::Ready;
            }
            RenderEvent::Failed { reason } => {
                self.phase = VisualizePhase::Failed(VisualizeError::Render(reason));
            }
        }
    }

    /// Whether the card reached an end state.
    pub fn terminal(&self) -> bool {
        matches!(
            self.phase,
            VisualizePhase::Ready | VisualizePhase::Unsuitable | VisualizePhase::Failed(_)
        )
    }

    /// Whether the card still has work in flight (so closing must cancel).
    pub fn in_flight(&self) -> bool {
        !self.terminal() && self.phase != VisualizePhase::Consent
    }

    /// Header label and whether it reads as an error.
    pub fn status(&self) -> (&'static str, bool) {
        match &self.phase {
            VisualizePhase::Consent => ("Consent", false),
            VisualizePhase::Requesting => ("Asking…", false),
            VisualizePhase::Thinking => ("Thinking…", false),
            VisualizePhase::Writing => ("Writing the scene…", false),
            VisualizePhase::Rendering => ("Rendering…", false),
            VisualizePhase::Ready => ("Ready", false),
            VisualizePhase::Unsuitable => ("Skipped", false),
            VisualizePhase::Failed(_) => ("Failed", true),
        }
    }

    pub fn failure(&self) -> Option<&VisualizeError> {
        match &self.phase {
            VisualizePhase::Failed(e) => Some(e),
            _ => None,
        }
    }

    pub fn can_retry(&self) -> bool {
        match &self.phase {
            VisualizePhase::Failed(e) => e.retryable(),
            _ => false,
        }
    }

    /// Same edit rules as the Explain card: an edit that touches or abuts the
    /// rows makes the clip possibly wrong; one strictly above only moves it.
    pub fn note_edit_rows(&mut self, first: usize, last: usize, removed: usize, added: usize) {
        if first <= self.end_row + 1 && self.start_row <= last + 1 {
            self.stale = true;
            return;
        }
        if last < self.start_row && removed != added {
            let shift = added as isize - removed as isize;
            self.start_row = self.start_row.saturating_add_signed(shift);
            self.end_row = self.end_row.saturating_add_signed(shift);
        }
    }
}

/// Strip rich's timestamp/level prefixes and the source location suffix from
/// a manim log line, leaving the sentence worth showing in the card.
fn clean_log_line(line: &str) -> String {
    let mut t = line.trim();
    // `[08/17/26 10:00:00] INFO  Animation 0: …  scene.py:123`
    for prefix in ["INFO", "WARNING", "ERROR", "DEBUG"] {
        if let Some(idx) = t.find(prefix) {
            // Only when the prefix sits in the timestamp region.
            if idx <= 24 {
                t = t[idx + prefix.len()..].trim_start();
                break;
            }
        }
    }
    // Drop a trailing `something.py:123` locator.
    let t = t.trim_end();
    if let Some(idx) = t.rfind(char::is_whitespace) {
        let tail = &t[idx + 1..];
        if tail.contains(".py:") {
            return t[..idx].trim_end().to_string();
        }
    }
    t.to_string()
}

// ── Playback formatting ──────────────────────────────────────────────────────

/// `mm:ss` for the transport label — `0:03`, `1:17`.
pub fn format_time(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sel<'a>(f: &'a dyn Fn(usize) -> Option<String>, text: &'a str) -> Selected<'a> {
        Selected {
            path: Path::new("/w/algo.cpp"),
            language: "cpp",
            start_row: 3,
            end_row: 5,
            text,
            line: f,
            symbols: &[],
            skeleton: "",
        }
    }

    fn no_lines(_: usize) -> Option<String> {
        None
    }

    fn card() -> VisualizeCard {
        let f = no_lines;
        VisualizeCard::new(1, &sel(&f, "for (auto& x : xs) sum += x;"), 40..90)
    }

    const GOOD_SCRIPT: &str = "from manim import *\n\nclass JadeScene(Scene):\n    def construct(self):\n        self.play(Write(Text(\"x\")))\n        self.wait(1)\n";

    fn plan_json(suitable: bool, script: &str) -> String {
        serde_json::json!({
            "suitable": suitable,
            "reason": "r",
            "title": "Summing a vector",
            "script": script,
            "duration_s": 8.0
        })
        .to_string()
    }

    // ── precheck ─────────────────────────────────────────────────────────

    #[test]
    fn data_languages_are_rejected_locally() {
        for lang in ["json", "toml", "markdown", "text"] {
            assert_eq!(precheck(lang, "a = 1"), Err(PrecheckReject::NotCode), "{lang}");
        }
    }

    #[test]
    fn comment_only_selections_are_rejected() {
        assert_eq!(
            precheck("cpp", "// a note\n// another\n"),
            Err(PrecheckReject::NoExecutableCode)
        );
        assert_eq!(
            precheck("python", "# just a comment\n"),
            Err(PrecheckReject::NoExecutableCode)
        );
        assert_eq!(precheck("cpp", "\n   \n}\n"), Err(PrecheckReject::NoExecutableCode));
    }

    #[test]
    fn includes_and_pragmas_alone_are_rejected() {
        assert_eq!(
            precheck("cpp", "#include <vector>\n#include <string>\n#pragma once\n"),
            Err(PrecheckReject::NoExecutableCode)
        );
    }

    #[test]
    fn real_code_passes_the_local_gate() {
        assert_eq!(precheck("cpp", "for (auto& x : xs) sum += x;"), Ok(()));
        assert_eq!(precheck("python", "def f(x):\n    return x * 2\n"), Ok(()));
        // One executable line among comments is enough.
        assert_eq!(precheck("cpp", "// setup\nint y = f(x);\n"), Ok(()));
    }

    // ── request shape ────────────────────────────────────────────────────

    #[test]
    fn the_request_is_schema_forced_medium_effort() {
        let f = no_lines;
        let r = visualize_request(&sel(&f, "x"));
        assert_eq!(r.max_tokens, 8000);
        assert_eq!(r.effort, Effort::Medium);
        let schema = r.json_schema.expect("visualize is a JSON response");
        assert_eq!(schema["additionalProperties"], false);
        let req: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for field in ["suitable", "reason", "title", "script", "duration_s"] {
            assert!(req.contains(&field), "{field} must be required");
        }
    }

    /// The gate has to be IN the prompt, or the schema field is noise.
    #[test]
    fn the_prompt_pins_the_contract() {
        for needle in [
            "from manim import *",
            "class JadeScene(Scene)",
            "suitable",
            "6 to 12 seconds",
            "Manim Community",
        ] {
            assert!(VISUALIZE_SYSTEM.contains(needle), "missing: {needle}");
        }
    }

    // ── response parsing ─────────────────────────────────────────────────

    #[test]
    fn a_wellformed_plan_parses() {
        let p = parse_scene_plan(&plan_json(true, GOOD_SCRIPT)).unwrap();
        assert!(p.suitable);
        assert_eq!(p.title, "Summing a vector");
        assert_eq!(p.duration_s, 8.0);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_scene_plan("not json").is_err());
        assert!(parse_scene_plan("").is_err());
    }

    // ── the reducer ──────────────────────────────────────────────────────

    #[test]
    fn the_happy_path_ends_in_rendering_then_ready() {
        let mut c = card();
        assert_eq!(c.phase, VisualizePhase::Requesting);
        c.apply_chat(ChatDelta::Started(jade_ai::ChatProviderId::Anthropic));
        assert_eq!(c.phase, VisualizePhase::Thinking);
        let body = plan_json(true, GOOD_SCRIPT);
        let (a, b) = body.split_at(body.len() / 2);
        c.apply_chat(ChatDelta::Text(a.into()));
        assert_eq!(c.phase, VisualizePhase::Writing);
        c.apply_chat(ChatDelta::Text(b.into()));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        assert_eq!(c.phase, VisualizePhase::Rendering);
        assert!(c.plan.is_some());
        assert!(c.in_flight());

        c.apply_render(jade_build::manim::RenderEvent::Log("INFO Animation 0: Write(Text) scene.py:12".into()));
        assert_eq!(c.render_line, "Animation 0: Write(Text)");
        c.apply_render(jade_build::manim::RenderEvent::Done { video: PathBuf::from("/v.mp4") });
        assert_eq!(c.phase, VisualizePhase::Ready);
        assert_eq!(c.video.as_deref(), Some(Path::new("/v.mp4")));
        assert!(!c.in_flight());
    }

    /// The model-side gate: suitable=false ends benignly, with the reason.
    #[test]
    fn an_unsuitable_verdict_is_terminal_and_not_an_error() {
        let mut c = card();
        c.apply_chat(ChatDelta::Text(plan_json(false, "")));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        assert_eq!(c.phase, VisualizePhase::Unsuitable);
        assert!(!c.can_retry(), "declining is a verdict, not a failure");
        assert_eq!(c.status(), ("Skipped", false));
    }

    /// StopReason::MaxTokens is a failure here — a truncated script cannot
    /// run — unlike in Explain, where a truncated answer still reads.
    #[test]
    fn max_tokens_is_a_failure() {
        let mut c = card();
        c.apply_chat(ChatDelta::Text(plan_json(true, GOOD_SCRIPT)));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::MaxTokens });
        assert_eq!(
            c.phase,
            VisualizePhase::Failed(VisualizeError::Truncated)
        );
        assert!(c.can_retry());
    }

    #[test]
    fn a_script_the_gate_refuses_fails_before_any_spawn() {
        let mut c = card();
        let evil = "from manim import *\nimport os\nclass JadeScene(Scene):\n    pass\n";
        c.apply_chat(ChatDelta::Text(plan_json(true, evil)));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        match &c.phase {
            VisualizePhase::Failed(VisualizeError::BadScript(_)) => {}
            other => panic!("{other:?}"),
        }
        assert!(c.can_retry(), "a fresh sample may pass the gate");
    }

    #[test]
    fn unreadable_json_fails_cleanly() {
        let mut c = card();
        c.apply_chat(ChatDelta::Text("{broken".into()));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        assert!(matches!(
            c.phase,
            VisualizePhase::Failed(VisualizeError::BadScript(_))
        ));
    }

    #[test]
    fn a_render_failure_carries_manims_reason() {
        let mut c = card();
        c.apply_chat(ChatDelta::Text(plan_json(true, GOOD_SCRIPT)));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        c.apply_render(jade_build::manim::RenderEvent::Failed {
            reason: "manim exited with code 1: NameError".into(),
        });
        match &c.phase {
            VisualizePhase::Failed(VisualizeError::Render(r)) => {
                assert!(r.contains("NameError"))
            }
            other => panic!("{other:?}"),
        }
        assert!(c.can_retry());
    }

    /// An aborted task can queue text behind its terminal delta.
    #[test]
    fn terminal_phases_absorb_later_deltas() {
        let mut c = card();
        c.apply_chat(ChatDelta::Failed(ChatError::Overloaded));
        c.apply_chat(ChatDelta::Text("LATE".into()));
        assert_eq!(c.json, "");
        assert!(matches!(c.phase, VisualizePhase::Failed(_)));

        let mut c = card();
        c.apply_chat(ChatDelta::Text(plan_json(false, "")));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        c.apply_render(jade_build::manim::RenderEvent::Done { video: "/x.mp4".into() });
        assert_eq!(c.phase, VisualizePhase::Unsuitable, "a late render event must not revive it");
    }

    /// Chat deltas that straggle in after the render started must not stomp
    /// the Rendering phase.
    #[test]
    fn rendering_ignores_late_chat_deltas() {
        let mut c = card();
        c.apply_chat(ChatDelta::Text(plan_json(true, GOOD_SCRIPT)));
        c.apply_chat(ChatDelta::Done { stop_reason: StopReason::EndTurn });
        assert_eq!(c.phase, VisualizePhase::Rendering);
        c.apply_chat(ChatDelta::Text("late".into()));
        c.apply_chat(ChatDelta::Failed(ChatError::Canceled));
        assert_eq!(c.phase, VisualizePhase::Rendering);
    }

    #[test]
    fn consent_is_not_in_flight() {
        let mut c = card();
        c.phase = VisualizePhase::Consent;
        assert!(!c.in_flight());
        assert!(c.awaiting_consent());
    }

    // ── staleness ────────────────────────────────────────────────────────

    #[test]
    fn edits_track_exactly_like_explain() {
        let mut c = card(); // rows 3..=5
        c.note_edit_rows(4, 4, 0, 0);
        assert!(c.stale);

        let mut c = card();
        c.note_edit_rows(0, 0, 0, 2);
        assert!(!c.stale);
        assert_eq!((c.start_row, c.end_row), (5, 7));
    }

    // ── formatting ───────────────────────────────────────────────────────

    #[test]
    fn times_format_as_minutes_and_seconds() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(3.4), "0:03");
        assert_eq!(format_time(71.9), "1:11");
        assert_eq!(format_time(-2.0), "0:00");
    }

    /// Headless end-to-end minus the window: a mock llama-server streams the
    /// plan over real HTTP/SSE, the real `ChatBackend` maps it, this module's
    /// reducer gates it, and `jade-build` renders it in the real sandbox.
    /// Ignored because the render takes real seconds and needs the venv:
    ///
    /// ```sh
    /// cargo test -p jade --bin jade e2e_mock_wire -- --ignored
    /// ```
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "runs a real sandboxed Manim render; needs the private venv"]
    async fn e2e_mock_wire_to_rendered_mp4() {
        use std::io::{Read, Write};

        // ── the mock server ──────────────────────────────────────────────
        let plan = plan_json(
            true,
            "from manim import *\n\nclass JadeScene(Scene):\n    def construct(self):\n        t = Text(\"e2e\")\n        self.play(Write(t))\n        self.wait(0.5)\n",
        );
        let chunks: Vec<String> = plan
            .as_bytes()
            .chunks(120)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Read until the end of the request headers + body enough.
            let mut buf = [0u8; 65536];
            let _ = s.read(&mut buf);
            let mut body = String::from("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
            for c in &chunks {
                let payload = serde_json::json!({
                    "choices": [{ "delta": { "content": c }, "finish_reason": null }]
                });
                body.push_str(&format!("data: {payload}\n\n"));
            }
            body.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
            body.push_str("data: [DONE]\n\n");
            let _ = s.write_all(body.as_bytes());
        });

        // ── the real chat backend, pointed at it ─────────────────────────
        let backend = jade_ai::ChatBackend::new();
        backend.set_runtime(tokio::runtime::Handle::current());
        backend.set_credential_for_test(None);
        backend.set_local_status(jade_ai::LocalStatus::Ready { endpoint });

        let f = |_: usize| None;
        let req = visualize_request(&sel(&f, "for (auto& x : xs) sum += x;"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        backend.start(jade_ai::Lane::Visualize, req, tx);

        let mut card = card();
        while let Some(delta) = rx.recv().await {
            let terminal = matches!(
                delta,
                ChatDelta::Done { .. } | ChatDelta::Failed(_)
            );
            card.apply_chat(delta);
            if terminal {
                break;
            }
        }
        assert_eq!(card.phase, VisualizePhase::Rendering, "json: {}", card.json);
        let script = card.plan.as_ref().unwrap().script.clone();

        // ── the real sandboxed render ────────────────────────────────────
        let cache = std::env::temp_dir().join(format!("jade-viz-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let (rtx, mut rrx) = tokio::sync::mpsc::unbounded_channel();
        let _h = jade_build::manim::render(
            jade_build::venv::venv_dir(),
            cache.clone(),
            script,
            rtx,
        );
        loop {
            match rrx.recv().await.expect("a terminal render event") {
                jade_build::manim::RenderEvent::Log(_) => continue,
                ev => {
                    card.apply_render(ev);
                    break;
                }
            }
        }
        assert_eq!(card.phase, VisualizePhase::Ready, "{:?}", card.phase);
        let video = card.video.clone().unwrap();
        assert!(video.exists());
        assert!(std::fs::metadata(&video).unwrap().len() > 1_000);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn log_lines_are_cleaned_for_the_card() {
        assert_eq!(
            clean_log_line("[08/17/26 10:00:00] INFO     Animation 0 : Write(Text)   scene.py:123"),
            "Animation 0 : Write(Text)"
        );
        assert_eq!(clean_log_line("   "), "");
        assert_eq!(clean_log_line("plain line"), "plain line");
    }
}
