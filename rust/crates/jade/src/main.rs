//! Jade — GPUI app shell for the C++/Metal IDE with live GPU training
//! visualization (Rust rewrite of the Electron/TypeScript app).
//!
//! This binary wires the telemetry server + the four engine crates
//! (`jade-build` / `jade-debug` / `jade-sysmon` / `jade-ai`) into the GPUI
//! window shell (`app::JadeApp`). Every async source is bridged onto one unified
//! [`app::AppEvent`] channel (see `app.rs`), so the pump coalesces bursts in one
//! place.
//!
//! Usage:
//!   cargo run -p jade                          # bare window
//!   cargo run -p jade -- --file main.cpp       # with an active file
//!   cargo run -p jade -- --project dir/        # first .cpp/.cc/.mm in dir
//!   cargo run -p jade -- --train ../probe      # launch the probe demo program
//!   cargo run -p jade -- --project dir --smoke build-run   # headless verify
//!   cargo run -p jade -- --project dir --smoke term        # headless PTY check
//!
//! Repo root (for the engine's native-source dirs) is resolved at runtime from
//! `JADE_REPO_ROOT`, falling back to `CARGO_MANIFEST_DIR/../../..` (the
//! jade-ide checkout the binary was built from).
//!
//! Module map:
//!   theme       — jade-dark/jade-light palettes (§4.2)
//!   format      — value formatting ported from the TS renderer
//!   prefs       — telemetry preference persistence
//!   registry    — discovered-item model + auto-check rule (§5.6)
//!   training    — capped data buffers / tensor rings / ghost snapshots (§7.1)
//!   output      — output-panel scrollback + ANSI strip (§6)
//!   memory_bar  — memory-bar model + threshold classification (§5.3)
//!   app         — window layout + unified event pump + action handlers (§2, §6)
//!   panels      — file_tree / code_view / training_view / telemetry_sidebar,
//!                 plus Phase-4 wave 2: terminal_panel (§5.2) + runtime_panel (§5.4)

// Phase-2 shell helpers (deferred pref setters, some TrainingData helpers) are
// deliberately present ahead of full wiring.
#![allow(dead_code)]

mod ai_prefs;
mod app;
mod asm;
mod assets;
mod beautiful;
mod benchmark;
mod debug;
mod decorations;
mod editor_view;
mod explain;
mod find;
mod fonts;
mod format;
mod frequency;
mod ghost;
mod highlight;
mod hw;
mod kumo;
#[cfg(test)]
mod interaction_tests;
mod memory_bar;
mod output;
mod panels;
mod panes;
mod prefs;
mod quick_open;
mod registry;
mod run_store;
mod structure;
mod sync;
mod theme;
mod timer_groups;
mod training;
mod video;
mod visualize;
mod wave;
mod wg3d;
mod workspace_state;
mod workspace_tree;
mod xp;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jade_ai::{ChatBackend, InlineCompletionBackend};
use jade_build::{BuildEngine, EngineConfig};
use jade_sysmon::SystemMonitor;
use jade_telemetry::{Event, TelemetryServer};
use jade_term::TermManager;
use gpui::{
    point, px, size, App, AppContext, Bounds, KeyBinding, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions,
};
use gpui_platform::application;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use app::{AppDeps, AppEvent, AppMode, JadeApp};
use workspace_tree::{is_watch_relevant, WatchDebounce};

/// Match the Electron app's crisp text (`-webkit-font-smoothing: antialiased`,
/// main.css:145): set this app's `AppleFontSmoothing` default to 0 so gpui's
/// mac text system skips CoreGraphics' glyph dilation (stem darkening). Uses
/// the app-scoped preference — the same `defaults write <app> …` people apply
/// to VS Code — and respects an explicit value the user already set. Runs
/// before any text rasterizes because gpui caches the lookup in a OnceLock.
#[cfg(target_os = "macos")]
fn disable_font_smoothing() {
    use core_foundation::base::TCFType;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_foundation_sys::preferences::{
        kCFPreferencesCurrentApplication, CFPreferencesAppSynchronize,
        CFPreferencesCopyAppValue, CFPreferencesSetAppValue,
    };

    let key = CFString::new("AppleFontSmoothing");
    unsafe {
        // Respect an existing explicit choice (e.g. user re-enabled smoothing).
        let existing =
            CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), kCFPreferencesCurrentApplication);
        if !existing.is_null() {
            core_foundation::base::CFRelease(existing as _);
            return;
        }
        let zero = CFNumber::from(0i64);
        CFPreferencesSetAppValue(
            key.as_concrete_TypeRef(),
            zero.as_concrete_TypeRef() as _,
            kCFPreferencesCurrentApplication,
        );
        CFPreferencesAppSynchronize(kCFPreferencesCurrentApplication);
    }
}

#[cfg(not(target_os = "macos"))]
fn disable_font_smoothing() {}

// Native menu-bar actions (File → Open Folder…, Jade → Quit). Dispatched by
// the OS menu; handlers are registered in `main`'s run closure.
gpui::actions!(jade, [OpenFolder, Quit]);

/// GUI apps launched from Finder/Dock inherit launchd's minimal PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`) — no `/opt/homebrew/bin` — so `cmake`,
/// `clangd`, and `llama-server` fail to resolve and every Build/Run/Debug
/// action dies with "command not found". Merge the user's login-shell PATH
/// once at startup (the fix-path trick the Electron app relied on); if the
/// shell probe fails, fall back to appending the standard tool dirs.
/// Terminal launches already carry a rich PATH and return early.
fn fix_gui_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    if current
        .split(':')
        .any(|d| d == "/opt/homebrew/bin" || d == "/usr/local/bin")
    {
        return;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let probed = std::process::Command::new(&shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    let merged = match probed {
        Some(p) => format!("{p}:{current}"),
        None => format!("{current}:/opt/homebrew/bin:/usr/local/bin"),
    };
    std::env::set_var("PATH", merged);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Must run before any engine/runtime spawns so every child process
    // (cmake, clangd, lldb, llama-server) inherits the repaired PATH.
    fix_gui_path();
    // Must run before the first glyph is rasterized (gpui caches the check).
    disable_font_smoothing();

    // Telemetry server on a dedicated tokio runtime; the Handle is kept so
    // button handlers (and the smoke hook) can spawn engine futures.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let socket = TelemetryServer::default_socket_path();
    let (server, tel_events) = {
        let _guard = runtime.enter();
        TelemetryServer::start(socket.clone()).expect("bind telemetry socket")
    };
    let server = Arc::new(server);
    // Buffer-name aliasing works from app start (TS parity: the server always
    // resolved allocation sites). The probe's decl meta carries the exe path,
    // so the empty fallback only matters for probes that omit it; Run/Debug
    // re-install with the freshly built executable.
    server.set_symbolicator(Arc::new(jade_build::AtosSymbolicator::new(
        std::path::PathBuf::new(),
    )));

    // ── Engine lifecycle (deliverable §2) ─────────────────────────────────────
    let root = repo_root();
    let engine = Arc::new(BuildEngine::new(
        EngineConfig::from_repo_root(&root).with_telemetry(server.clone()),
    ));
    let sysmon = Arc::new(SystemMonitor::new());
    let ai = Arc::new(InlineCompletionBackend::new()); // constructed, NOT started
    // Chat backend for the Explain / Visualize selection features (§4.14,
    // §4.15). Nothing is resolved here: the credential is looked up lazily on
    // the first request, so startup never touches the keychain.
    let chat = Arc::new(ChatBackend::new());
    {
        let _guard = runtime.enter();
        sysmon.start(); // SystemMonitor runs from app start
    }

    // ── Terminal engine (§5.2) ────────────────────────────────────────────────
    let term = Arc::new(TermManager::new());
    let term_events = term.take_events();

    // ── Unified event channel + source forwarders (deliverable §1) ────────────
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    spawn_forwarders(&handle, app_tx.clone(), tel_events, &sysmon, &ai);

    // Forward terminal events (Damaged/Exited) onto the unified pump.
    if let Some(mut trx) = term_events {
        let tx = app_tx.clone();
        handle.spawn(async move {
            while let Some(ev) = trx.recv().await {
                if tx.send(AppEvent::Term(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let mut active_file = resolve_active_file(&args);
    let workspace_root = resolve_workspace_root(&args, active_file.as_deref(), &root);
    let demo = args.iter().any(|a| a == "--train");
    // Hardware mode (§B1): `--hardware` wins, else the folder's persisted
    // `ui.mode`, else software.
    let mode = resolve_mode(&args, &workspace_root);
    // A hardware project seeds the editor with its top Verilog file — the
    // C++-only pick above finds nothing in a Verilog folder.
    if active_file.is_none() && mode == AppMode::Hardware {
        active_file = hw::first_hw_file(&workspace_root);
    }
    // "No workspace open" (inventory §2): neither --project nor --file AND no
    // restored tabs → the welcome overlay covers the editor and the tree does
    // NOT scan the fallback repo root.
    let workspace_opened = resolve_workspace_opened(&args, &workspace_root);

    // ── FS-watch: debounced tree refresh (§5.1) ───────────────────────────────
    // The watcher is now owned by `JadeApp` (created in `assemble`, replaced by
    // `open_project`) via this re-spawn closure, so opening a new folder can
    // restart the watch on the new root. It captures the tokio Handle + the
    // unified sender; the debounce semantics live in `spawn_fs_watch`.
    let fs_watch: app::FsWatchSpawn = {
        let handle = handle.clone();
        let app_tx = app_tx.clone();
        Arc::new(move |root: &Path| {
            spawn_fs_watch(root, &handle, app_tx.clone()).map(|w| Box::new(w) as Box<dyn Send>)
        })
    };

    println!("JADE_TELEMETRY_SOCK={}", socket.display());
    println!("repo root: {}", root.display());
    if let Some(f) = &active_file {
        println!("active file: {}", f.display());
    }

    // Kept out of `deps` so the quit and signal handlers below can take the
    // managed llama-server down; `deps` moves into the window.
    let ai_cleanup = ai.clone();

    // A second sender for the hardware source watch below (`app_tx` itself
    // moves into `deps`).
    let app_tx2 = app_tx.clone();
    let deps = AppDeps {
        server: server.clone(),
        engine,
        ai,
        chat,
        sysmon,
        term: term.clone(),
        runtime: handle.clone(),
        app_tx,
        active_file,
        repo_root: root,
        workspace_root,
        workspace_opened,
        fs_watch,
        demo,
        prefs_path: None, // real ~/.config/jade prefs
        mode,
        // The GUI has no test channel: the app spawns the real engine session
        // when hardware mode starts.
        hw_tx: None,
        hw_watch: {
            let handle = handle.clone();
            let app_tx2 = app_tx2.clone();
            Arc::new(move |root: &Path| {
                spawn_hw_watch(root, &handle, app_tx2.clone())
                    .map(|w| Box::new(w) as Box<dyn Send>)
            })
        },
    };

    // ── Headless smoke hook (deliverable §7) ──────────────────────────────────
    if let Some(mode) = arg_value(&args, "--smoke") {
        if mode == "open" {
            // `--smoke open <file>`: drive the real tab/highlight path headlessly.
            let target = arg_value(&args, "open").map(PathBuf::from);
            run_smoke_open(deps, target);
        } else if mode == "term" {
            run_smoke_term(runtime, deps);
        } else if mode == "wg3d" {
            run_smoke_wg3d(deps);
        } else if mode == "decorations" {
            let target = arg_value(&args, "decorations")
                .map(PathBuf::from)
                .or_else(|| deps.active_file.clone());
            run_smoke_decorations(target);
        } else if mode == "structure" {
            let target = arg_value(&args, "structure").map(PathBuf::from);
            run_smoke_structure(deps, target);
        } else if mode == "quickopen" {
            let query = arg_value(&args, "quickopen").unwrap_or_default();
            run_smoke_quickopen(deps, &query);
        } else if mode == "edit" {
            let target = arg_value(&args, "edit")
                .map(PathBuf::from)
                .or_else(|| deps.active_file.clone());
            run_smoke_edit(deps, target);
        } else if mode == "ghost" {
            run_smoke_ghost();
        } else if mode == "lsp" {
            let target = arg_value(&args, "lsp").map(PathBuf::from);
            run_smoke_lsp(runtime, deps, target);
        } else if mode == "sighelp" {
            run_smoke_sighelp(runtime, deps, app_rx);
        } else if mode == "hw" {
            run_smoke_hw(runtime, deps);
        } else {
            run_smoke(runtime, deps, app_rx, &mode);
        }
        return;
    }

    // Keep the runtime alive on a background thread (GUI path). The fs-watcher is
    // now owned by `JadeApp` (created in `assemble` via the `fs_watch` closure),
    // so it is not held here.
    std::thread::spawn(move || runtime.block_on(std::future::pending::<()>()));

    // Optionally launch the probe's test training program against ourselves.
    maybe_launch_train(&args, &socket);

    // ⌃C / `kill` must take the managed llama-server with us. `on_app_quit`
    // below covers ⌘Q and the menu, but a `cargo run` in a terminal dies by
    // signal, and the server would then hold the GPU until the next reboot.
    spawn_signal_cleanup(&handle, ai_cleanup.clone());

    // Register the bundled-icon asset source (lucide SVGs) so `gpui::svg()` can
    // resolve `icons/<name>.svg`; `with_assets` also rebuilds the SvgRenderer over
    // it. Chained onto the platform Application before the first window opens; the
    // font registration below is unaffected (it runs against the same `App`).
    application().with_assets(assets::Assets).run(move |cx: &mut App| {
        fonts::register_bundled_fonts(cx);

        // Quitting Jade must stop the model server. GPUI ends the process
        // without dropping the tokio `Child`, so `kill_on_drop` never fires and
        // llama-server kept running (and kept the GPU allocated) long after the
        // window closed. `kill_managed_now` is synchronous on purpose — there is
        // no runtime to await here, and quit does not wait for us.
        cx.on_app_quit(move |_cx| {
            ai_cleanup.kill_managed_now();
            // The same hole for the Visualize render: GPUI ends the process
            // without dropping the tokio Child, so the sandboxed manim would
            // outlive the app. Kill it by pid, synchronously.
            jade_build::manim::kill_active_render();
            async {}
        })
        .detach();

        // Signature window chrome (§2 "the look" / main.ts:41-66): 1400×900 window,
        // `hiddenInset`-equivalent titlebar (transparent, traffic lights at 12,12),
        // 800×500 minimum. The #1E1F22 backdrop is painted by the root div's
        // `bg(theme.bg)`, which now fills the titlebar inset too (appears_transparent).
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Jade".into()),
                        appears_transparent: true,
                        // Centered on the 38px action bar: the lights are 12px
                        // tall, so 13px down puts their middle at 19px.
                        traffic_light_position: Some(point(px(12.), px(13.))),
                    }),
                    window_min_size: Some(size(px(800.), px(500.))),
                    window_background: WindowBackgroundAppearance::Opaque,
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| JadeApp::new(cx, deps, app_rx)),
            )
            .unwrap();

        // Native macOS menu bar: File → Open Folder… routes to the same
        // directory picker as ⌘O / the action-bar button (CLion-style project
        // opening); each opened folder lands in the project subtabs.
        cx.on_action(move |_: &OpenFolder, cx| {
            // The menu action is dispatched *inside* the active window's own
            // update (`Window::dispatch_action` → `cx.defer` → `window.update`),
            // so calling `window.update` again here re-enters the same window —
            // `update_window_id` has already `take()`n it out of its slot, so the
            // nested update returns `Err` and the folder picker never opens (the
            // action-bar folder button works because it's already in the entity
            // context). Defer our update so it runs after the dispatch unwinds and
            // the window slot is restored.
            cx.defer(move |cx| {
                let _ = window.update(cx, |app, _window, cx| app.prompt_open_project(cx));
            });
        });
        cx.on_action(|_: &Quit, cx| cx.quit());

        // gpui builds the macOS menu itself and takes each item's ⌘-shortcut
        // from the keymap binding registered for that item's action
        // (gpui_macos platform.rs: `bindings_for_action` → `initWithTitle_
        // action_keyEquivalent_`). Jade registered no bindings at all, so "Quit
        // Jade" carried no shortcut.
        //
        // CAVEAT: with this binding in place, the item STILL reports no key
        // equivalent over the accessibility API (AXMenuItemCmdChar is missing).
        // So the menu label does not come from here today. ⌘Q itself is carried
        // by the root `on_key_down` in app.rs, alongside ⌘P and ⌘O. The binding
        // stays because it is the mechanism that is supposed to label the item,
        // and it costs one line — but do not read it as the reason ⌘Q works.
        //
        // Only Quit is bound. ⌘O already works through that same root handler,
        // and a binding for it could fire the action AND the handler, opening
        // two folder pickers.
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);

        cx.set_menus(vec![
            gpui::Menu::new("Jade").items(vec![gpui::MenuItem::action("Quit Jade", Quit)]),
            gpui::Menu::new("File").items(vec![gpui::MenuItem::action(
                "Open Folder…",
                OpenFolder,
            )]),
        ]);

        cx.activate(true);
    });
}

/// Kill the managed llama-server when Jade is signalled, then exit.
///
/// GPUI's `on_app_quit` handles ⌘Q, but it never runs for ⌃C or `kill`, and a
/// llama-server that outlives Jade holds its GPU memory and answers on the
/// managed port forever. SIGKILL of Jade itself cannot be caught — the PID file
/// covers that case, by letting the next run adopt the orphan.
fn spawn_signal_cleanup(handle: &Handle, ai: Arc<InlineCompletionBackend>) {
    handle.spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut hup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let code = tokio::select! {
            _ = tokio::signal::ctrl_c() => 130, // 128 + SIGINT
            _ = term.recv() => 143,             // 128 + SIGTERM
            _ = hup.recv() => 129,              // 128 + SIGHUP
        };
        ai.kill_managed_now();
        jade_build::manim::kill_active_render();
        std::process::exit(code);
    });
}

/// Bridge every async source onto the unified [`AppEvent`] channel. Runs on the
/// tokio runtime so the tokio receivers/watch channels are driven correctly.
fn spawn_forwarders(
    handle: &Handle,
    app_tx: UnboundedSender<AppEvent>,
    mut tel_events: UnboundedReceiver<Event>,
    sysmon: &Arc<SystemMonitor>,
    ai: &Arc<InlineCompletionBackend>,
) {
    // Telemetry Events.
    let tx = app_tx.clone();
    handle.spawn(async move {
        while let Some(e) = tel_events.recv().await {
            if tx.send(AppEvent::Telemetry(e)).is_err() {
                break;
            }
        }
    });

    // Sysmon stats (watch channel: emit the seed, then every change).
    let mut srx = sysmon.subscribe();
    let tx = app_tx.clone();
    handle.spawn(async move {
        let _ = tx.send(AppEvent::Sys(srx.borrow().clone()));
        while srx.changed().await.is_ok() {
            if tx.send(AppEvent::Sys(srx.borrow().clone())).is_err() {
                break;
            }
        }
    });

    // AI status (watch channel).
    let mut arx = ai.subscribe();
    let tx = app_tx.clone();
    handle.spawn(async move {
        let _ = tx.send(AppEvent::Ai(arx.borrow().clone()));
        while arx.changed().await.is_ok() {
            if tx.send(AppEvent::Ai(arx.borrow().clone())).is_err() {
                break;
            }
        }
    });
}

/// Watch the workspace root recursively (Phase-4 wave 2, §5.1). Raw notify
/// callbacks are filtered through the tree's ignore rules
/// ([`is_watch_relevant`]) and debounced 250ms ([`WatchDebounce`]) on the tokio
/// runtime; each settled burst emits one [`AppEvent::TreeChanged`], which the
/// app answers with an expansion-preserving re-scan. Returns the watcher, which
/// must be kept alive for the process lifetime; `None` (with a stderr note) if
/// the watch could not be established — the tree simply stops auto-refreshing.
fn spawn_fs_watch(
    root: &Path,
    handle: &Handle,
    app_tx: UnboundedSender<AppEvent>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};

    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Judge relevance on the path *relative to the root* so a dot-directory or
    // otherwise-ignored ancestor of the workspace root can't drop everything.
    let rel_root = root.to_path_buf();
    let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(ev) = res {
            // Only changes the tree would display; drops `.o`/build-dir bursts.
            let relevant = ev
                .paths
                .iter()
                .any(|p| is_watch_relevant(p.strip_prefix(&rel_root).unwrap_or(p)));
            if relevant {
                let _ = raw_tx.send(());
            }
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[jade] fs-watch unavailable: {e}");
            return None;
        }
    };
    if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
        eprintln!("[jade] fs-watch failed for {}: {e}", root.display());
        return None;
    }

    // Debounce loop: WatchDebounce holds the pure 250ms-window logic; this task
    // supplies the clock and the channel plumbing.
    handle.spawn(async move {
        let start = std::time::Instant::now();
        let mut deb = WatchDebounce::new(250);
        loop {
            if deb.is_pending() {
                // Poll for more events in short slices so the window can lapse.
                match tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    raw_rx.recv(),
                )
                .await
                {
                    Ok(Some(())) => deb.on_event(start.elapsed().as_millis() as u64),
                    Ok(None) => break, // watcher dropped
                    Err(_) => {}       // timeout slice — just re-check the window
                }
                if deb.poll(start.elapsed().as_millis() as u64)
                    && app_tx.send(AppEvent::TreeChanged).is_err()
                {
                    break;
                }
            } else {
                match raw_rx.recv().await {
                    Some(()) => deb.on_event(start.elapsed().as_millis() as u64),
                    None => break,
                }
            }
        }
    });

    Some(watcher)
}

/// Watch the workspace root for hardware-source changes (plan A5): only
/// `.v/.qsf/.sdc` files, and never anything under `.jade/` — the build writes
/// its artifacts there, and `is_watch_relevant` allows `.jade`, so without
/// the exclusion every build would re-trigger itself. Each settled 250 ms
/// burst emits one `AppEvent::Hw(SourcesChanged)`.
fn spawn_hw_watch(
    root: &Path,
    handle: &Handle,
    app_tx: UnboundedSender<AppEvent>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};

    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let rel_root = root.to_path_buf();
    let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(ev) = res {
            let relevant = ev.paths.iter().any(|p| {
                let rel = p.strip_prefix(&rel_root).unwrap_or(p);
                let under_jade = rel
                    .components()
                    .any(|c| c.as_os_str() == ".jade");
                let hw_ext = matches!(
                    rel.extension().and_then(|e| e.to_str()),
                    Some("v" | "sv" | "svh" | "vh" | "qsf" | "sdc")
                );
                !under_jade && hw_ext
            });
            if relevant {
                let _ = raw_tx.send(());
            }
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[jade] hw watch unavailable: {e}");
            return None;
        }
    };
    if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
        eprintln!("[jade] hw watch failed for {}: {e}", root.display());
        return None;
    }

    handle.spawn(async move {
        let start = std::time::Instant::now();
        let mut deb = WatchDebounce::new(250);
        loop {
            if deb.is_pending() {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    raw_rx.recv(),
                )
                .await
                {
                    Ok(Some(())) => deb.on_event(start.elapsed().as_millis() as u64),
                    Ok(None) => break,
                    Err(_) => {}
                }
                if deb.poll(start.elapsed().as_millis() as u64)
                    && app_tx
                        .send(AppEvent::Hw(jade_hw::HwEvent::SourcesChanged))
                        .is_err()
                {
                    break;
                }
            } else {
                match raw_rx.recv().await {
                    Some(()) => deb.on_event(start.elapsed().as_millis() as u64),
                    None => break,
                }
            }
        }
    });

    Some(watcher)
}

/// `--smoke term` (Phase-4 wave 2): create a real terminal with cwd = workspace
/// root, type `printf hello-jade-term`, and poll snapshots until the text shows
/// up in the grid (jade-term's own integration-test pattern). Prints a
/// `--smoke wg3d` (deliverable §7): instantiate the 3D weight-grid module,
/// feed 70 synthetic frames of a 32×32 buffer through its ring (asserting the
/// ring caps at 64), project the default framed camera, and print a
/// machine-readable line. No window; exercises the real telemetry apply feed.
fn run_smoke_wg3d(deps: AppDeps) {
    use crate::wg3d::{grid::build_bars, math};

    let mut app = JadeApp::assemble(deps);

    // 70 frames of a 32×32 buffer, streamed through the SAME apply path the GUI
    // uses (Event::Tensor → app.wg3d.on_frame ring).
    for step in 0..70i64 {
        let data: Vec<f32> = (0..32 * 32)
            .map(|i| ((i as f32) * 0.017 + step as f32 * 0.1).sin())
            .collect();
        app.apply_app_event(AppEvent::Telemetry(Event::Tensor {
            name: "W".to_string(),
            step,
            rows: 32,
            cols: 32,
            src_rows: None,
            src_cols: None,
            dtype: "f32".to_string(),
            data,
        }));
    }

    // Ring must cap at 64 (independent of the training view's 32).
    let ring_len = app.wg3d.frames_len();
    if ring_len != 64 {
        println!("[smoke] FAIL wg3d ring={ring_len} (expected 64)");
        return;
    }

    // Open the overlay (selects "W") and build the current frame's bars.
    app.wg3d.open(Some("W"));
    let Some(bars) = app.wg3d.current_bars() else {
        println!("[smoke] FAIL wg3d produced no bars");
        return;
    };
    let n = bars.bars.len();

    // Depth-sort the bars back-to-front and confirm the order is monotonic.
    let view = app.wg3d.camera.view_matrix();
    let mut depths: Vec<f32> = bars
        .bars
        .iter()
        .map(|b| math::view_depth(&view, b.centroid()))
        .collect();
    depths.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let depth_sorted = depths.windows(2).all(|w| w[0] <= w[1]);

    // Project the scene origin through the default framed camera.
    let (w, h) = (1400.0f32, 862.0f32);
    let proj = math::Mat4::perspective(app.wg3d.camera.fov_y, w / h, 0.1, 4000.0);
    let mvp = proj.mul(&view);
    let sample = math::project(&mvp, [0.0, 0.0, 0.0], w, h);

    match sample {
        Some(pr) if depth_sorted => println!(
            "[smoke] wg3d bars={n} depth_sorted=ok proj_sample=<{:.1},{:.1}>",
            pr.x, pr.y
        ),
        Some(_) => {
            println!("[smoke] FAIL wg3d depth sort not monotonic");
            return;
        }
        None => {
            println!("[smoke] FAIL wg3d origin projected behind camera");
            return;
        }
    }

    // Perf probe: per-frame CPU cost of build (colormap+subsample) + depth sort
    // at the interactive default (64×64) and the raised budget (128×128).
    for dim in [64u32, 128] {
        let data: Vec<f32> = (0..dim as usize * dim as usize)
            .map(|i| ((i as f32) * 0.013).sin())
            .collect();
        let f = crate::training::TensorFrame {
            step: 0,
            rows: dim,
            cols: dim,
            src_rows: None,
            src_cols: None,
            data,
        };
        let t0 = std::time::Instant::now();
        let g = build_bars(&f).expect("bars");
        let mut ds: Vec<(f32, usize)> = g
            .bars
            .iter()
            .enumerate()
            .map(|(i, b)| (math::view_depth(&view, b.centroid()), i))
            .collect();
        ds.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let us = t0.elapsed().as_micros();
        println!(
            "[smoke] wg3d perf {dim}x{dim}: build+sort {us}us ({} bars)",
            g.bars.len()
        );
    }
}


/// with the total symbol count and the top-level count.
fn run_smoke_structure(deps: AppDeps, target: Option<PathBuf>) {
    let target = match target.or_else(|| deps.active_file.clone()) {
        Some(t) => t,
        None => {
            println!("[smoke] FAIL structure: no file (usage: --smoke structure <file>)");
            return;
        }
    };
    let mut app = JadeApp::assemble(deps);
    app.open_file(target.clone());
    let symbols = app.active_symbols();
    let total = structure::count(symbols);
    let top = symbols.len();
    println!(
        "[smoke] structure {} symbols={} top_level={}",
        target.display(),
        total,
        top
    );
}
/// each with its relative-path hint when it differs from the bare name.
fn run_smoke_quickopen(deps: AppDeps, query: &str) {
    let root = deps.workspace_root.clone();
    let tree = workspace_tree::FileTree::scan_full(root.clone());
    let files = quick_open::flatten(&tree);
    let matches = quick_open::filter(&files, query, &root);
    println!(
        "[smoke] quickopen query={:?} matches={} (of {} files)",
        query,
        matches.len(),
        files.len()
    );
    for m in &matches {
        match &m.hint {
            Some(hint) => println!("  {}  ·  {}", m.name, hint),
            None => println!("  {}", m.name),
        }
    }
}

/// machine-readable `[smoke] term ok cols=<c> rows=<r>` line.
fn run_smoke_term(runtime: tokio::runtime::Runtime, deps: AppDeps) {
    runtime.block_on(async move {
        let term = deps.term.clone();
        let id = match term.create(&deps.workspace_root) {
            Ok(id) => id,
            Err(e) => {
                println!("[smoke] FAIL term: {e}");
                return;
            }
        };
        term.write(id, b"printf 'hello-jade-term\\n'\n");

        // Generous timeout: shell startup (user dotfiles) can be slow.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut ok = false;
        let (mut cols, mut rows) = (0usize, 0usize);
        while std::time::Instant::now() < deadline {
            if let Some(snap) = term.snapshot(id) {
                cols = snap.cols;
                rows = snap.rows;
                if snap.contains_text("hello-jade-term") {
                    ok = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        term.destroy(id);
        if ok {
            println!("[smoke] term ok cols={cols} rows={rows}");
        } else {
            println!("[smoke] FAIL term: text never appeared in the grid");
        }
    });
}

/// Run the same build (then run) action functions the buttons call, printing
/// machine-readable `[smoke]` lines, then exit. No window; events are pumped on
/// the tokio runtime directly (deliverable §7).
fn run_smoke(
    runtime: tokio::runtime::Runtime,
    deps: AppDeps,
    mut app_rx: UnboundedReceiver<AppEvent>,
    mode: &str,
) {
    runtime.block_on(async move {
        let mut app = JadeApp::assemble(deps);

        if app.active_file.is_none() {
            println!("[smoke] FAIL no active file (pass --file or --project)");
            return;
        }

        // Build.
        app.action_build();
        while app.building {
            match app_rx.recv().await {
                Some(ev) => app.apply_app_event(ev),
                None => break,
            }
        }
        let ok = app
            .last_build
            .as_ref()
            .map(|b| b.success && b.executable.is_some())
            .unwrap_or(false);
        if !ok {
            // Headline the first real *error* — clang emits warnings first, and
            // leading with one (e.g. -Wsign-compare) hides the actual failure.
            let reason = app
                .last_build
                .as_ref()
                .and_then(|b| {
                    b.errors
                        .iter()
                        .find(|e| e.severity == jade_build::Severity::Error)
                        .or_else(|| b.errors.first())
                })
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "compile produced no executable".to_string());
            println!("[smoke] FAIL build: {reason}");
            return;
        }
        let exe = app
            .last_build
            .as_ref()
            .unwrap()
            .executable
            .clone()
            .unwrap();
        println!("[smoke] build ok {}", exe.display());

        if mode == "build-run" {
            // Headless: skip the pre-run tracking panel, launch directly.
            app.launch_run();
            while app.running {
                match app_rx.recv().await {
                    Some(ev) => app.apply_app_event(ev),
                    None => break,
                }
            }
            match &app.last_run {
                Some(r) => println!(
                    "[smoke] run exit={} duration={} executed_lines={}",
                    r.exit_code, r.duration_ms, r.executed_lines
                ),
                None => println!("[smoke] FAIL run produced no result"),
            }
        }
    });
}

/// `--smoke hw` (hardware mode, plan A10): run the headless engine loop
/// against the workspace — build, HELLO, deterministic STEP, live LED
/// activity, hot swap with input replay — and print `[smoke]` lines. Skips
/// cleanly when verilator is not installed.
fn run_smoke_hw(runtime: tokio::runtime::Runtime, deps: AppDeps) {
    use jade_hw::{HwCommand, HwEvent, HwRunState};

    if jade_hw::detect_verilator().is_err() {
        println!("[smoke] SKIP hw: verilator is not installed (brew install verilator)");
        return;
    }
    /// Drain events until `pred` matches, or report a timeout.
    async fn wait_hw(
        ev: &mut tokio::sync::mpsc::UnboundedReceiver<jade_hw::HwEvent>,
        what: &str,
        timeout_s: u64,
        mut pred: impl FnMut(&jade_hw::HwEvent) -> bool,
    ) -> Option<jade_hw::HwEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(left, ev.recv()).await {
                Ok(Some(e)) => {
                    if pred(&e) {
                        return Some(e);
                    }
                }
                _ => {
                    println!("[smoke] FAIL hw: timed out waiting for {what}");
                    return None;
                }
            }
        }
    }

    let root = deps.workspace_root.clone();
    runtime.block_on(async move {
        let (ev_tx, mut ev) = tokio::sync::mpsc::unbounded_channel();
        let engine = jade_hw::start_session(root.clone(), ev_tx);

        let Some(done) = wait_hw(&mut ev, "the first CompileDone", 120, |e| {
            matches!(e, HwEvent::CompileDone { .. })
        })
        .await
        else {
            return;
        };
        if done != (HwEvent::CompileDone { ok: true }) {
            println!("[smoke] FAIL hw: first build failed");
            return;
        }
        let Some(HwEvent::PortsDiscovered { top }) = wait_hw(&mut ev, "HELLO", 10, |e| {
            matches!(e, HwEvent::PortsDiscovered { .. })
        })
        .await
        else {
            return;
        };
        println!("[smoke] hw build ok top={top}");

        // Deterministic step while paused.
        let _ = engine.commands.send(HwCommand::Pause);
        if wait_hw(&mut ev, "paused", 5, |e| {
            matches!(e, HwEvent::RunState(HwRunState::Paused))
        })
        .await
        .is_none()
        {
            return;
        }
        let t0 = std::time::Instant::now();
        let _ = engine.commands.send(HwCommand::Step(1 << 25));
        let Some(HwEvent::LedFrame { bitmask, .. }) = wait_hw(&mut ev, "the stepped LED frame", 60, |e| {
            matches!(e, HwEvent::LedFrame { .. })
        })
        .await
        else {
            return;
        };
        let rate = (1u64 << 25) as f64 / t0.elapsed().as_secs_f64();
        println!(
            "[smoke] hw step mask={bitmask:#07b} rate={:.1}Mcyc/s",
            rate / 1e6
        );

        // Live run shows LED activity.
        let _ = engine.commands.send(HwCommand::Resume);
        let mut masks = std::collections::HashSet::new();
        let live_deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < live_deadline && masks.len() < 2 {
            let left = live_deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(left, ev.recv()).await {
                Ok(Some(HwEvent::LedFrame { bitmask, .. })) => {
                    masks.insert(bitmask);
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }

        // Hot swap with DIP replay.
        let _ = engine.commands.send(HwCommand::SetDip(2, true));
        let _ = engine.commands.send(HwCommand::Recompile);
        if wait_hw(&mut ev, "the hot-swap CompileDone", 60, |e| {
            matches!(e, HwEvent::CompileDone { ok: true })
        })
        .await
        .is_none()
        {
            return;
        }
        if wait_hw(&mut ev, "HELLO after the hot swap", 10, |e| {
            matches!(e, HwEvent::PortsDiscovered { .. })
        })
        .await
        .is_none()
        {
            return;
        }
        engine.stop();
        let _ = engine.task.await;
        println!(
            "[smoke] hw ok live_masks={} hot_swap=ok replay=ok",
            masks.len()
        );
    });
}

/// `--smoke decorations <file>` (Phase-4 wave 3): run the static size scanner
/// and the flow analyzer over a file and print a machine-readable line proving
/// both pure pipelines work headlessly. No window; no app state needed.
fn run_smoke_decorations(target: Option<PathBuf>) {
    let Some(target) = target else {
        println!("[smoke] FAIL decorations: no file (usage: --smoke decorations <file>)");
        return;
    };
    let text = match std::fs::read(&target) {
        Ok(raw) => String::from_utf8_lossy(&raw).into_owned(),
        Err(e) => {
            println!("[smoke] FAIL decorations {}: {e}", target.display());
            return;
        }
    };
    let sizes = decorations::size_annotations::collect_size_annotations(&text);
    let flow = decorations::flow::analyze(&text);
    println!(
        "[smoke] decorations {} sizes={} glyphs={} segments={}",
        target.display(),
        sizes.len(),
        flow.glyphs.len(),
        flow.segments.len(),
    );
}

/// `--smoke open <file>` (deliverable §7/§8): assemble the app (which scans the
/// tree and seeds any `--file`/`--project` tab), open `<file>` through the real
/// tab + tree-sitter-highlight path, and print a machine-readable line proving
/// the viewer pipeline works headlessly.
fn run_smoke_open(deps: AppDeps, target: Option<PathBuf>) {
    let target = match target.or_else(|| deps.active_file.clone()) {
        Some(t) => t,
        None => {
            println!("[smoke] FAIL open: no file (usage: --smoke open <file>)");
            return;
        }
    };
    let mut app = JadeApp::assemble(deps);
    app.open_file(target.clone());
    match app.editor.active_tab() {
        Some(tab) if tab.path == target => println!(
            "[smoke] open {} lines={} spans={}",
            target.display(),
            tab.line_count(),
            tab.span_count()
        ),
        _ => println!("[smoke] FAIL open {}", target.display()),
    }
}

/// `--smoke edit <file>` (E2): open a file, apply a scripted edit sequence
/// (insert 'x' at 0:0, newline+indent, undo, redo, word-right ×3) directly on the
/// buffer, and print a machine-readable line proving the editing core works
/// headlessly.
fn run_smoke_edit(deps: AppDeps, target: Option<PathBuf>) {
    let target = match target {
        Some(t) => t,
        None => {
            println!("[smoke] FAIL edit: no file (usage: --smoke edit <file>)");
            return;
        }
    };
    let mut app = JadeApp::assemble(deps);
    app.open_file(target.clone());
    let Some(tab) = app.editor.active_tab_mut() else {
        println!("[smoke] FAIL edit: could not open {}", target.display());
        return;
    };
    // Caret to document start, then the scripted sequence.
    tab.buffer.set_caret(0);
    let mut lsp_changes = 0usize;
    lsp_changes += tab.buffer.type_char('x').changes.len();
    lsp_changes += tab.buffer.insert_newline().changes.len();
    tab.buffer.undo();
    tab.buffer.redo();
    tab.buffer.move_word_right(false);
    tab.buffer.move_word_right(false);
    tab.buffer.move_word_right(false);
    let caret = tab.caret_point();
    println!(
        "[smoke] edit {} version={} dirty={} caret={}:{} lsp_changes={}",
        target.display(),
        tab.buffer.version(),
        tab.buffer.is_dirty(),
        caret.row,
        caret.col,
        lsp_changes
    );
}

/// `--smoke ghost` (E3): drive the AI ghost-text cache + post-processing path
/// with a **canned** model response (no llama-server — the string below stands in
/// for the `/infill` result the real backend would return). Exercises a fresh
/// cache miss, then a typed-through hit served from the cache, printing a
/// machine-readable `[smoke] ghost cached=<bool> text="…"` line for each.
fn run_smoke_ghost() {
    use ghost::{post_process, GhostCache, MAX_LINES};

    // The scripted context: caret just after `for (int i = 0`, with a `)` already
    // sitting after the cursor on the line.
    let prefix1 = "int main() {\n    for (int i = 0";
    let suffix = ")";
    let line_suffix = ")";
    // Canned model output (what `/infill` would return). Note the trailing `)`
    // that duplicates what already follows the cursor — post-processing drops it,
    // and the blank line beyond is truncated.
    let canned = "; i < n; i++)\n\n    // loop body here";

    let mut cache = GhostCache::new();

    // 1) Cache miss: fresh response → post-process → cache the RAW output.
    let cached_hit = cache.lookup(prefix1, suffix).is_some();
    let text1 = post_process(canned, line_suffix, MAX_LINES).unwrap_or_default();
    cache.put(prefix1, suffix, canned);
    println!("[smoke] ghost cached={cached_hit} text={text1:?}");

    // 2) Typed-through hit: the user typed `; i < n` through the suggestion, so the
    //    remainder is served instantly from the cache (no request).
    let prefix2 = "int main() {\n    for (int i = 0; i < n";
    let served = cache.lookup(prefix2, suffix);
    let cached_hit2 = served.is_some();
    let text2 = served
        .and_then(|raw| post_process(&raw, line_suffix, MAX_LINES))
        .unwrap_or_default();
    println!("[smoke] ghost cached={cached_hit2} text={text2:?}");
}

/// `--smoke lsp <dir>` (E2, best-effort): initialize clangd on a temp project,
/// open a file with an error, await the first diagnostics event, and print a
/// machine-readable line. Skips gracefully when clangd is unavailable.
fn run_smoke_lsp(runtime: tokio::runtime::Runtime, _deps: AppDeps, dir: Option<PathBuf>) {
    use jade_lsp::{LspClient, LspEvent};
    runtime.block_on(async move {
        // Build a throwaway project (or use the passed dir) with a known error.
        let root = match dir {
            Some(d) => d,
            None => {
                let d = std::env::temp_dir().join(format!("jade-lsp-smoke-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&d);
                let _ = std::fs::write(
                    d.join("main.cpp"),
                    "int main() { int x = ; return 0; }\n",
                );
                d
            }
        };
        let file = root.join("main.cpp");
        let t0 = std::time::Instant::now();
        let mut handle = match LspClient::initialize(&root, None).await {
            Ok(h) => h,
            Err(e) => {
                println!("[smoke] lsp skipped (clangd unavailable: {e})");
                return;
            }
        };
        let mut events = handle.take_events().expect("events");
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let _ = handle.did_open(&file, &text, 1);
        // Await the first non-empty diagnostics for our file, with a timeout.
        let deadline = std::time::Duration::from_secs(20);
        let got = tokio::time::timeout(deadline, async {
            loop {
                match events.recv().await {
                    Some(LspEvent::Diagnostics { path, diagnostics })
                        if path == file && !diagnostics.is_empty() =>
                    {
                        return Some(diagnostics.len());
                    }
                    Some(LspEvent::Exited) | None => return None,
                    _ => continue,
                }
            }
        })
        .await;
        match got {
            Ok(Some(n)) => println!(
                "[smoke] lsp diagnostics={} in {}ms",
                n,
                t0.elapsed().as_millis()
            ),
            Ok(None) => println!("[smoke] lsp skipped (clangd exited before diagnostics)"),
            Err(_) => println!("[smoke] lsp skipped (no diagnostics within timeout)"),
        }
        handle.shutdown().await;
    });
}

/// `--smoke sighelp`: drive the REAL app signature-help flow against clangd —
/// assemble the app, open a file with a call site (which spawns clangd), wait for
/// the session, position the caret inside `add(…)`, fire `schedule_signature_help`,
/// pump the unified event channel, and print whether a hint populated. Proves the
/// wiring (request → `AppEvent::SignatureHelp` → `on_signature_help` → state)
/// end-to-end. Skips gracefully if clangd never comes up.
fn run_smoke_sighelp(
    runtime: tokio::runtime::Runtime,
    deps: AppDeps,
    mut app_rx: UnboundedReceiver<AppEvent>,
) {
    runtime.block_on(async move {
        let root = deps.workspace_root.clone();
        let file = root.join("sighelp_smoke.cpp");
        let src = "int add(int a, int b) { return a + b; }\nint main() { int z = add(1, 2); return z; }\n";
        if std::fs::write(&file, src).is_err() {
            println!("[smoke] FAIL sighelp: could not write {}", file.display());
            return;
        }

        let mut app = JadeApp::assemble(deps);
        app.open_file(file.clone()); // triggers clangd init + didOpen

        // Pump until clangd is ready, or give up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !app.lsp_active() && std::time::Instant::now() < deadline {
            if let Ok(Some(ev)) =
                tokio::time::timeout(std::time::Duration::from_secs(1), app_rx.recv()).await
            {
                app.apply_app_event(ev);
            }
        }
        if !app.lsp_active() {
            println!("[smoke] sighelp skipped (clangd never became ready)");
            return;
        }

        // Caret just after the '(' in the CALL `add(1, 2)` (rfind → skip the
        // definition `int add(`).
        let open_paren = src.rfind("add(").map(|i| i + 4).unwrap();
        if let Some(tab) = app.editor.active_tab_mut() {
            tab.buffer.set_caret(open_paren);
        }

        // Retry: clangd may still be indexing right after didOpen.
        let mut got: Option<String> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while got.is_none() && std::time::Instant::now() < deadline {
            app.schedule_signature_help();
            // Drain events for a beat so the response lands and applies.
            let beat = std::time::Instant::now() + std::time::Duration::from_millis(700);
            while std::time::Instant::now() < beat {
                if let Ok(Some(ev)) =
                    tokio::time::timeout(std::time::Duration::from_millis(200), app_rx.recv()).await
                {
                    app.apply_app_event(ev);
                }
            }
            got = app.signature.as_ref().map(|s| s.label.clone());
        }

        match got {
            Some(label) => println!("[smoke] sighelp ok label={label:?}"),
            None => println!("[smoke] FAIL sighelp: no hint populated in the app"),
        }
    });
}

/// The file tree's scan root: the `--project` dir if given, else the active
/// file's parent directory, else the repo root.
fn resolve_workspace_root(args: &[String], active: Option<&std::path::Path>, root: &Path) -> PathBuf {
    if let Some(dir) = arg_value(args, "--project") {
        return PathBuf::from(dir);
    }
    if let Some(f) = active {
        if let Some(parent) = f.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.to_path_buf();
            }
        }
    }
    root.to_path_buf()
}

/// The app mode at launch (§B1): `--hardware` forces hardware mode; else the
/// workspace's persisted `ui.mode`; else software.
fn resolve_mode(args: &[String], workspace_root: &Path) -> AppMode {
    if args.iter().any(|a| a == "--hardware") {
        return AppMode::Hardware;
    }
    if workspace_state::load(workspace_root).mode.as_deref() == Some("hardware") {
        return AppMode::Hardware;
    }
    AppMode::Software
}

/// Whether a workspace is considered "open" at launch (inventory §2): true when
/// launched with `--project` or `--file`, else true only if the fallback root
/// already has restored tabs in its persisted `ui` blob. When false, `main` and
/// `JadeApp` show the welcome overlay and skip scanning the fallback repo root.
fn resolve_workspace_opened(args: &[String], workspace_root: &Path) -> bool {
    if arg_value(args, "--project").is_some() || arg_value(args, "--file").is_some() {
        return true;
    }
    !workspace_state::load(workspace_root).open_tabs.is_empty()
}

/// Resolve the jade-ide checkout root: `JADE_REPO_ROOT` if set, else the
/// compile-time `CARGO_MANIFEST_DIR/../../..` (jade is at `rust/crates/jade`).
fn repo_root() -> PathBuf {
    if let Some(r) = std::env::var_os("JADE_REPO_ROOT") {
        return PathBuf::from(r);
    }
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    fallback.canonicalize().unwrap_or(fallback)
}

/// `--file <path>` sets the active file directly; `--project <dir>` picks the
/// first `.cpp`/`.cc`/`.mm` alphabetically (deliverable §5).
fn resolve_active_file(args: &[String]) -> Option<PathBuf> {
    if let Some(f) = arg_value(args, "--file") {
        return Some(PathBuf::from(f));
    }
    if let Some(dir) = arg_value(args, "--project") {
        let mut cands: Vec<PathBuf> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("cpp") | Some("cc") | Some("mm")
                )
            })
            .collect();
        cands.sort();
        return cands.into_iter().next();
    }
    None
}

/// Value following `flag` in `args`, if present.
fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Replicate the pre-Phase-3 `--train <dir>` hatch: launch the probe's
/// `test_train` against our socket so the pipeline streams end to end.
fn maybe_launch_train(args: &[String], socket: &std::path::Path) {
    let Some(dir) = arg_value(args, "--train").map(PathBuf::from) else {
        return;
    };
    let dylib = dir
        .join("jade_probe.dylib")
        .canonicalize()
        .expect("probe dylib");
    std::process::Command::new(dir.join("test_train"))
        .current_dir(&dir)
        .env("DYLD_INSERT_LIBRARIES", dylib)
        .env("JADE_TELEMETRY_SOCK", socket)
        .env_remove("JADE_TRACK_ALL")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test_train");
    println!("launched test_train with injected probe");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--project`/`--file` force a workspace open regardless of persisted tabs.
    #[test]
    fn workspace_opened_with_project_or_file() {
        let root = std::env::temp_dir();
        assert!(resolve_workspace_opened(
            &["--project".to_string(), "/x".to_string()],
            &root
        ));
        assert!(resolve_workspace_opened(
            &["--file".to_string(), "/x.cpp".to_string()],
            &root
        ));
    }

    /// Bare launch: opened iff the fallback root has restored tabs (§2).
    #[test]
    fn workspace_opened_follows_restored_tabs() {
        let dir = std::env::temp_dir().join(format!("jade-wo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No persisted tabs → welcome mode.
        assert!(!resolve_workspace_opened(&[], &dir));
        // Persist one open tab → workspace considered open.
        let ui = workspace_state::WorkspaceUi {
            open_tabs: vec![workspace_state::TabState {
                path: "/p/a.cpp".to_string(),
                is_dirty: false,
            }],
            ..Default::default()
        };
        workspace_state::save(&dir, &ui);
        assert!(resolve_workspace_opened(&[], &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
