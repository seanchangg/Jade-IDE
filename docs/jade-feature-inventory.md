# Jade — Complete Feature & UI Inventory

**Purpose:** this is the "don't lose anything" checklist for the Rust rewrite. Every
user-facing feature, UI behavior, protocol, constant, and quirk in the current
Electron/TypeScript app (as of 2026-07-13) is recorded here with `file:line`
references into this repo. The rewrite borrows Zed's performant engine code, but
Jade is **not** a Zed clone — the sections tagged `[IDENTITY]` are what make Jade
Jade and must survive verbatim; sections tagged `[COMMODITY]` are standard IDE
plumbing where the replacement (Zed crate or otherwise) only needs feature parity,
not behavioral cloning.

App identity: product name **Jade** (`app.setName('Jade')`, `src/main/main.ts:150`),
window title "Jade", app id `com.jade.ide` (`package.json`). Repo/env vars still use
the older "jade" prefix (`jade.*` preload API, `JADE_*` protocol, `.jade/` state
dir, `JADE_*` env vars for AI) — both prefixes are load-bearing.

---

## 1. Persistence & configuration `[IDENTITY — exact keys matter for migration]`

Three storage tiers:

### 1.1 Global config file
`app.getPath('userData')/jade-config.json` (`src/main/main.ts:15`), synchronous JSON
read/write, read errors swallowed to `{}` (`main.ts:17-32`).
Keys routed here — `GLOBAL_KEYS` (`main.ts:122`): `workspaceRoot`, `xpTotal`,
`theme`, `aiModel`, `aiMultiline`.
**Quirk to preserve/migrate:** `workspaceRoot` is stored under the property name
`lastWorkspace`, not `workspaceRoot` (asymmetric mapping, `main.ts:121-145`).

### 1.2 Per-workspace state
`<workspaceRoot>/.jade/workspace.json` (`src/main/workspace.ts:32-33`).
In-memory cache, flush debounced **2000ms**, pretty-printed JSON (`workspace.ts:151-195`).
The renderer bundles UI state into a single `'ui'` key. `PERSISTED_KEYS`
(`src/renderer/state.ts:96-101`), autosave debounced **1500ms** (`state.ts:130-139`):
`fileTreeVisible, terminalVisible, memoryBarVisible, fileTreeWidth, terminalHeight,
stickyNotes, openTabs, activeTabIndex, breakpoints, benchmarks, aiCompletionEnabled`.
Maps/Sets serialized to objects/arrays before save (`state.ts:107-115`). State is
loaded only after a workspace opens (`app.ts:830`).

### 1.3 localStorage (telemetry preferences only)
Only `telemetry-panel.ts` uses real localStorage:
- `jade.telemetry.enabled.<kind> <name>` — checkbox state (`telemetry-panel.ts:35`)
- `jade.telemetry.shape.<kind> <name>` — user shape hint, format `"RxC"` (`:36`)
- `jade.telemetry.maxdim.<kind> <name>` — streaming resolution cap (`:37`)

XP total is saved separately via `jade.state.save('xpTotal', …)`, debounced
**2000ms** (`src/renderer/editor/xp-bar.ts:106-113`).

---

## 2. Window, layout & chrome `[IDENTITY — the look]`

- Window: 1400×900, min 800×500, `titleBarStyle: 'hiddenInset'`, traffic lights at
  (12,12), background `#1E1F22`, shown only on `ready-to-show` (`main.ts:41-66`).
  `contextIsolation: true`, `nodeIntegration: false`, `sandbox: false` (node-pty).
- `nativeTheme.themeSource = 'dark'` forced (`main.ts:153`); light mode is a
  renderer-level toggle (`body.light-mode`), not an OS theme follow.
- **No custom application menu** — Electron default; all custom shortcuts are
  registered in the renderer (see §3).
- **Welcome overlay** when no workspace open (`app.ts:54-81`): "Jade" title,
  "Open a folder to get started", Open Folder button, shortcut hint row
  (`⌘B File tree · ⌘\` Terminal · ⌘E Flow arrows · ⌘S Save`).
- **Action bar** (38px, hidden until workspace opens): left padding 80px to clear
  traffic lights, `-webkit-app-region: drag` with `no-drag` on controls
  (`main.css:222-243`). Groups: [sidebar / terminal / flow / runtime toggles] |
  [diagnostics badges] | [flag preset select, custom flags input, ASM, Build, Run,
  Debug, Stop, AI menu, theme toggle, home button].
- **Main layout**: floating-card look — sidebar / editor / runtime panels each with
  rounded corners, border, subtle shadow, 6px gaps (`main.css:457-463, 2342-2344`).
  - Left sidebar (`#sidebar-area`): Files|Structure tab switcher; width animates
    28px (collapsed strip with rotated "FILES" label, click reopens) ⇄ 260px
    (`app.ts:449-459`, `main.css:481-495`). Binary toggle, not drag-resizable.
  - Right runtime sidebar (`#runtime-area`): 0 ⇄ 280px, hosts RuntimePanel +
    TrainingView (+ telemetry sidebar).
  - Bottom terminal area: drag-resizable via 4px handle, clamped 150px–70vh,
    live-writes `terminalHeight` (`terminal-panel.ts:335-353`). Debug panel docks
    *above* the terminal in the same area (see §5.8).
  - Bottommost memory bar strip: 0 ⇄ 28px.
- **Dead persisted field:** `fileTreeWidth` is stored but never read — sidebar width
  is hardcoded (`state.ts` vs `app.ts:449`). Default `terminalHeight` inconsistency:
  280 in `state.ts` vs 200 fallback in `terminal-panel.ts`.

---

## 3. Keyboard shortcuts `[IDENTITY]`

Registered as Monaco commands (`app.ts:609-656`, `editor-manager.ts:106-131`):

| Shortcut | Action |
|---|---|
| ⌘B | Toggle file tree |
| ⌘\` | Toggle terminal |
| ⌘E | Toggle execution-flow arrows |
| ⌘⇧B | Build active file |
| ⌘R | Run last build |
| ⌘. | Stop build/debug/run |
| ⌘⇧A | Toggle ASM viewer |
| ⌘⇧D | Debug active file |
| F5 / F10 / F11 / ⇧F11 | Debug continue / step over / into / out |
| ⌘P | Quick Open |
| ⌘S | Save active file |
| ⌘W | Close active tab |
| ⌘⇧E | Explain the selection (§4.14) |
| ⌘⇧M | Visualize the selection (§4.15) |
| ⌥→/⌥← (+⇧) | Punctuation-aware word nav / select |

Contextual: Quick Open (Esc/↑/↓/Enter, `app.ts:963-983`); inline rename/create and
benchmark-name inputs (Enter commits, Esc cancels); Esc closes the 3D weight grid.

---

## 4. Editor layer

### 4.1 Tabs & editing `[COMMODITY — parity via Zed editor]`
`src/renderer/editor/editor-manager.ts`. Open dedupes by path; Monaco model reuse;
per-tab viewState save/restore on switch; dirty dot `●`; middle-click closes tab;
drag-to-reorder tabs (`:296-336`); close adjusts active index; save = full-file
write via `jade.workspace.writeFile`; LSP full-document sync on change
(`notifyLspChange`, `:377-389` — incremental sync was an acknowledged TODO).
`.metal` files are **not** sent to clangd (`:177-180`).
Tab restore on workspace open skips deleted files silently (`:272-280`).

Monaco options worth preserving as defaults (`:49-104`): JetBrains Mono 13px /
lineHeight 20, ligatures on, **minimap off**, 6px scrollbars without shadows,
smooth caret + smooth scrolling, padding top/bottom 16, glyph margin on,
bracket-pair colorization **off**, indent guides on with active highlight,
tabSize 4 spaces, wordWrap off, whitespace hidden.

Language detection by extension (`:399-412`): cpp/cc/cxx/c++→cpp, c→c,
h/hpp/hxx/cu→cpp, metal→metal, mm/m→objective-c, plus json/md/py/sh/js/ts.

### 4.2 Themes `[IDENTITY — the exact palettes]`
`src/renderer/editor/theme.ts`. Two full themes, `inherit: false`:
- **jade-dark**: "JetBrains New UI charcoal" — editor `#1E1F22`, panels `#2B2D30`.
  Accent system: emerald `#56B389` (keywords, cursor, selection tints, gutter-added),
  periwinkle `#8DB2FF` (functions/info/modified), blue-gray `#9BB5CF` (types),
  amber `#D4A76A` (strings/numbers/warnings), red `#CF6B6B` (errors/deleted),
  muted green-gray `#6B7A72` comments. ASM: registers red, function labels bold
  emerald. Full token/editor color tables at `theme.ts:12-178`.
- **jade-light**: research-rationale documented in-code (`theme.ts:184-197`) —
  warm cream `#F4EFE2` bg (not white), warm charcoal `#373528` text (~10:1, not
  21:1), desaturated green/amber accents, borders at black 8% alpha. Comments
  italic. Tables at `theme.ts:198-324`.

### 4.3 Custom languages `[IDENTITY]`
- **Metal** (`metal-language.ts`): full language configuration (comments, brackets,
  auto-close incl. `/*`→`*/`, surround pairs incl. `<>`, indent rules, doc-comment
  continuation with `* `). Tokenizer covers C++ keywords + Metal keywords
  (`kernel, vertex, fragment, device, constant, threadgroup, …`), Metal
  vector/matrix/texture/encoder types, and a dedicated `@attribute` state for
  `[[...]]` attributes → `annotation` token (amber).
- **ASM** (`asm-language.ts`): tokenizer only (no language config). Semantic color
  scheme: x86-64 **and** ARM64 register sets → red; function labels → bold teal;
  local/branch labels dimmed as comments; branch/jump ops → keyword; data movement
  and arithmetic → blue; SIMD/vector ops → green; nop/fence/syscall → dimmed;
  hex/immediates as numbers. Comments: `;`, `@`, `//`.
- Rust-rewrite note: reproduce these scopes with tree-sitter grammars + theme
  mappings; the ASM *semantic grouping* (SIMD green, registers red) is the part to
  keep, not the Monarch mechanics.

### 4.4 LSP providers `[COMMODITY]`
`lsp-providers.ts`: completion (trigger chars `. : > < " /`), hover, definition,
references, push diagnostics — all gated on `lspReady`, registered for `'cpp'`
**only** (a `'c'` selector is defined but never used — C files silently lack
providers; known gap `lsp-providers.ts:10-13`). Diagnostics for `.metal` files are
explicitly cleared (`:130-133`). No signature-help/code-action/format/rename.

### 4.5 Memory decorations `[IDENTITY — flagship feature]`
`memory-decorations.ts` (611 lines). Three cooperating systems:
1. **Static size annotations** — parses the buffer to annotate struct/class
   declarations `[N B]`, local variables (only non-primitives or primitives >8B, to
   avoid clutter, `:461-466`), and `new` expressions `[heap N]` / `[heap N×c=total]`.
   Size model: `PRIMITIVE_SIZES` LP64/Apple-Silicon table (`:40-56`), `STL_SIZES`
   approximations (`std::string=24, vector=24, map=48, unordered_map=56, mutex=64,
   …`, `:59-84`), recursive computation for `array/pair/optional/atomic`, struct
   size estimation with alignment `min(size,8)` (`:420-437`), user struct parsing at
   brace-depth 1 (`:287-341`).
2. **Execution count annotations** — from `executedLines`; only count>1; format
   `×123 / ×1.5K / ×2.3M`; diff arrows `↑/↓` vs the previous run's snapshot
   (`:474-495`).
3. **Runtime allocation decorations** — per `file:line` tracker fed by alloc/free
   events (O(1) pointer index, `:29-30`); inline text `← N calls, X.YKB[, M leaked]`
   with leak-red styling (`:568-605`).

Annotation mechanics to preserve: one merged decoration per line; generation-based
CSS invalidation (stylesheet rebuilt per scan so stale decorations go invisible,
`:10-13, 175-188`); annotation text rendered via `::after` in emerald at 11px/0.7
opacity. Debounces: content-change rescan **1000ms**, scan coalescing **50ms**.
Memory-bar store writes coalesced to once per animation frame (`:545-556`).
Event types handled: `alloc/free`, `heap-summary`, `asan-stats`,
`asan-leak-summary`, `asan-leak-location` (`:86-155`).

### 4.6 Debug decorations `[COMMODITY with quirks]`
`debug-decorations.ts`: breakpoints per file in store key `breakpoints`
(persisted); toggled by clicking glyph margin or line-number gutter; live-synced to
LLDB when a session is active; paused-line styling + glyph arrow + "Execution
paused here" hover; `setCurrentDebugLine` reveals line centered and moves cursor.

### 4.7 Structure tints `[IDENTITY]`
`structure-decorations.ts`: whole-block background tint per **top-level multi-line
symbol**, cycling 5 low-alpha colors (green/blue/amber/red/blue-gray at 0.04–0.05
alpha) so adjacent functions/classes are visually separable without fighting
syntax highlighting. Includes overview-ruler marker.

### 4.8 Execution flow `[IDENTITY — flagship feature]`
`execution-flow.ts` (694 lines). Toggled by ⌘E. Static regex/brace-count analysis
(no AST) finds functions (incl. class-method resolution by variable type,
`:478-694`), then decorates:
- Glyph margin: `●` sequential (emerald), `→` call (periwinkle), `↩` return
  (amber), `↺` loop-back (periwinkle), `⑂` branch (amber), `⊘` error (red).
- Whole-line tints matching each kind (alpha 0.04–0.12, colored left border).
- After a run: executed lines tinted green, non-executed glyphs dimmed to 0.25
  opacity so only the real path pops; error line from `errorLine` store key.
- **Hover connectors**: hovering a glyph draws an SVG cubic-bezier curve from that
  line to its call/return/loop counterpart with endpoint dots, and highlights the
  target line (`:158-277`).
- **Cmd+Click on a glyph navigates**: call→definition, return→call site,
  function-entry→caller (`:279-319`).
- Re-analysis debounced **300ms** on content change.

### 4.9 Sticky notes `[IDENTITY]`
`sticky-notes.ts`: click a **line number** to create a note at the mouse position
(one per file+line). Draggable by header, resizable (min 160×100), default
220×160. contentEditable with live `[ ]`/`[x]` → `☐`/`☑` checkbox conversion;
clicking a checkbox toggles it. Content saves debounced **500ms** as markdown-ish
text; position/size persist on mouseup. Notes are absolute-positioned and
**deliberately do not scroll with the editor** (`:277-280`); only notes for the
active file are shown (others `display:none`). Persisted in the workspace `ui`
blob.

### 4.10 XP bar `[IDENTITY — gamification]`
`xp-bar.ts`: mounted in the tab-bar right slot. A content change earns credit only
if it inserts a newline, is ≤300 chars (anti-paste), and the completed line ends
with `;` (comments stripped). XP = 1 × streak per credit; streak increments while
qualifying edits arrive within **10s** of each other, else resets to 1. Level N→N+1
costs `150 + (N-1)*100` XP. UI: `L{n}` label (tooltip: progress/needed/total),
fill track, `×{streak}` badge shown when streak>1. Persist key `xpTotal` (global).

### 4.11 AI inline completion (renderer) `[IDENTITY — behavior tuning]`
`editor/inline-completion.ts`: languages cpp/c/metal/objective-c/python/js/ts/shell.
Debounce **120ms** with cancellation both sides of the request; prefix cap 6000
chars, suffix cap 2000; single-line mode default (multiline = 6 lines max);
48-entry cache with **typed-through hits** (typing through a suggestion serves the
remainder instantly); cache cleared on multiline toggle. Post-processing: truncate
at first blank line, strip trailing duplicate of text after cursor, suppress empty.
Ghost text via Monaco inlineSuggest (Tab accept / Esc dismiss),
`enableForwardStability: true`. Design follows JetBrains FLCC paper (arXiv
2405.08704, cited at `:7-11`).

### 4.12 Frequency completion `[IDENTITY — small but distinctive]`
`frequency-completion.ts`: scans the whole buffer for identifiers; suggests words
appearing ≥2 times, length ≥3, case-insensitive prefix match, ranked by frequency
(sortText `10000-count`), shown with detail `×{count}`. Runs alongside LSP
completion.

### 4.14 Explain (⌘⇧E) `[IDENTITY — Rust-era feature, no Electron ancestor]`
Select code, press ⌘⇧E: a card anchored to the selection streams a prose
explanation. Context: the selection, clangd hover declarations for its
identifiers (≤12, `SYMBOL_BUDGET`-bounded), a declaration-only file outline,
±30 surrounding lines (`explain.rs`). Providers: Anthropic (streaming Messages
API, `Lane::Explain`) with fallback to the managed llama-server instruct
preset. Card: `beautiful` design system, measured-height slide placement
(`clamp_card`), stale marking on edits, pop-out window, Esc closes. The bind
lives in BOTH `editor_key` and the root `on_key_down`.

### 4.15 Visualize (⌘⇧M) `[IDENTITY — Rust-era feature, no Electron ancestor]`
Select code, press ⌘⇧M: the model writes a Manim Community scene as one
schema-validated JSON response (`suitable`, `reason`, `title`, `script`,
`duration_s`); Jade renders it to an mp4 and plays it in the card with
transport controls (▶/⏸, zero-tolerance scrub, times, replay) and a pop-out.
Independent of Explain: own lane, own card; both can be open at once.

Gating, in order, so an unnecessary visualizer never runs on inappropriate
code: (1) `visualize::precheck` locally rejects data languages (json/toml/
markdown/text) and selections with no executable code — free, no request;
(2) the prompt + required `suitable` schema field make the model decline
fragments with no data movement or control flow to show (the card ends as
"Skipped" with the reason); (3) `manim::validate_scene_script` rejects any
script that is not exactly one `class JadeScene(Scene)` with only
`from manim import *` (a speed bump, not a boundary).

Security: `ai_prefs.visualize_enabled` defaults to false — first ⌘⇧M shows a
one-time consent card (Enter accepts, Esc declines). The render runs under
`sandbox-exec`: no network, writes only inside the per-request work dir,
`/Users` reads denied except the venv and work dir (probed by
`jade-build/tests/manim_sandbox.rs`). 180s wall cap; pid killed from
`on_app_quit` and the signal handler. Manim lives in a private venv at
`~/.local/share/jade/manim-venv` — never the user's Python. Renders cache in
`<workspace>/.jade/manim/<hash>/`, pruned to 20. `StopReason::MaxTokens` is a
failure (a truncated script cannot run); `ChatModel::Local` is refused.
Playback: AVPlayer + AVPlayerItemVideoOutput decoding 420f IOSurface-backed
CVPixelBuffers into `gpui::surface()`, pumped once per
`request_animation_frame` only while playing and on-screen
(`ensure_video_frame`); wrong-format frames degrade to a banner instead of
tripping GPUI's format assert.

---

## 5. Panels

### 5.1 File tree `[COMMODITY]`
`panels/file-tree.ts`. Header: FILES + new-file/new-folder/collapse-all/minimize.
Lazy-loading tree (initial depth 3, then 1 per expand — `workspace.ts:71-120`);
dirs first, locale-sorted. Extension glyph map (`:504-522`): `◆ ◇ ◈ {} ≡ ¶ ◎ $ ⚙ ▣ ·`;
source files accent-colored, headers accent2. Click file opens; active file
highlighted. FS-watch refresh debounced **250ms** (coalesces build artifact
bursts). Context menu: New File/Folder (dirs), Rename, Delete (native
`window.confirm` — the app's only modal). Inline rename pre-selects name minus
extension. Drag & drop move (file→its parent dir targeting, drop on empty area →
root, self-move guards). **No git status indicators.**
Ignore lists (`workspace.ts:6-15`): node_modules, .git/.svn/.hg, build dirs incl.
`cmake-build-jade`, caches, IDE dirs; extensions `.o .obj .a .lib .so .dylib .exe
.out .class .pyc`; dotfiles hidden except `.jade`; `.dSYM` hidden; root-level
extensionless files hidden (compiled-binary heuristic).

### 5.2 Terminal `[COMMODITY — Zed terminal covers this]`
`panels/terminal-panel.ts`: xterm + fit + web-links addons over node-pty
(`$SHELL` or `/bin/zsh`, xterm-256color, truecolor). Multi-instance with a
toggleable list (`≡`): rows named `zsh <id>`, duplicate (spawns new, doesn't clone)
and delete actions. Hardcoded dark/light ANSI palettes swapped on theme change
(canvas can't read CSS vars, `:13-46`). `writeOutput` targets instance 0 for
`[jade]` status lines. PTY exit writes dim `[exited <code>]`. PTY creation
returning `-1` = node-pty unavailable, degrade gracefully (`pty-manager.ts:93-124`).

### 5.3 Memory bar `[IDENTITY]`
`panels/memory-bar.ts`: bottom strip. Left: SYS MEM, HEAP, PEAK, PRESSURE (5-dot
gauge). Right: CPU%, GPU% (`—` when unavailable/-1). Thresholds: heap warn >80% /
danger >95% of peak; peak forced danger + `(N leaked)` when leaks; pressure warn
>60 / danger >80; CPU/GPU warn >60 / danger >85. **Fallback:** before any program
data exists, shows OS-level memory from systemStats, then switches to program data
(`:119-135`). Gauges smoothing: EMA weight 0.3, GPU -1 propagated as unavailable
(`gauges/system-gauges.ts`).

### 5.4 Runtime panel `[IDENTITY]`
`panels/runtime-panel.ts`: right sidebar; auto-shown on first run and first
training event. Sections: **SPEED** (live-ticking duration at 100ms while running,
personal best with accent when beaten, vs-last delta colored), **MEMORY**
(heap/peak/allocs/frees/leaks, leaks red; skips DOM writes while hidden),
**HOTSPOTS** (top 10 executed lines as bars, click-to-jump; empty state "Build +
Run with flow on"), **BENCHMARKS** (named snapshots via ⚑ on history rows, name
input prefilled `#<run> <flags>`, fastest gets accent, delta tag vs latest run,
deletable, sorted fastest-first, persisted), **HISTORY** (last 10 runs).

### 5.5 Structure panel `[IDENTITY — replaceable internals]`
`panels/structure-panel.ts`: regex-based C++ outline (namespace/class/struct/enum/
functions/methods/members with access specifiers) — mermaid-style tree with
connector lines and kind-colored dots; click navigates. Refresh debounced
**800ms**. In the rewrite, tree-sitter replaces the regex parser; keep the visual
treatment and the tint hookup (§4.7).

### 5.6 Telemetry sidebar `[IDENTITY — protocol UI]`
`panels/telemetry-panel.ts`: registry + UI. Sections SCALARS / TIMERS / BUFFERS;
rows = checkbox + name + live value + (buffers) shape button. **First 3 scalars
auto-checked** unless the user has a stored preference (`AUTO_CHECK_SCALARS = 3`,
`:40, 182-190`). Value formats: buffers `R×C @step`; timers ms→s at ≥1000; scalars
4-decimal, exponential outside [1e-3, 1e6). Inline shape editor (rows×cols,
Enter/Esc, singleton popover). Placeholder→real-name rename migrates all three
localStorage prefs (`:115-160`). Value updates coalesced per animation frame;
structural rebuilds keyed by `"<kind> <name>"` so checkboxes survive. Decodes
base64 tensors **once** here and publishes `tensorFrame` to the store.

### 5.7 Quick Open `[COMMODITY]`
`app.ts:863-1002`: ⌘P; substring filename match, case-insensitive, 10 results max,
relative-path hint, cached file list per workspace.

### 5.8 Debug panel `[COMMODITY layout, custom docking rule]`
`panels/debug-panel.ts`: CLion-style, 240px, docks above the terminal and
**temporarily hides it, restoring prior visibility on hide** (`:101-120`). Header:
status text (running muted / paused amber bold / exited italic) + Continue, Step
Over/Into/Out, Stop buttons. Columns: FRAMES (240px, click navigates, frame 0
active) | VARIABLES (expandable tree, expansion state survives steps via path set,
lazy child fetch) | CONSOLE (ANSI-stripped, autoscroll).

### 5.9 Dead code
`panels/memory-overlay.ts` is unmounted dead code superseded by RuntimePanel
(confirmed: no import sites; CSS block `main.css:1585-1719` also unused). Do not
port. Also dead: store keys `workspaceReady`, `debugPausedLine` (init-only),
`fileTreeWidth` (written, never read).

---

## 6. Action-bar features (beyond toggles)

- **Diagnostics badges**: errors/warnings/info counts polled from Monaco markers
  every **2000ms** + on model change; click opens a dropdown listing all markers
  sorted by severity, click-to-jump (`app.ts:676-739`).
- **Flag presets** (`app.ts:95-101, 510-517`): Metal
  (`-x objective-c++ -Imetal-cpp -framework Metal -framework Foundation -framework
  QuartzCore`), OpenMP (`-fopenmp`), pthreads (`-pthread`), Accelerate
  (`-framework Accelerate`). Choosing copies into the custom-flags input and resets
  the select to placeholder.
- **ASM viewer** (⌘⇧A, `app.ts:304-445`): right-half overlay Monaco editor showing
  Intel-syntax `-O3 -march=native` assembly of the active file with **bidirectional
  line cross-highlighting** (cursor in either side highlights the counterpart;
  asm→src also scrolls). Auto-refreshes 1.5s after source edits while visible
  (saves first).
- **Build/Run/Debug lifecycle**: buttons swap labels (`Building…`/`Running…`),
  spin the icon, disable during build; terminal is force-shown and receives
  ANSI-colored `[jade]`/`[cmake]` status lines (green success / red failure / cyan
  progress). Build errors become red Monaco markers (owner `jade-build`) and jump
  to first error. Run wires `executedLines` into the store, records the run in
  RuntimePanel, auto-shows the runtime sidebar on first run, parses `file:line:col`
  from failing sanitizer output into `errorLine`. Debug builds with forced `-O0`.
- **AI menu**: enable checkbox (disable **kills the model server to free
  GPU/unified memory** — deliberate, `app.ts:290-291`), multiline checkbox, model
  select (Fast 1.5B / Balanced 3B ~3.3GB / Best 7B ~8GB; disabled with tooltip when
  on an external server), live status footer. Button states: active / pulsing
  `ai-starting` / dimmed `ai-off`.
- **Theme toggle**: swaps Monaco theme + `body.light-mode` + publishes `themeMode`
  (consumed by xterm palettes and canvas charts), persists.
- **Home**: stops everything, clears `workspaceRoot` persistence, returns to
  welcome overlay.
- **Status/notification philosophy**: there is **no toast system** — status lives
  in the terminal, button states, badges, and the debug status line.

---

## 7. Training / ML visualization `[IDENTITY — the flagship differentiator]`

### 7.1 Training view (`renderer/ml/training-view.ts`)
Right-sidebar "TRAINING" view. Sections: Loss, Memory, Kernel time (auto-hidden
until plottable), Timing breakdown, Tensors (auto-hidden until a buffer enabled;
header has a "3D" button).
- Buffers: scalars/memory capped at **1000 points**; tensor ring buffer **32
  frames per buffer**; timing events capped 5000 total but eviction preserves the
  **last 500 per timer name** (`:233-244`).
- **Ghost previous-run overlay**: on clear, current run's data is snapshotted and
  re-drawn at 25% alpha (`+'40'` hex) under the new run — loss, memory, and kernel
  charts all do this (`:74-77, 311-319, 437-439`).
- Charts are canvases (252×120), redrawn via dirty-flag + single rAF, **no render
  while hidden**; colors resolved at draw time and refreshed on theme change. No
  hover/tooltips. 5-color series palette cycling; per-series min/max scaling
  combining current+previous run; index-based X axis; labels
  `name: value` stacked, color = series color.
- Memory chart: prefers a scalar whose name contains `memory`/`heap`, else live
  heap samples; area fill at 10% alpha; `formatBytes` B/KB/MB.
- Kernel-time chart: **shared Y scale across all timers** (they share a unit —
  deliberate, `:533-614`).
- Timing breakdown: top 8 timers by total ms, horizontal bars, average shown (s if
  ≥1000ms), pooled row DOM (no innerHTML rebuild).
- Tensor previews: 236px canvases, `image-rendering: pixelated`,
  nearest-neighbor upscale from an exact rows×cols offscreen canvas; height
  preserves **source** aspect ratio clamped [40,180]px; label
  `name R×C @step [min…max]` with compact formatting; click → 3D view.
- **Diverging colormap** (shared 2D/3D convention): normalized by frame max-abs,
  t≥0 white→red `(255, 255(1-t), 255(1-t))`, t<0 white→blue (`:792-804`).
- A `WeightGrid3D` is created **eagerly** (hidden) so its ring buffer fills from
  the first frame (`:79-83`).

### 7.2 3D weight grid (`renderer/ml/weight-grid-3d.ts`, raw WebGL2)
Full-window overlay (z-index 4000, `-webkit-app-region: no-drag` required —
`weight-grid.css:8-30`). One instanced bar per cell:
- Height ∝ |value|, `HEIGHT_SCALE=12`, floor `MIN_BAR=0.02`; **negative values
  extend below the plane** with corrected lighting normals; same diverging
  colormap. `MAX_CELLS = 256×256` with sqrt-stride subsampling. Ring buffer
  **64 frames** per buffer (independent of training view's 32), fills while hidden.
- Scene: 0.9×0.9-footprint unit boxes; reference grid at y=0 (aspect-correct,
  center line `#444455`, lines `#2a2a34`, 0.35 alpha); vertical value axis with
  `0/+max/−max` labels; 4 tick labels per edge showing **source tensor indices**;
  billboard text labels (JetBrains Mono 30px on 256×64 canvases) drawn last,
  depth-test off; Lambert lighting: ambient 0.65 + two directional (0.9, 0.3).
- Camera: `OrbitCamera` — yaw π/4, pitch 0.6, dist 120, fovY 50, pitch clamp 1.53,
  damping 0.08, dist clamp [0.5, 3500]. Left-drag orbit, right-drag/Shift-drag pan,
  wheel dolly `exp(deltaY·0.0015)`. Initial framing
  `dist = max(rows,cols,4)·1.3 + 24`, offset `(0.55, 0.7, 0.85)·dist`.
- Toolbar: "3D WEIGHTS", buffer select, rows×cols shape-override inputs (sent to
  probe via registry `setShape`; Esc-stopPropagation while typing), streaming
  resolution select (≤64/≤128/≤256), **training-step scrubber** (dragging off the
  newest frame freezes live mode; back to newest resumes; buffer switch resets to
  live), readout `name dims step N [min…max] [i/count] • live`, × close (also Esc).
- Perf contract: render loop runs only while visible and (dirty or damping);
  DPR capped at 2; frame update = one bufferSubData + one drawElementsInstanced;
  full GL teardown on destroy incl. `loseContext()`.

### 7.3 Rewrite guidance
The 2D/3D colormap convention, ring-buffer sizes, live/freeze scrubber semantics,
ghost-run overlays, and idle-stopping render loops are the product; WebGL2/Monaco
mechanics are not. In Rust: GPUI-painted charts, Metal/wgpu instanced bars.

---

## 8. Main process subsystems

### 8.1 Build & run pipeline `[IDENTITY — preserve semantics exactly]`
`src/main/build-runner.ts` (924 lines).
- Build dir: `cmake-build-jade` (separate from CLion's). Configure cached per
  buildDir keyed by `\x1f`-joined configure args + `CMakeCache.txt` existence;
  cache invalidated on configure failure (`:15, 418-511`).
- Configure args: Debug build type, `CMAKE_EXPORT_COMPILE_COMMANDS=ON`,
  `JADE_INCLUDE_DIR`, CXX/OBJCXX/linker flags. Base cxxFlags `-g` + user flags;
  sanitize adds `-fsanitize=address -fno-omit-frame-pointer`; instrument adds
  `-fprofile-arcs -ftest-coverage` / `--coverage`. Build: `cmake --build --parallel`.
- **CMake File API** used to find the built executable: empty query file
  `codemodel-v2`, reply parsed, prefers the executable target whose sources include
  the active file (`:382-416`).
- **CMakeLists auto-generation** when none found within 8 parent levels (stops at
  $HOME or /): picks main.cpp/mm/cc/m near a .metal active file or first source
  alphabetically; detects ObjC and Metal shaders; generates C++17 project with
  Metal/Foundation/MPS/QuartzCore linking and a `.metal → .air → default.metallib`
  xcrun custom-command chain; emits cyan `[jade]` notice (`:261-361`).
- Error parsing: `file:line:col: (error|warning|note)` + CMake configure error
  regexes (`:50-79`).
- **Run** (`:561-793`): single concurrent run (previous killed);
  `JADE_TELEMETRY_SOCK` exported from the telemetry server; ASan env
  `ASAN_OPTIONS=detect_leaks=0:print_stats=1`; **dylib injection rules**: malloc
  interposer only when NOT sanitizing, Metal probe always attempted
  (`DYLD_INSERT_LIBRARIES`, colon-joined). Stdout/stderr line-buffered with partial
  trailing lines flushed on close.
- Coverage (instrument mode): find `.gcda` (depth ≤6), run `gcov` on the **first**
  one only (10s timeout), parse `count: line` from `.gcov`, clean up `.gcov` and
  processed `.gcda` (keep `.gcno`), emit cyan coverage summary.
- ASan output parsing: leak summary, per-leak `file:line` locations, error types
  echoed red `[ASan] <type>`; two stats formats (`:146-218`).
- **ASM generation** (`:809-903`): `clang++ -std=c++17 -O3 -march=native -g -S
  -masm=intel` to stdout; `.loc`-directive mapping builds asm→source line map;
  debug labels/directives filtered; demangled via `c++filt` (5s timeout, graceful
  fallback).
- Dylib compile-on-demand: probe `clang++ -dynamiclib -fobjc-arc -O2 -framework
  Metal -framework Foundation` → `/tmp/jade_probe.dylib` (30s timeout); interposer
  `clang -shared -ldl` → `/tmp/jade_interpose.dylib` (15s); recompiled on source
  mtime; silent failure = feature off (`:29-48, 536-559`).
- **Known perf issue** (docs/perf-findings.md): alloc/free events are sent one IPC
  message each — the Rust rewrite eliminates this by construction; if patching the
  Electron app first, batch flushes every 50–100ms or 500 events.

### 8.2 `__JADE_*` wire protocols `[IDENTITY — byte-for-byte]`
Pipe-delimited magic lines from instrumented programs (`build-runner.ts:83-143,
640-677`). **stdout**: `__JADE_ALLOC|ptr|size|file|line|ts`, `__JADE_FREE|…`,
`__JADE_SCALAR|name|step|value|ts`, `__JADE_TIMING|name|ms|step` (scalar/timing
routed through the telemetry registry so legacy programs appear in the sidebar).
**stderr**: `__JADE_TRACE|…|line|…`, `__JADE_FUNC_ENTER|addr|…`,
`__JADE_FUNC_EXIT|…`, `__JADE_HEAP_SUMMARY|totalAlloc|totalFreed|currentHeap|
peakHeap|allocCount|freeCount`, `__JADE_INTERPOSE_ACTIVE`. Unrecognized
`__JADE_*` lines are swallowed, not printed. Two stray `console.log`s on the
heap-summary path (`:176, :187`) should not be ported.

### 8.3 Telemetry server `[IDENTITY — spec in docs/telemetry-protocol.md]`
`telemetry-server.ts`: Unix socket `os.tmpdir()/jade-telemetry-<pid>.sock`,
NDJSON both ways, stale socket file deleted on start, 8MB partial-line guard,
malformed lines silently skipped, multiple concurrent clients, `track` replay to
new clients. Registry defaults `enabled:false, maxDim:128`; tensor frames for
disabled buffers dropped defensively; late-arriving meta merged + decl re-emitted.
**Buffer aliasing** (`:239-319`): probe names like `Matrix::Matrix #3` are
symbolicated via `atos` (8s timeout) against the exe + parent dirs, source line
regex-matched for `lhs =` or `Type name(...)` to recover the variable name;
collisions get outer-scope prefixes then `#2` suffixes; **track messages sent back
to the probe use the probe's own name** (reverse alias lookup, `:337-354`).
Only subsystem that guards against double IPC registration on macOS `activate`
re-init (`:449-477`) — the other subsystems have a latent double-registration bug
(see §10).

### 8.4 Debug driver `[COMMODITY if Zed DAP replaces it; record semantics anyway]`
`debug-driver.ts`: interactive `lldb` CLI wrapper. Custom prompt `JADE_LLDB> `;
auto-confirm on; stop-line context suppressed. Injects `JADE_TELEMETRY_SOCK` +
probe dylib via `target.env-vars` (**no malloc interposer while debugging**).
Output framing: prompt match or **150ms** quiet-settle for command replies; **80ms**
quiet window for unsolicited stops; command queue serialization; **10s** command
timeout; stop() = `kill`+`quit` then SIGKILL after 1s. Stop handling: parses
`stop reason`, location `at file:line`, then `frame variable -T` + `bt`
sequentially. Variable parser handles aggregates/containers/pointers, builds
dotted paths (`m.inner.q`, `vec[0]`, `*m.buf`), marks non-null pointers expandable;
lazy children via `frame variable -T -P 1`. Backtrace regex
``frame #N: … `func at file:line``.

### 8.5 LSP client `[COMMODITY — Zed native clangd]`
`lsp-client.ts`: spawns `clangd --background-index --clang-tidy
--completion-style=detailed --header-insertion=iwyu --pch-storage=memory`;
`fallbackFlags: ['-std=c++17', '-I<include dir>']`; language ids by extension
(`.mm`→objective-cpp, `.cu`→cuda, default cpp). Custom addition worth keeping:
`lsp:memory-info` extracts `sizeof = (\d+)` from clangd hover text (`:244-296`).

### 8.6 System monitor `[COMMODITY]`
`system-monitor.ts`: 1500ms poll; CPU% from os.cpus() idle/total deltas; mem from
os.totalmem/freemem; GPU via `systeminformation` sampled every **4th tick** (~6s)
with cached value between, `-1` = unavailable, module load failure tolerated.

### 8.7 AI backend `[IDENTITY — keep contract; Rust port is a thin client]`
`inline-completion.ts` (main): llama.cpp `llama-server` over HTTP `/infill` +
`/health`. Endpoint resolution: `JADE_FIM_ENDPOINT` → `127.0.0.1:8012`
(llama.vscode convention) → spawn managed on **8630**. Binary discovery:
`JADE_LLAMA_SERVER` env → PATH → `/opt/homebrew/bin, /usr/local/bin,
/opt/local/bin` (GUI apps lack shell PATH). Models: Qwen2.5-Coder 1.5B/3B/7B
Q8_0 GGUF via `-hf`. Spawn args: `-ngl 99 -ub 1024 -b 1024 --ctx-size 0
--cache-reuse 256`. Request: n_predict 64 (single-line, stop `\n`) / 96, top_k 40,
top_p 0.99, temp 0.1, `cache_prompt: true`, `t_max_predict_ms: 1500`.
Timeouts: health 800ms, request 4000ms, startup deadline 15min (first-run model
download), health poll 1500ms. In-flight request aborted on supersede
(single-slot protection). Model switch on an external endpoint is a no-op. Stop
kills the server to free unified memory; restart reloads (no re-download).
Any server honoring `/health` + `/infill {content}` can replace it via
`JADE_FIM_ENDPOINT` — this is the custom-model escape hatch.

### 8.8 Workspace/FS `[COMMODITY]`
`workspace.ts`: `fs.watch` recursive (macOS-only OK today; Linux would need a
different watcher); rename events resolved to create/delete via existsSync; CRUD
incl. recursive dir delete; ignore lists as §5.1.

### 8.9 Lifecycle
`window-all-closed`: shutdown LSP, PTYs, system monitor, AI backend, telemetry,
then quit — **no macOS keep-alive**. `will-quit` re-shuts telemetry (idempotent).
macOS `activate` recreates window + re-runs `initializeServices()` — latent
double-`ipcMain.handle` registration bug for all subsystems except telemetry
(`main.ts:73-176`); fix rather than preserve, noted so nobody copies it.

---

## 9. Native components (carry over unchanged)

- `probe/jade_probe.mm` → `/tmp/jade_probe.dylib`: injected Metal probe streaming
  GPU tensors/scalars/timings over the telemetry socket.
- `include/jade_interpose.c` → `/tmp/jade_interpose.dylib`: malloc interposer
  emitting `__JADE_ALLOC/FREE/HEAP_SUMMARY`.
- `include/jade_trace.c/.cpp`: `-finstrument-functions` tracing
  (`__JADE_TRACE/FUNC_ENTER/FUNC_EXIT`).
- `include/idetools.h`: user-facing instrumentation macros.
- `probe/mock_server.py`, `probe/test_train.mm`: protocol test clients — use these
  to verify the Rust telemetry server end-to-end.
- `docs/telemetry-protocol.md` is the **authoritative wire spec** (decl/scalar/
  timing/tensor/track messages, replay rules, legacy stdout back-compat).
- Fixed dylib paths in `/tmp` are shared across app instances/users — known wart;
  the Rust rewrite may move them under the app cache dir *if* it also updates the
  compile-on-demand paths consistently.

---

## 10. Constants quick-reference

| What | Value | Where |
|---|---|---|
| Window | 1400×900 min 800×500, bg `#1E1F22` | main.ts:41 |
| Workspace state flush | 2000ms debounce | workspace.ts:168 |
| Renderer `ui` autosave | 1500ms debounce | state.ts:130 |
| File-tree watch refresh | 250ms debounce | file-tree.ts:135 |
| Structure panel refresh | 800ms debounce | structure-panel.ts |
| Flow re-analysis | 300ms debounce | execution-flow.ts:142 |
| Memory annotation rescan | 1000ms + 50ms coalesce | memory-decorations.ts:133,167 |
| Sticky note content save | 500ms debounce | sticky-notes.ts:153 |
| XP persist / streak window | 2000ms / 10s | xp-bar.ts |
| AI debounce / prefix / suffix / cache | 120ms / 6000 / 2000 / 48 entries | editor/inline-completion.ts |
| AI health / request / startup / poll | 800ms / 4s / 15min / 1.5s | main/inline-completion.ts |
| Managed llama port / default endpoint | 8630 / 127.0.0.1:8012 | main/inline-completion.ts:13-14 |
| Diagnostics badge poll | 2000ms | app.ts:676 |
| Run-duration live tick | 100ms | runtime-panel.ts |
| System monitor poll / GPU subsample | 1500ms / every 4th tick | system-monitor.ts |
| LLDB settle / async / cmd timeout / kill fallback | 150ms / 80ms / 10s / 1s | debug-driver.ts |
| Telemetry socket buffer guard | 8MB | telemetry-server.ts:146 |
| atos symbolication timeout | 8s | telemetry-server.ts:239 |
| Scalar/memory points / TV tensor ring / 3D ring | 1000 / 32 / 64 | training-view.ts:34-35, weight-grid-3d.ts:31 |
| Timing cap | 5000 total, keep last 500/name | training-view.ts:233 |
| 3D max cells / height scale / DPR cap | 256×256 / 12 / 2 | weight-grid-3d.ts |
| Orbit camera | yaw π/4, pitch 0.6, dist 120, fov 50, damping 0.08 | weight-grid-gl.ts:111-118 |
| Terminal height clamp | 150px–70vh | terminal-panel.ts:335 |
| Sticky note default / min | 220×160 / 160×100 | sticky-notes.ts |
| XP level curve | 150 + (N−1)·100 | xp-bar.ts:14 |
| Telemetry auto-check | first 3 scalars | telemetry-panel.ts:40 |
| Default track maxDim | 128 | telemetry-server.ts / protocol doc |

---

## 11. Known bugs / dead code / deliberate non-features (do NOT blindly port)

- IPC firehose: per-allocation `BUILD_MEMORY_EVENT` messages (perf-findings.md #1)
  — fixed by architecture in Rust.
- Debug `console.log`s in `parseHeapSummary` (perf-findings.md #2).
- `memory-overlay.ts` + its CSS: dead, superseded by RuntimePanel.
- Dead store keys: `workspaceReady`, `debugPausedLine`, `fileTreeWidth`.
- macOS `activate` double-registration latent bug (§8.9).
- LSP providers registered for `cpp` only; `c` files lack completion/hover.
- gcov coverage only processes the first `.gcda` found.
- Preload exposes no listener cleanup (listeners accumulate; harmless in practice).
- Deliberate non-features (don't add "for parity"): no git integration, no toast
  system, no minimap, no bracket-pair colorization, sticky notes don't scroll with
  code, dark-only OS theme.
