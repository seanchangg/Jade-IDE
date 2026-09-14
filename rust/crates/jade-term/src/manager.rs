//! [`TermManager`] — owns terminal instances and their PTY I/O threads.
//!
//! This is the Rust port of `src/main/pty-manager.ts` (the C++/Electron IDE's
//! `PtyManager`). The spawn semantics below are ported line-for-line and the
//! porting is cited in the doc comments. Where the TS relied on `node-pty` +
//! xterm.js, we use `alacritty_terminal`: its `tty` module owns the Unix PTY
//! and its `event_loop` reads the PTY, drives the VTE parser into the `Term`
//! state machine, and reports child exit race-free via `SIGCHLD`.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions, Pty, Shell};

use tokio::sync::mpsc;

use crate::snapshot::{Cell, CellFlags, Cursor, GridSnapshot};

/// Numeric terminal id, handed out starting from 1
/// (ports `pty-manager.ts:19` `nextId = 1`).
pub type TermId = u32;

/// Default scrollback retained above the viewport (ports the renderer's
/// scrollback expectation; `jade-feature-inventory.md` §5.2).
pub const DEFAULT_SCROLLBACK: usize = 1000;

/// Initial terminal geometry (ports `pty-manager.ts:30-31` `cols: 80, rows: 24`).
const INIT_COLS: u16 = 80;
const INIT_ROWS: u16 = 24;

/// How long the reaper waits after each signal before it sends the next one.
/// SIGHUP -> SIGTERM -> SIGKILL, so a close takes at most about three graces.
const KILL_GRACE: Duration = Duration::from_millis(500);

/// Cell pixel size reported to the kernel's winsize. Only affects `ws_xpixel`/
/// `ws_ypixel`; the terminal is character-addressed so the exact values are
/// cosmetic, but non-zero keeps well-behaved TUIs happy.
const CELL_W: u16 = 8;
const CELL_H: u16 = 16;

/// Events emitted to the renderer over a tokio mpsc channel.
///
/// `Damaged` is coalesced: at most one is queued per instance between snapshots
/// (a per-instance dirty flag; no per-cell damage list). The renderer reacts by
/// calling [`TermManager::snapshot`], which clears the flag so the next write
/// re-arms it. This is the "damage → notify → snapshot" pattern the GPUI panel
/// should follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermEvent {
    /// The grid changed; the renderer should re-snapshot instance `id`.
    Damaged { id: TermId },
    /// The child process exited. `code` is `None` if it was terminated by a
    /// signal (e.g. our `destroy` sends `SIGHUP`). The renderer writes the
    /// dim `[exited <code>]` line (`pty-manager.ts:82-90` → renderer §5.2).
    Exited { id: TermId, code: Option<i32> },
}

/// Errors from [`TermManager::create`].
#[derive(Debug)]
pub enum TermError {
    /// The PTY could not be allocated / the shell could not be spawned.
    ///
    /// Callers degrade gracefully, mirroring `pty-manager.ts:22` (`create`
    /// returning `null`) and the IPC layer returning `-1`
    /// (`pty-manager.ts:96`, inventory §5.2 "PTY creation returning `-1`").
    PtyUnavailable(std::io::Error),
}

impl std::fmt::Display for TermError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TermError::PtyUnavailable(e) => write!(f, "PTY unavailable: {e}"),
        }
    }
}

impl std::error::Error for TermError {}

/// Terminal grid dimensions, satisfying `alacritty_terminal`'s [`Dimensions`].
/// `total_lines == screen_lines` here because the grid's scrollback size is
/// configured separately via [`TermConfig::scrolling_history`].
#[derive(Clone, Copy)]
struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The event sink `alacritty_terminal` calls (on the PTY-reader thread) for
/// terminal events. Cloned into both the `Term` and the `EventLoop`.
#[derive(Clone)]
struct EventProxy {
    id: TermId,
    tx: mpsc::UnboundedSender<TermEvent>,
    dirty: Arc<AtomicBool>,
    /// Channel back into the PTY event loop, filled in right after the loop is
    /// created (the proxy must exist first). Carries the parser's answerback
    /// text (`Event::PtyWrite`).
    pty_tx: Arc<Mutex<Option<EventLoopSender>>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            // New grid content. Coalesce: only queue a Damaged on the
            // false→true transition of the dirty flag.
            Event::Wakeup => {
                if !self.dirty.swap(true, Ordering::AcqRel) {
                    let _ = self.tx.send(TermEvent::Damaged { id: self.id });
                }
            }
            // The terminal was queried (DSR cursor-position, DA device
            // attributes, XTGETTCAP, …) and the parser produced the reply.
            // Write it straight back to the PTY — interactive TUIs (Ink/Claude
            // Code, vim) block on or mis-layout without these answers.
            Event::PtyWrite(text) => {
                if let Some(sender) = self.pty_tx.lock().unwrap().as_ref() {
                    let _ = sender.send(Msg::Input(text.into_bytes().into()));
                }
            }
            // Race-free child exit (SIGCHLD). Ports the `onExit` callback with
            // exit code (`pty-manager.ts:82-90`).
            Event::ChildExit(status) => {
                let _ = self.tx.send(TermEvent::Exited { id: self.id, code: status.code() });
            }
            // Title/bell/clipboard/etc. are not part of the headless engine's
            // contract yet; the renderer layer can subscribe to more later.
            _ => {}
        }
    }
}

/// Type of the PTY reader thread's join handle (alacritty returns its loop +
/// state so the owner can reclaim them; we only need to join for child reaping).
type IoThread = JoinHandle<(EventLoop<Pty, EventProxy>, State)>;

/// A single live terminal: its state machine, the channel to its PTY thread,
/// and its dirty flag.
struct Instance {
    term: Arc<FairMutex<Term<EventProxy>>>,
    sender: EventLoopSender,
    io_thread: Option<IoThread>,
    dirty: Arc<AtomicBool>,
    cols: usize,
    rows: usize,
    scrollback: usize,
    /// The shell's pid. It called `setsid`, so it is also its session and
    /// process-group id.
    child_pid: libc::pid_t,
    /// The PTY master fd. Valid while the `Pty` lives, which the reaper keeps
    /// alive until the child is gone.
    master_fd: RawFd,
}

impl Instance {
    /// Ask the PTY thread to shut down and hand the child to a reaper thread.
    /// Ports `process.kill()` (`pty-manager.ts:64`).
    ///
    /// This never blocks the caller. `Pty`'s `Drop` sends `SIGHUP` and then
    /// waits for the shell, and that wait does not return while a foreground
    /// program that ignores `SIGHUP` (Claude Code, for one) holds the terminal
    /// open: the shell stays in the kernel's exit state until the program
    /// goes. Done on the UI thread, that froze the app. The reaper thread
    /// escalates to `SIGTERM` and `SIGKILL` so the wait is bounded.
    fn shutdown(&mut self) -> Option<JoinHandle<()>> {
        // `send` also wakes the poller, so a thread blocked in `poll.wait`
        // returns and observes the shutdown.
        let _ = self.sender.send(Msg::Shutdown);
        let handle = self.io_thread.take()?;
        let pid = self.child_pid;
        let fd = self.master_fd;
        std::thread::Builder::new()
            .name("PTY reaper".into())
            .spawn(move || {
                // The reader thread returns the loop, which owns the `Pty`.
                // Keep it alive until the child is gone so `master_fd` stays
                // valid and `Pty::drop`'s wait returns at once.
                let owned = handle.join().ok();
                reap(pid, fd);
                drop(owned);
            })
            .ok()
    }
}

/// Stop the shell `pid` and everything it runs on the terminal `master_fd`:
/// `SIGHUP` first, then `SIGTERM` and `SIGKILL` to the shell and to the
/// terminal's foreground process group, one [`KILL_GRACE`] apart. Returns once
/// the shell is reaped or every signal was sent.
fn reap(pid: libc::pid_t, master_fd: RawFd) {
    // SAFETY: plain signal/wait syscalls on a pid this manager spawned.
    unsafe {
        libc::kill(pid, libc::SIGHUP);
    }
    if wait_exit(pid, KILL_GRACE) {
        return;
    }
    // Whatever runs in the foreground is what keeps the shell from exiting.
    let fg = unsafe { libc::tcgetpgrp(master_fd) };
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        unsafe {
            if fg > 0 && fg != pid {
                libc::killpg(fg, sig);
            }
            libc::killpg(pid, sig);
            libc::kill(pid, sig);
        }
        if wait_exit(pid, KILL_GRACE) {
            return;
        }
    }
}

/// Poll `waitpid(WNOHANG)` for up to `timeout`. True once the child is reaped
/// (or was already reaped by someone else).
fn wait_exit(pid: libc::pid_t, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid || r == -1 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct ManagerInner {
    instances: HashMap<TermId, Instance>,
    next_id: TermId,
    scrollback: usize,
}

/// Owns terminal instances and hands out ids from 1.
///
/// Renderer-agnostic and safe to share behind an `Arc` (`&self` methods); all
/// mutable state lives behind an internal mutex, and each `Term`'s own state is
/// behind its own fair mutex so snapshots clone out without blocking PTY I/O for
/// long. Port of `PtyManager` (`pty-manager.ts:17-91`).
pub struct TermManager {
    inner: Mutex<ManagerInner>,
    tx: mpsc::UnboundedSender<TermEvent>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<TermEvent>>>,
}

impl TermManager {
    /// Create an empty manager. Take the event stream once via
    /// [`TermManager::take_events`].
    pub fn new() -> Self {
        Self::with_scrollback(DEFAULT_SCROLLBACK)
    }

    /// Like [`TermManager::new`] but with a custom scrollback length (lines kept
    /// above the viewport and returned by [`snapshot`](Self::snapshot)).
    pub fn with_scrollback(scrollback: usize) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        TermManager {
            inner: Mutex::new(ManagerInner { instances: HashMap::new(), next_id: 1, scrollback }),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Take the single event-stream receiver. Returns `None` if already taken.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<TermEvent>> {
        self.rx.lock().unwrap().take()
    }

    /// Spawn a shell in `cwd` and return its id.
    ///
    /// Ports `PtyManager.create` (`pty-manager.ts:21-41`):
    /// - shell = `$SHELL` else `/bin/zsh` (`:25`)
    /// - terminfo name `xterm-256color` (`:29`, handled by alacritty's PTY)
    /// - `cwd` = the given path (`:30`, workspace root per inventory §5.2)
    /// - initial 80x24 (`:30-31`)
    /// - env `TERM=xterm-256color`, `COLORTERM=truecolor` (`:34-35`)
    ///
    /// Returns [`TermError::PtyUnavailable`] when the PTY can't be allocated —
    /// the caller degrades gracefully (`pty-manager.ts:22`, `:96`).
    pub fn create(&self, cwd: &Path) -> Result<TermId, TermError> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        self.spawn(cwd, Shell::new(shell, Vec::new()))
    }

    /// Spawn with an explicit shell program (production uses [`create`], which
    /// resolves `$SHELL`/`/bin/zsh`). Handy for deterministic tests that want
    /// `/bin/sh`.
    ///
    /// [`create`]: Self::create
    pub fn create_with_shell(&self, cwd: &Path, shell: &Path) -> Result<TermId, TermError> {
        self.spawn(cwd, Shell::new(shell.to_string_lossy().into_owned(), Vec::new()))
    }

    fn spawn(&self, cwd: &Path, shell: Shell) -> Result<TermId, TermError> {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        let scrollback = inner.scrollback;

        // env: inherit + override TERM/COLORTERM (`pty-manager.ts:32-36`).
        let mut env = std::collections::HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());

        // NB: on Windows `PtyOptions` also has `escape_args`; this crate is the
        // Unix (macOS) PTY engine, so all fields are specified explicitly.
        let options = PtyOptions {
            shell: Some(shell),
            working_directory: Some(cwd.to_path_buf()),
            drain_on_exit: false,
            env,
        };

        let window_size = WindowSize {
            num_lines: INIT_ROWS,
            num_cols: INIT_COLS,
            cell_width: CELL_W,
            cell_height: CELL_H,
        };

        // Allocate the PTY + fork/exec the shell. Failure => degrade.
        let pty = tty::new(&options, window_size, id as u64)
            .map_err(TermError::PtyUnavailable)?;

        let child_pid = pty.child().id() as libc::pid_t;
        let master_fd = pty.file().as_raw_fd();

        let dirty = Arc::new(AtomicBool::new(false));
        let pty_tx = Arc::new(Mutex::new(None));
        let proxy = EventProxy {
            id,
            tx: self.tx.clone(),
            dirty: Arc::clone(&dirty),
            pty_tx: Arc::clone(&pty_tx),
        };

        let size = TermSize { cols: INIT_COLS as usize, rows: INIT_ROWS as usize };
        let config = TermConfig { scrolling_history: scrollback, ..Default::default() };
        let term = Term::new(config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        // The event loop reads the PTY, drives the parser into `term`, and
        // notifies `proxy`. `drain_on_exit=false`, `ref_test=false`.
        let event_loop = EventLoop::new(Arc::clone(&term), proxy, pty, false, false)
            .map_err(TermError::PtyUnavailable)?;
        let sender = event_loop.channel();
        // Fill the answerback channel before the reader thread starts, so no
        // query reply can race the slot being empty.
        *pty_tx.lock().unwrap() = Some(sender.clone());
        let io_thread = event_loop.spawn();

        inner.instances.insert(
            id,
            Instance {
                term,
                sender,
                io_thread: Some(io_thread),
                dirty,
                cols: INIT_COLS as usize,
                rows: INIT_ROWS as usize,
                scrollback,
                child_pid,
                master_fd,
            },
        );
        inner.next_id += 1;
        Ok(id)
    }

    /// Write bytes to the PTY. User keystrokes and programmatic writes (e.g.
    /// build `[jade]` status lines) both flow through here to the same PTY
    /// (inventory §5.2). No-op for unknown ids (`pty-manager.ts:43-48`).
    pub fn write(&self, id: TermId, bytes: &[u8]) {
        let inner = self.inner.lock().unwrap();
        if let Some(inst) = inner.instances.get(&id) {
            let _ = inst.sender.send(Msg::Input(bytes.to_vec().into()));
        }
    }

    /// Resize the terminal to `cols`x`rows`. Resizes the grid *and* the kernel
    /// winsize (so the child receives `SIGWINCH`). No-op / tolerant for unknown
    /// or dead instances (`pty-manager.ts:50-59`).
    pub fn resize(&self, id: TermId, cols: u16, rows: u16) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(inst) = inner.instances.get_mut(&id) {
            let size = TermSize { cols: cols as usize, rows: rows as usize };
            inst.term.lock().resize(size);
            inst.cols = cols as usize;
            inst.rows = rows as usize;
            let _ = inst.sender.send(Msg::Resize(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: CELL_W,
                cell_height: CELL_H,
            }));
            // The grid changed but alacritty doesn't emit Wakeup for a resize;
            // arm damage ourselves so the renderer re-snapshots.
            if !inst.dirty.swap(true, Ordering::AcqRel) {
                let _ = self.tx.send(TermEvent::Damaged { id });
            }
        }
    }

    /// Kill and remove a terminal (`pty-manager.ts:61-67`).
    ///
    /// Returns at once. The id is gone from the manager immediately; the shell
    /// and its foreground job are stopped by a background reaper thread that
    /// escalates from `SIGHUP` to `SIGKILL` (see [`Instance::shutdown`]).
    pub fn destroy(&self, id: TermId) {
        let inst = {
            let mut inner = self.inner.lock().unwrap();
            inner.instances.remove(&id)
        };
        // The manager mutex is released before the shutdown starts.
        if let Some(mut inst) = inst {
            let _ = inst.shutdown();
        }
    }

    /// Kill and remove every terminal (`pty-manager.ts:69-73`). Unlike
    /// [`destroy`](Self::destroy) this waits for the reapers, so every child
    /// is gone when it returns (bounded by the signal escalation).
    pub fn destroy_all(&self) {
        let instances: Vec<Instance> = {
            let mut inner = self.inner.lock().unwrap();
            inner.instances.drain().map(|(_, v)| v).collect()
        };
        let reapers: Vec<JoinHandle<()>> =
            instances.into_iter().filter_map(|mut inst| inst.shutdown()).collect();
        for r in reapers {
            let _ = r.join();
        }
    }

    /// True if `id` refers to a live instance.
    pub fn contains(&self, id: TermId) -> bool {
        self.inner.lock().unwrap().instances.contains_key(&id)
    }

    /// Number of live instances.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().instances.len()
    }

    /// Whether there are no live instances.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clone out an owned snapshot of the grid (viewport + scrollback + cursor).
    /// Returns `None` for unknown ids. Clears the instance's dirty flag first,
    /// so any write racing this snapshot re-arms a fresh `Damaged`.
    pub fn snapshot(&self, id: TermId) -> Option<GridSnapshot> {
        let inner = self.inner.lock().unwrap();
        let inst = inner.instances.get(&id)?;

        // Clear damage *before* reading, so a concurrent Wakeup is not lost.
        inst.dirty.store(false, Ordering::Release);

        let scrollback_cap = inst.scrollback;
        let guard = inst.term.lock();
        let grid = guard.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let offset = grid.display_offset() as i32;

        // Viewport: display row i => grid Line(i - display_offset).
        let mut cells = Vec::with_capacity(rows);
        for i in 0..rows {
            let line = Line(i as i32 - offset);
            let row = &grid[line];
            let mut out = Vec::with_capacity(cols);
            for c in 0..cols {
                out.push(convert_cell(&row[Column(c)]));
            }
            cells.push(out);
        }

        // Scrollback: the lines directly above the viewport, oldest first.
        // Available above the current viewport top (-offset) is
        // history_size - display_offset.
        let avail = grid.history_size().saturating_sub(offset as usize);
        let n = scrollback_cap.min(avail);
        let mut scrollback = Vec::with_capacity(n);
        for j in 0..n {
            // Oldest first: top-n .. top-1, where top = -offset.
            let line = Line(-offset - n as i32 + j as i32);
            let row = &grid[line];
            let mut out = Vec::with_capacity(cols);
            for c in 0..cols {
                out.push(convert_cell(&row[Column(c)]));
            }
            scrollback.push(out);
        }

        // Cursor: always in the screen region; map into viewport rows.
        let point = grid.cursor.point;
        let cursor_line = point.line.0 + offset;
        let mode = *guard.mode();
        let show = mode.contains(TermMode::SHOW_CURSOR);
        let visible = show && cursor_line >= 0 && (cursor_line as usize) < rows;
        let cursor = Cursor {
            col: point.column.0.min(cols.saturating_sub(1)),
            line: cursor_line.clamp(0, rows.saturating_sub(1) as i32) as usize,
            visible,
        };

        Some(GridSnapshot {
            cols,
            rows,
            cursor,
            cells,
            scrollback,
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        })
    }
}

impl Default for TermManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TermManager {
    fn drop(&mut self) {
        // Best-effort: start a reaper for every still-running child and give
        // them a bounded time to finish, so a quit does not leave orphans. A
        // reaper that is still stuck after that must not hold the process.
        let Ok(mut inner) = self.inner.lock() else { return };
        let reapers: Vec<JoinHandle<()>> =
            inner.instances.drain().filter_map(|(_, mut inst)| inst.shutdown()).collect();
        drop(inner);
        let deadline = Instant::now() + KILL_GRACE * 4;
        while reapers.iter().any(|r| !r.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Convert an `alacritty_terminal` cell into our renderer-agnostic [`Cell`].
fn convert_cell(cell: &AlacCell) -> Cell {
    let flags = CellFlags {
        bold: cell.flags.intersects(Flags::BOLD),
        italic: cell.flags.intersects(Flags::ITALIC),
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
        inverse: cell.flags.contains(Flags::INVERSE),
    };
    Cell { ch: cell.c, fg: cell.fg.into(), bg: cell.bg.into(), flags }
}
