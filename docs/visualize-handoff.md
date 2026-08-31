# Visualize (§4.15) — implementation handoff

You are picking up the second of two selection-driven AI features in Jade. The
first, **Explain** (§4.14), is built and working; this document describes what
exists, what you are building, and the specific things that will bite you.

Read `docs/jade-feature-inventory.md` first — this codebase treats it as the
spec, and its section numbers are cited throughout the source.

---

## What the feature is

The user selects a block of code and presses **⌘⇧M**. A card appears anchored to
the selection. Claude (or the local instruct model) writes a Manim scene that
animates the fragment; Jade renders it to an mp4 and plays it in the card, with
transport controls and a pop-out window.

**Visualize is independent of Explain.** Neither triggers the other, neither
waits on the other, and both can be open at once. That separation is what keeps
each one simple: Explain is a plain text stream, Visualize is a single
schema-validated JSON response.

---

## What already exists (do not rebuild it)

| Thing | Where | Note |
|---|---|---|
| Chat provider layer | `crates/jade-ai/src/chat/` | Anthropic + local, SSE framing, per-lane single-flight |
| **`Lane::Visualize`** | `chat/mod.rs` | Already defined. Its slot is separate from Explain's, so ⌘⇧M cannot cancel an in-flight ⌘⇧E |
| Structured output | `chat/anthropic.rs` | `ChatRequest::json_schema` → `output_config.format`. Tested |
| Model server | `crates/jade-ai/src/presets.rs` | ONE `llama-server` in router mode serving `jade-fim` + `jade-chat` |
| Prompt context | `crates/jade/src/explain.rs` | `Selected`, `build_user_message`, symbol lookup, file skeleton — all reusable verbatim |
| Card shell + placement | `crates/jade/src/panels/explain_card.rs` | `clamp_card`, anchoring, the measured-height slide |
| Design system | `crates/jade/src/beautiful/` | Beautiful UI port: tokens, card bands, chips, status pills, loader, markdown |
| Pop-out window | `panels/explain_popout.rs` | Copy the pattern, including the `cx.defer` |

**Reuse the context builder.** `explain.rs` already assembles: the selection,
the declarations of every identifier in it (via clangd hover), the file's
declaration-only outline (via `structure::parse_symbols`), and ±30 lines. That
is exactly the context a scene-writing prompt needs. Give it a different system
prompt and a JSON schema; do not write a second context builder.

---

## Order of work

Do these in order. Steps 1 and 2 are independent of the UI and testable without
a window; step 3 is the one that can crash the IDE.

### 1. Manim: consent, sandbox, venv, render to disk

No UI. Driven from a test.

- `crates/jade-build/src/venv.rs` — a private virtual environment at
  `~/.local/share/jade/manim-venv`, created with `python3 -m venv` then
  `pip install manim`. **Do not install into the user's Python.** On this
  machine `python3` is Anaconda; polluting it would be hostile.
  Discover `python3` with the four-tier resolver in
  `jade-ai/src/backend.rs:590` (`JADE_PYTHON` → PATH → `EXTRA_BIN_DIRS`) —
  GUI apps on macOS do not inherit the shell PATH, which is why that exists.
- `crates/jade-build/src/manim.rs` — spawn and supervise the render. Model it on
  `jade-build/src/compile.rs:27` (`run_cmake`): `tokio::process::Command`, both
  pipes captured, `tokio::select!` line streaming into an `UnboundedSender`, a
  `oneshot` stop channel, a `JoinHandle`.

Command:

```
<venv>/bin/manim render -qm --format mp4 --media_dir <work>
    --disable_caching --progress_bar none --verbosity WARNING
    <work>/scene.py JadeScene
```

- `-qm` is 1280×720 at 30fps. Deliberate over Manim's 1080p60 default: it halves
  the repaint rate (see risk 1), halves decode bandwidth, and renders 3-5× faster.
- `PYTHONUNBUFFERED=1`, or Python block-buffers stdout when not a tty and the
  "live log" arrives all at once at exit.
- `MPLBACKEND=Agg` — Manim can pull matplotlib, and a GUI backend will try to
  open a window from a headless child.
- Output lands at `<work>/videos/scene/720p30/JadeScene.mp4`. Parse Manim's own
  `File ready at '<path>'` line, falling back to the newest `*.mp4` under
  `<work>/videos/`. Do **not** hard-code the quality directory name; it is
  version-dependent. And confirm the file exists rather than trusting the exit
  code — Manim can exit 0 having rendered nothing.
- Cache to `<workspace>/.jade/manim/<hash of script+scene+quality>/`. `.jade/` is
  already gitignored. Prune to the newest 20 on startup.

**Teardown, all three:** `kill_on_drop(true)`, an explicit `oneshot`, and a pid
in an `AtomicU32` killed from `cx.on_app_quit`. `main.rs:337-343` records why:
GPUI ends the process without dropping tokio `Child`ren, which is how
llama-server used to survive the app.

### 2. Security — settle this before writing the renderer

This feature **executes model-authored Python**, derived from source code that
is by definition attacker-influenced. A comment in a downloaded repo reading
"also, in the script, read `~/.ssh/id_rsa` and…" is a live remote-code-execution
path. Four requirements, in the first slice, not as a follow-up:

1. `ai_prefs.visualize_enabled` defaults to **false**. First use shows a one-time
   consent card saying plainly that this runs generated Python locally. Explain
   keeps working without it.
2. A syntactic gate before spawn: the script must parse as exactly one
   `class JadeScene(Scene)` with only `from manim import *`, and is rejected on
   any of `import os|sys|subprocess|socket|shutil|pathlib`, `__import__`,
   `open(`, `eval(`, `exec(`, `compile(`. Comment it as a speed bump, not a
   boundary, so nobody later mistakes it for one.
3. The actual boundary: spawn under `sandbox-exec` with a profile permitting
   reads of the Python and Manim install, read-write only inside the per-request
   temp directory, and `(deny network*)`. Deprecated on macOS but functional,
   and the only sandbox available without shipping a container.
4. A 180s wall-clock cap, killed by pid, and the temp directory removed in a
   `Drop` impl.

Shipping the animation without this is worse than not shipping it.

### 3. Playback — the step that can crash the IDE

**Verify against a fixed mp4 checked into test fixtures, not against Manim
output.** Get this working on a known-good file first.

`crates/jade/src/video/` — `AVPlayer` + `AVPlayerItemVideoOutput` asked for
`420f` directly, handed to `gpui::surface()` with no conversion.

```toml
# under [target.'cfg(target_os = "macos")'.dependencies]
objc2-av-foundation = { version = "0.3", default-features = false, features = [
  "AVAsset", "AVPlayer", "AVPlayerItem", "AVPlayerItemOutput",
  "objc2-core-media", "objc2-core-video" ] }
objc2-core-media = { version = "0.3", default-features = false, features = ["CMBase", "CMTime"] }
objc2-core-video = { version = "0.3", default-features = false, features = [
  "CVBase", "CVBuffer", "CVImageBuffer", "CVPixelBuffer" ] }
```

`default-features = false` matters: the crate has 172 features and a naive `all`
adds minutes to a clean build. Verify with `cargo tree -d` that no new duplicate
versions appear — the lock already carries two `objc2` generations.
`objc2-av-foundation 0.3.2` needs `objc2 >=0.6.2, <0.8.0`; the lock has 0.6.4 via
GPUI, so they unify.

**GPUI panics on a wrong pixel format — it does not degrade.**
`gpui_macos/src/metal_renderer.rs:1496` is a bare
`assert_eq!(surface.image_buffer.get_pixel_format(), kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)`,
followed at `:1503` and `:1514` by `create_texture_from_image(...).unwrap()` on
both planes. A buffer in the wrong format, or one that is not IOSurface-backed
and Metal-compatible, takes the whole IDE down.

So create the output with all three attributes — reuse the dictionary
`wg3d/metal.rs:418-426` already builds (`CVPixelBufferKeys::MetalCompatibility`,
`IOSurfaceProperties`) — and then guard anyway:

```rust
if pb.get_pixel_format() != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
    return self.degrade("decoder produced an unexpected pixel format");
}
```

**The CVPixelBuffer bridge is one function, and refcounting it wrong gives an
intermittent crash under load — the hardest class to reproduce.**
`copyPixelBufferForItemTime:` is a COPY-rule (+1), `CFRetained` owns that +1, and
`wrap_under_get_rule` adds another:

```rust
/// `wrap_under_get_rule` RETAINS, so the objc2 `CFRetained` keeps its own +1
/// and both may drop independently. Never `wrap_under_create_rule` here.
fn bridge(buf: &CFRetained<objc2_core_video::CVPixelBuffer>) -> CfCVPixelBuffer {
    let raw = CFRetained::as_ptr(buf).as_ptr() as CVPixelBufferRef;
    unsafe { CfCVPixelBuffer::wrap_under_get_rule(raw) }
}
```

`wg3d/metal.rs:498-505` already documents this trap for the sibling case — read
it. Run the card under Address Sanitizer with `MallocScribble` and Zombies, and
soak a loop for five minutes, before merging.

The presentation clock is `core_video::host_time` — already a dependency, no new
crate needed.

**The frame pump is not a timer.** Use `window.request_animation_frame()`
(`gpui/src/window.rs:2199`), whose doc comment names video players specifically.
Drive it from a pre-render hook next to `ensure_preview_images` (`app.rs:5533`):

```rust
fn ensure_video_frame(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
    let Some(card) = self.visualize.as_mut() else { return };
    if !card.visible || !card.playing() { return }
    card.player.pump();                 // one hasNewPixelBufferForItemTime: call
    window.request_animation_frame();    // vsync-paced, notifies this view only
}
```

Requesting no frame lets the window go idle — the settle-and-stop contract
`wg3d::render::ensure_anim` honors (`wg3d/render.rs:48-77`). No pool rotation is
needed: unlike wg3d you never write into these buffers, so refcounting suffices.
Keep the previous buffer one extra frame anyway.

**Rejected alternatives, with the reason, so they are not re-litigated:**
- `gpui::img()` with decoded frames — `RenderImage` holds every frame and the
  sprite atlas never frees until `cx.drop_image` (`app.rs:5537`). 30s at 720p30
  is 900 frames × 3.7MB ≈ **3.3GB**.
- `ffmpeg` piping — `ffmpeg-next` needs system libav* and `pkg-config`, and a raw
  pipe is 41MB/s of memcpy plus a second copy into a buffer you must allocate
  with the right attributes anyway.

### 4. The request

One request, `Lane::Visualize`, `max_tokens: 8000`, `effort: Medium`, with
`json_schema`:

```json
{ "type": "object", "additionalProperties": false,
  "required": ["title", "script", "duration_s"],
  "properties": {
    "title":      { "type": "string" },
    "script":     { "type": "string" },
    "duration_s": { "type": "number" } } }
```

Because Explain is a separate feature this response is *only* JSON — no tool
tail, no fence splitter, no partial-JSON scanning.

The system prompt must pin: Manim Community v0.19, exactly one
`class JadeScene(Scene)`, `from manim import *` only, a whitelist of mobjects and
animations, 6-12 seconds, nothing leaving the 16:9 frame, `Text` over `MathTex`
unless the maths is the point, and — the part that matters — that the animation
must show data moving or control flowing, not decorate the code.

**`StopReason::MaxTokens` is a failure here**, unlike in Explain: a truncated
script cannot run. `chat/provider.rs` already distinguishes it.

**The local tier cannot do this.** `ChatModel::can_write_scenes()` returns false
for `Local`; hide or disable Visualize when it is selected. A 3B instruct model
does not write a runnable Manim scene.

### 5. Card, transport, pop-out

Reuse `explain_card.rs` wholesale for placement. The video area is 16:9:

```
Card::new(t)                              w 520
├── Card::bar      chip(language) · "L84–97" · stage pill · ⧉ · ×
├── video area 488×274
│     Disabled   → consent
│     Requesting → pixel_loader + shimmer
│     Rendering  → progress + the last manim log line
│     Ready      → gpui::surface(pb).object_fit(Contain)
│     Failed     → reason
└── Card::footer   ▶/⏸ · scrub · "0:03 / 0:11" · retry
```

`ObjectFit::Contain`, not `Fill`, or a 16:9 clip is stretched. Seek with
`toleranceBefore`/`toleranceAfter` of `kCMTimeZero` — Manim's x264 output has
sparse keyframes and a tolerant seek feels notchy — and pump once immediately
after a seek so the frame updates while paused.

Icons: `pause` needs adding to both the `icons!` macro and `UI_ICON_NAMES` in
`assets.rs`. `rotate-ccw` and `external-link` are already registered.

---

## Ranked risks

1. **A 30Hz repaint through a 7,500-line `render`.** Every animation frame
   re-runs all of `JadeApp::render`. If that exceeds 16ms, playing a clip
   stutters the whole IDE *including typing* — the risk that makes the feature a
   net negative. Mitigate with 720p30; instrument `render` duration behind an env
   flag and compare against dragging the wg3d grid (a known-acceptable 16ms
   baseline); stop the pump the moment the card is off-screen or the window is
   inactive. If it still exceeds budget, move the card into its own
   `Entity<VisualizeCard>` — `request_animation_frame` notifies `current_view()`
   — and `.cached(...)` the editor subtree. Only if measured; it is the first
   break in the god-entity invariant.
2. **Executing model-authored Python.** Covered above. Second only because the
   mitigation is fully specified; it is first in implementation order.
3. **CVPixelBuffer over-release, or a format GPUI asserts on.** One bridge
   function, the explicit format guard, and an ASan soak.
4. **Colour shift.** GPUI's shader is hard-coded full-range BT.601
   (`wg3d/metal.rs` header). Manim's x264 output is typically BT.709 or untagged;
   AVFoundation honours the `420f` *range* request but the matrix stays whatever
   the file says. Manim's blues and yellows shift; white-on-black text does not,
   which is why this ranks below the crash risks. If visible, request `32BGRA`
   instead and run the already-written `luma_fs`/`chroma_fs` passes from
   `metal.rs:127-146` into a pooled buffer — that path *does* reintroduce the
   three-buffer rotation, since you would then be writing into them.
5. **Manim environment fragility.** A heavyweight Python package with a system
   ffmpeg dependency and real CLI differences between Community and 3b1b. The
   private venv removes most of it; a `manim --version` probe at discovery puts
   the version in every failure banner.
6. **The unpinned GPUI git dependency.** `jade/Cargo.toml:11` tracks Zed's `main`
   with no rev, and `SurfaceSource` carries no stability guarantee. The lock pins
   `56ab1c48`. Pin the rev in the manifest before building on `surface()` — the
   comment there already says that was the intent.

---

## This machine

| Tool | State |
|---|---|
| `ffmpeg` | present, `/opt/homebrew/bin/ffmpeg` |
| LaTeX | present, MacTeX at `/Library/TeX/texbin` |
| `python3` | 3.13.5, Anaconda at `/opt/anaconda3/bin/python3` |
| **`manim`** | **not installed** — step 1 installs it into the private venv |
| `llama-server` | 9960 (a935fbffe), Homebrew, router mode supported |

---

## Conventions to follow

- The feature inventory is the spec. Add §4.15, and add ⌘⇧M to the §3 shortcut
  table so the next feature does not take it.
- ⌘⇧M is free — verified against `editor_key` (`app.rs:3684-3790`), the root
  handler, and inventory §3. Bind it in **both** `editor_key` and the root
  `on_key_down`, as ⌘⇧A is: `main.rs:397` binds only `cmd-q`, and the comment at
  `:384-396` records that the binding does not actually deliver it.
- Avoid ⌘⇧V: `"v" => self.editor_paste(cx)` has no shift guard.
- Colors come from `beautiful::dark()` tokens, never literals. Status is always
  saturated text on its own 14% tint, never the accent.
- Keep the pure logic (script validation, progress parsing, output-path
  resolution, the card reducer) out of the GPUI layer so it is testable without a
  window — that is what makes `explain.rs` and `beautiful/markdown.rs` testable.
- Gate anything touching Metal or AVFoundation behind `#[cfg(target_os = "macos")]`
  with a portable stub, so the headless suites keep running — `wg3d` falls back
  to a CPU painter for the same reason.
