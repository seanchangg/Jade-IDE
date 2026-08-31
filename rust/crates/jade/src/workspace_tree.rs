//! Workspace file-tree model (feature inventory §5.1, deliverable §1).
//!
//! Pure logic + filesystem scan, no GPUI. Mirrors the Electron
//! `src/main/workspace.ts` ignore rules and sort order exactly:
//!   - ignored dirs: `node_modules`, `.git/.svn/.hg`, build dirs incl.
//!     `cmake-build-jade`, caches, IDE dirs (`workspace.ts:6-10`);
//!   - ignored file extensions: `.o .obj .a .lib .so .dylib .exe .out .class
//!     .pyc` (`workspace.ts:12-15`);
//!   - dotfiles hidden except `.jade`; `*.dSYM` hidden; root-level extensionless
//!     files hidden (compiled-binary heuristic, `workspace.ts:91-94`);
//!   - directories first, then files grouped by file kind ([`FileKind::rank`])
//!     and case-insensitive alphabetical inside each group.
//!
//! Lazy loading: the initial scan materializes **3 levels**; expanding an
//! unloaded directory loads **1 more level** (§5.1). A directory whose children
//! are `None` is an unloaded placeholder.
//!
//! FS watching (Phase-4 wave 2, §5.1): the app watches the workspace root
//! recursively (the `notify` crate, wired in `main.rs`), filters events through
//! [`is_watch_relevant`] (the same ignore rules), and debounces bursts 250ms via
//! [`WatchDebounce`] before a single tree refresh. [`FileTree::refresh`] re-scans
//! while preserving the set of expanded directories by path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Directory names never shown (`workspace.ts:6-10`).
pub const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    ".DS_Store",
    "build",
    "cmake-build-debug",
    "cmake-build-release",
    "cmake-build-jade",
    ".cache",
    "__pycache__",
    ".vscode",
    ".idea",
];

/// File extensions never shown (`workspace.ts:12-15`), compared with the leading
/// dot as in the TS `path.extname` check.
pub const IGNORED_EXTENSIONS: &[&str] = &[
    ".o", ".obj", ".a", ".lib", ".so", ".dylib", ".exe", ".out", ".class", ".pyc",
];

/// Initial scan depth (levels of entries materialized eagerly).
pub const INITIAL_DEPTH: usize = 3;

/// File-type classification (deliverable §1). It drives two things in the panel:
/// the per-kind icon and color (§5.1), and the sort order inside a directory —
/// files group by kind ([`FileKind::rank`]) before they sort by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// `.c .cc .cpp .cxx .m .mm` — a compiled translation unit.
    Source,
    /// `.h .hh .hpp .hxx .inl` — a declaration.
    Header,
    /// `.metal .cu .cl .wgsl .glsl .hlsl .vert .frag .comp` — a GPU kernel.
    Shader,
    /// `.v .sv .svh .vh .vhd .vhdl` — a hardware description (hardware mode).
    Hdl,
    /// `.rs .py .js .ts .go .java .swift .kt .rb .lua .cs .sql` — other code.
    Script,
    /// `.sh .bash .zsh .fish .command` — a shell script.
    Shell,
    /// `Makefile`, `CMakeLists.txt`, `.cmake .mk .ninja`, `meson.build`.
    Build,
    /// `.toml .yaml .yml .ini .cfg .conf .plist .xml .entitlements .env`.
    Config,
    /// `.json .jsonl .ndjson .proto` — structured data.
    Data,
    /// `.csv .tsv .parquet .arrow .xlsx` — tabular data.
    Table,
    /// `.gguf .safetensors .pt .pth .ckpt .onnx .npy .npz .bin` — model weights.
    Model,
    /// `.md .markdown .rst .txt .log .pdf .tex` — prose.
    Doc,
    /// `.png .jpg .jpeg .gif .svg .webp .bmp .ico .tiff` — an image.
    Image,
    /// `.zip .tar .gz .tgz .bz2 .xz .7z .whl .dmg` — an archive.
    Archive,
    /// `*.lock`, `package-lock.json` — a pinned dependency set.
    Lock,
    /// Everything else.
    Other,
}

impl FileKind {
    /// Sort weight inside one directory: code first, then build and config, then
    /// data, then documents and assets. Files of one kind stay together and sort
    /// by name inside the group.
    pub fn rank(self) -> u8 {
        match self {
            FileKind::Source => 0,
            FileKind::Header => 1,
            FileKind::Hdl => 2,
            FileKind::Shader => 3,
            FileKind::Script => 4,
            FileKind::Shell => 5,
            FileKind::Build => 6,
            FileKind::Config => 7,
            FileKind::Data => 8,
            FileKind::Table => 9,
            FileKind::Model => 10,
            FileKind::Doc => 11,
            FileKind::Image => 12,
            FileKind::Archive => 13,
            FileKind::Lock => 14,
            FileKind::Other => 15,
        }
    }
}

/// Classify a file name (deliverable §1). Well-known whole names (`Makefile`,
/// `CMakeLists.txt`, `*.lock`) win over the extension; otherwise the last
/// extension decides.
pub fn classify(name: &str) -> FileKind {
    let lower = name.to_ascii_lowercase();

    // Whole-name matches first: these carry a meaning the extension does not.
    match lower.as_str() {
        "makefile" | "gnumakefile" | "cmakelists.txt" | "meson.build" | "dockerfile"
        | "build.ninja" | "justfile" | "rakefile" => return FileKind::Build,
        "package-lock.json" | "yarn.lock" | "cargo.lock" | "poetry.lock" | "uv.lock" => {
            return FileKind::Lock
        }
        "license" | "readme" | "notice" | "changelog" | "authors" => return FileKind::Doc,
        _ => {}
    }

    let ext = lower.rsplit('.').next().filter(|_| lower.contains('.'));
    match ext {
        Some("c" | "cc" | "cpp" | "cxx" | "c++" | "m" | "mm") => FileKind::Source,
        Some("h" | "hh" | "hpp" | "hxx" | "h++" | "inl" | "ipp") => FileKind::Header,
        Some("metal" | "cu" | "cuh" | "cl" | "wgsl" | "glsl" | "hlsl" | "vert" | "frag"
        | "comp" | "spv") => FileKind::Shader,
        // Hardware sources (hardware mode). Quartus settings files rank as
        // Config, bitstreams as Data (below).
        Some("v" | "sv" | "svh" | "vh" | "vhd" | "vhdl") => FileKind::Hdl,
        Some(
            "rs" | "py" | "pyi" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "java"
            | "swift" | "kt" | "rb" | "lua" | "cs" | "php" | "pl" | "r" | "jl" | "zig"
            | "sql" | "ipynb",
        ) => FileKind::Script,
        Some("sh" | "bash" | "zsh" | "fish" | "command" | "bat" | "ps1") => FileKind::Shell,
        Some("cmake" | "mk" | "ninja" | "mak" | "gradle" | "bazel" | "bzl") => FileKind::Build,
        Some(
            "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "plist" | "xml" | "properties"
            | "entitlements" | "env" | "editorconfig" | "gitignore" | "gitattributes"
            | "qsf" | "sdc" | "qpf",
        ) => FileKind::Config,
        Some(
            "json" | "jsonc" | "jsonl" | "ndjson" | "proto" | "graphql" | "sof" | "pof"
            | "svf" | "rbf",
        ) => FileKind::Data,
        Some("csv" | "tsv" | "parquet" | "arrow" | "xlsx" | "db" | "sqlite" | "sqlite3") => {
            FileKind::Table
        }
        Some(
            "gguf" | "ggml" | "safetensors" | "pt" | "pth" | "ckpt" | "onnx" | "npy" | "npz"
            | "bin" | "pb" | "tflite" | "mlmodel" | "mlpackage",
        ) => FileKind::Model,
        Some("md" | "markdown" | "rst" | "txt" | "log" | "pdf" | "tex" | "adoc") => FileKind::Doc,
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" | "tif" | "tiff"
        | "icns") => FileKind::Image,
        Some("zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "whl" | "dmg" | "pkg") => {
            FileKind::Archive
        }
        Some("lock") => FileKind::Lock,
        _ => FileKind::Other,
    }
}

/// One node in the tree. `children == None` on a directory means "not yet loaded"
/// (a lazy placeholder); `Some(vec)` means loaded (possibly empty). Files always
/// have `children == None` and `is_dir == false`.
#[derive(Debug, Clone)]
pub struct FileNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub kind: FileKind,
    pub children: Option<Vec<FileNode>>,
    pub expanded: bool,
}

impl FileNode {
    /// True for a directory whose children have not been read from disk yet.
    pub fn is_unloaded_dir(&self) -> bool {
        self.is_dir && self.children.is_none()
    }

    /// Ensure a directory's immediate children are loaded (1 level; grandchildren
    /// stay as unloaded placeholders). No-op for files or already-loaded dirs.
    pub fn ensure_loaded(&mut self) {
        if self.is_dir && self.children.is_none() {
            self.children = Some(scan_dir(&self.path, false, 1));
        }
    }
}

/// The scanned workspace tree rooted at `root`.
#[derive(Debug, Clone)]
pub struct FileTree {
    pub root: PathBuf,
    pub roots: Vec<FileNode>,
}

/// A flattened, indentation-annotated row for rendering (deliverable §2).
#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub kind: FileKind,
    /// Nesting depth (root entries = 0); the panel indents `12 + depth*16` px.
    pub depth: usize,
    pub expanded: bool,
}

impl FileTree {
    /// Scan `root` to the initial depth (§5.1). Returns an empty tree on IO error.
    pub fn scan(root: impl Into<PathBuf>) -> FileTree {
        let root = root.into();
        let roots = scan_dir(&root, true, INITIAL_DEPTH);
        FileTree { root, roots }
    }

    /// Scan `root` fully (all levels materialized), applying the same ignore
    /// rules as [`scan`](Self::scan). Used to build the Quick Open file cache
    /// (§5.7), which needs every openable file, not just the lazily-loaded
    /// initial 3 levels.
    pub fn scan_full(root: impl Into<PathBuf>) -> FileTree {
        let root = root.into();
        let roots = scan_dir(&root, true, usize::MAX);
        FileTree { root, roots }
    }

    /// Flatten the currently-visible tree (only descending into expanded dirs)
    /// into rows with depths for rendering.
    pub fn visible_rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        flatten(&self.roots, 0, &mut out);
        out
    }

    /// Toggle a directory (by path): flip `expanded`, loading children on first
    /// expansion. Returns true if a matching directory was found and toggled.
    pub fn toggle_dir(&mut self, path: &Path) -> bool {
        toggle_in(&mut self.roots, path)
    }

    /// The set of currently-expanded directory paths (Phase-4 wave 2: captured
    /// before an fs-watch refresh so expansion survives a re-scan).
    pub fn expanded_paths(&self) -> HashSet<PathBuf> {
        let mut set = HashSet::new();
        collect_expanded(&self.roots, &mut set);
        set
    }

    /// Re-scan the root (to the initial depth) and re-apply the previously
    /// expanded directories, loading beyond the initial depth as needed so a
    /// deeply-expanded subtree is restored. Used by the debounced fs-watch
    /// refresh (§5.1). Directories that vanished simply drop out; new ones appear
    /// collapsed. Open editor tabs are intentionally left untouched (a deleted
    /// file keeps its tab — the read-only viewer already holds its contents).
    pub fn refresh(&mut self) {
        let expanded = self.expanded_paths();
        self.roots = scan_dir(&self.root, true, INITIAL_DEPTH);
        // Apply shallow-first so an ancestor is loaded before its descendants.
        let mut paths: Vec<PathBuf> = expanded.into_iter().collect();
        paths.sort_by_key(|p| p.components().count());
        for p in paths {
            reexpand(&mut self.roots, &p);
        }
    }
}

fn collect_expanded(nodes: &[FileNode], out: &mut HashSet<PathBuf>) {
    for n in nodes {
        if n.is_dir && n.expanded {
            out.insert(n.path.clone());
        }
        if let Some(children) = &n.children {
            collect_expanded(children, out);
        }
    }
}

/// Descend toward `path`, lazily loading directories along the way, and mark the
/// target directory expanded. Returns true if `path` was found.
fn reexpand(nodes: &mut [FileNode], path: &Path) -> bool {
    for n in nodes.iter_mut() {
        if n.path == path {
            if n.is_dir {
                n.ensure_loaded();
                n.expanded = true;
            }
            return true;
        }
        if n.is_dir && path.starts_with(&n.path) {
            n.ensure_loaded();
            if let Some(children) = &mut n.children {
                if reexpand(children, path) {
                    return true;
                }
            }
        }
    }
    false
}

/// True if a changed path is one the tree would display — i.e. no path component
/// is an ignored directory / hidden dotfile / `.dSYM` bundle, and the leaf's
/// extension is not an ignored build-artifact type. Used to drop the FSEvents
/// bursts that build artifacts (`.o`, `cmake-build-jade/…`) generate (§5.1).
pub fn is_watch_relevant(path: &Path) -> bool {
    let mut comps = path.components().peekable();
    while let Some(comp) = comps.next() {
        let os = comp.as_os_str();
        let name = os.to_string_lossy();
        let is_leaf = comps.peek().is_none();
        // A path component that is a normal name (skip `/`, `.`, `..`).
        if matches!(
            comp,
            std::path::Component::RootDir
                | std::path::Component::CurDir
                | std::path::Component::ParentDir
                | std::path::Component::Prefix(_)
        ) {
            continue;
        }
        if IGNORED_DIRS.contains(&name.as_ref()) {
            return false;
        }
        if name.starts_with('.') && name != ".jade" {
            return false;
        }
        if name.ends_with(".dSYM") {
            return false;
        }
        if is_leaf {
            if let Some(dot) = name.rfind('.') {
                let ext = name[dot..].to_ascii_lowercase();
                if IGNORED_EXTENSIONS.contains(&ext.as_str()) {
                    return false;
                }
            }
        }
    }
    true
}

/// Debounce state for coalescing fs-watch bursts (the pure part of the 250ms
/// refresh, §5.1). Operates on a monotonic millisecond clock passed in by the
/// caller so it is deterministic to test; the async wiring supplies
/// `Instant::elapsed`-derived millis.
#[derive(Debug, Clone)]
pub struct WatchDebounce {
    window_ms: u64,
    pending: bool,
    last_event_ms: u64,
}

impl WatchDebounce {
    pub fn new(window_ms: u64) -> Self {
        WatchDebounce {
            window_ms,
            pending: false,
            last_event_ms: 0,
        }
    }

    /// Record a relevant change at `now_ms`; arms/extends the debounce window.
    pub fn on_event(&mut self, now_ms: u64) {
        self.pending = true;
        self.last_event_ms = now_ms;
    }

    /// If a refresh is due (armed and the window has elapsed since the last
    /// event), consume the pending flag and return true. Otherwise false.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if self.pending && now_ms.saturating_sub(self.last_event_ms) >= self.window_ms {
            self.pending = false;
            true
        } else {
            false
        }
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }
}

fn flatten(nodes: &[FileNode], depth: usize, out: &mut Vec<Row>) {
    for n in nodes {
        out.push(Row {
            name: n.name.clone(),
            path: n.path.clone(),
            is_dir: n.is_dir,
            kind: n.kind,
            depth,
            expanded: n.expanded,
        });
        if n.is_dir && n.expanded {
            if let Some(children) = &n.children {
                flatten(children, depth + 1, out);
            }
        }
    }
}

fn toggle_in(nodes: &mut [FileNode], path: &Path) -> bool {
    for n in nodes.iter_mut() {
        if n.path == path {
            if n.is_dir {
                n.ensure_loaded();
                n.expanded = !n.expanded;
            }
            return true;
        }
        if n.is_dir {
            if let Some(children) = &mut n.children {
                if toggle_in(children, path) {
                    return true;
                }
            }
        }
    }
    false
}

/// Scan one directory. `is_root` enables the root-level extensionless-file rule;
/// `load_levels` is how many levels of entries to materialize (a subdir gets its
/// own children loaded only while `load_levels > 1`, else it's a placeholder).
fn scan_dir(dir: &Path, is_root: bool, load_levels: usize) -> Vec<FileNode> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    // Collect (name, is_dir, path, kind).
    let mut entries: Vec<(String, bool, PathBuf, FileKind)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let kind = if is_dir { FileKind::Other } else { classify(&name) };
        entries.push((name, is_dir, entry.path(), kind));
    }

    // Dirs first; then files group by kind (sources, headers, shaders, … ) and
    // sort case-insensitively by name inside each group.
    entries.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a
            .3
            .rank()
            .cmp(&b.3.rank())
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            .then_with(|| a.0.cmp(&b.0)),
    });

    let mut nodes = Vec::new();
    for (name, is_dir, path, kind) in entries {
        if !keep(&name, is_dir, is_root) {
            continue;
        }
        let children = if is_dir && load_levels > 1 {
            Some(scan_dir(&path, false, load_levels - 1))
        } else {
            None // file, or unloaded dir placeholder
        };
        nodes.push(FileNode {
            kind,
            name,
            path,
            is_dir,
            children,
            expanded: false,
        });
    }
    nodes
}

/// Apply the visibility rules (`workspace.ts:90-111`). `is_root` gates the
/// extensionless-file heuristic to the top level only.
fn keep(name: &str, is_dir: bool, is_root: bool) -> bool {
    if is_dir && IGNORED_DIRS.contains(&name) {
        return false;
    }
    // Dotfiles hidden except `.jade`.
    if name.starts_with('.') && name != ".jade" {
        return false;
    }
    // `*.dSYM` bundles hidden.
    if name.ends_with(".dSYM") {
        return false;
    }
    // Root-level extensionless files: likely compiled binaries.
    if is_root && !is_dir && !name.contains('.') {
        return false;
    }
    // Ignored build-artifact extensions (files only).
    if !is_dir {
        if let Some(dot) = name.rfind('.') {
            let ext = name[dot..].to_ascii_lowercase();
            if IGNORED_EXTENSIONS.contains(&ext.as_str()) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_fixture() -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "jade-wt-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        // Kept files.
        fs::write(base.join("main.cpp"), "int main(){}").unwrap();
        fs::write(base.join("util.h"), "#pragma once").unwrap();
        fs::write(base.join("notes.txt"), "hi").unwrap();
        // Ignored: build-artifact ext, dotfile, extensionless root binary.
        fs::write(base.join("main.o"), "").unwrap();
        fs::write(base.join(".hidden"), "").unwrap();
        fs::write(base.join("a.out"), "").unwrap();
        fs::write(base.join("compiled_binary"), "").unwrap(); // extensionless @ root
        // `.jade` dotdir is the one dotfile exception.
        fs::create_dir_all(base.join(".jade")).unwrap();
        fs::write(base.join(".jade").join("workspace.json"), "{}").unwrap();
        // Ignored dirs.
        fs::create_dir_all(base.join("node_modules")).unwrap();
        fs::create_dir_all(base.join("cmake-build-jade")).unwrap();
        fs::create_dir_all(base.join("app.dSYM")).unwrap();
        // A real nested source dir.
        fs::create_dir_all(base.join("src").join("engine")).unwrap();
        fs::write(base.join("src").join("core.cc"), "").unwrap();
        fs::write(base.join("src").join("engine").join("gpu.metal"), "").unwrap();

        TempDir(base)
    }

    #[test]
    fn classify_extensions() {
        assert_eq!(classify("a.cpp"), FileKind::Source);
        assert_eq!(classify("a.mm"), FileKind::Source);
        assert_eq!(classify("a.c"), FileKind::Source);
        assert_eq!(classify("a.h"), FileKind::Header);
        assert_eq!(classify("a.hpp"), FileKind::Header);
        assert_eq!(classify("gpu.metal"), FileKind::Shader);
        assert_eq!(classify("train.py"), FileKind::Script);
        assert_eq!(classify("run.sh"), FileKind::Shell);
        assert_eq!(classify("ai.json"), FileKind::Data);
        assert_eq!(classify("Cargo.toml"), FileKind::Config);
        assert_eq!(classify("model.gguf"), FileKind::Model);
        assert_eq!(classify("loss.csv"), FileKind::Table);
        assert_eq!(classify("logo.png"), FileKind::Image);
        assert_eq!(classify("README.md"), FileKind::Doc);
        assert_eq!(classify("Makefile"), FileKind::Build);
        assert_eq!(classify("CMakeLists.txt"), FileKind::Build);
        assert_eq!(classify("Cargo.lock"), FileKind::Lock);
        assert_eq!(classify("notes"), FileKind::Other);
    }

    /// The whole-name rules must beat the extension: `CMakeLists.txt` is a build
    /// file, not a `.txt` document, and `package-lock.json` is a lock file.
    #[test]
    fn whole_name_beats_extension() {
        assert_ne!(classify("CMakeLists.txt"), FileKind::Doc);
        assert_ne!(classify("package-lock.json"), FileKind::Data);
        assert_eq!(classify("package-lock.json"), FileKind::Lock);
    }

    /// Inside one directory, files group by kind before they sort by name.
    #[test]
    fn files_group_by_kind() {
        let dir = temp_fixture();
        let tree = FileTree::scan(dir.0.clone());
        let files: Vec<&str> = tree
            .roots
            .iter()
            .filter(|n| !n.is_dir)
            .map(|n| n.name.as_str())
            .collect();
        // main.cpp (Source) < util.h (Header) < notes.txt (Doc), even though the
        // plain alphabetical order would be main.cpp, notes.txt, util.h.
        assert_eq!(files, vec!["main.cpp", "util.h", "notes.txt"]);
    }

    #[test]
    fn scan_applies_ignore_rules() {
        let dir = temp_fixture();
        let tree = FileTree::scan(dir.0.clone());
        let names: Vec<&str> = tree.roots.iter().map(|n| n.name.as_str()).collect();

        // Kept.
        assert!(names.contains(&"main.cpp"));
        assert!(names.contains(&"util.h"));
        assert!(names.contains(&"notes.txt"));
        assert!(names.contains(&"src"));
        assert!(names.contains(&".jade")); // dotfile exception

        // Ignored.
        assert!(!names.contains(&"main.o"), "build artifact ext");
        assert!(!names.contains(&".hidden"), "dotfile");
        assert!(!names.contains(&"a.out"), "ignored ext");
        assert!(!names.contains(&"compiled_binary"), "root extensionless");
        assert!(!names.contains(&"node_modules"));
        assert!(!names.contains(&"cmake-build-jade"));
        assert!(!names.contains(&"app.dSYM"));
    }

    #[test]
    fn dirs_first_then_alphabetical() {
        let dir = temp_fixture();
        let tree = FileTree::scan(dir.0.clone());
        // First non-dot dir should precede all files. `.jade` and `src` are dirs.
        let first_file = tree.roots.iter().position(|n| !n.is_dir).unwrap();
        let last_dir = tree.roots.iter().rposition(|n| n.is_dir).unwrap();
        assert!(last_dir < first_file, "all dirs come before all files");
    }

    #[test]
    fn initial_scan_loads_three_levels() {
        let dir = temp_fixture();
        let tree = FileTree::scan(dir.0.clone());
        let src = tree.roots.iter().find(|n| n.name == "src").unwrap();
        // Level 1 (src children) loaded.
        assert!(src.children.is_some());
        let engine = src
            .children
            .as_ref()
            .unwrap()
            .iter()
            .find(|n| n.name == "engine")
            .unwrap();
        // Level 2 (engine children) loaded within the 3-level budget.
        assert!(engine.children.is_some());
        assert!(engine
            .children
            .as_ref()
            .unwrap()
            .iter()
            .any(|n| n.name == "gpu.metal"));
    }

    #[test]
    fn lazy_expand_loads_one_more_level() {
        let dir = temp_fixture();
        // Build a deep structure beyond the initial 3 levels.
        let deep = dir.0.join("l1").join("l2").join("l3").join("l4");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.cpp"), "").unwrap();

        let mut tree = FileTree::scan(dir.0.clone());
        // Walk to l3 (an unloaded placeholder at the 3-level boundary).
        // l1(0) -> l2(1) -> l3(2): l3's children should be None initially.
        let l1 = tree.roots.iter().find(|n| n.name == "l1").unwrap();
        let l2 = l1.children.as_ref().unwrap().iter().find(|n| n.name == "l2").unwrap();
        let l3 = l2.children.as_ref().unwrap().iter().find(|n| n.name == "l3").unwrap();
        assert!(l3.is_unloaded_dir(), "l3 is a lazy placeholder at depth 3");
        let l3_path = l3.path.clone();

        // Expand l3 → loads its children (l4), whose own children stay unloaded.
        assert!(tree.toggle_dir(&l3_path));
        let l1 = tree.roots.iter().find(|n| n.name == "l1").unwrap();
        let l2 = l1.children.as_ref().unwrap().iter().find(|n| n.name == "l2").unwrap();
        let l3 = l2.children.as_ref().unwrap().iter().find(|n| n.name == "l3").unwrap();
        assert!(l3.expanded);
        let l4 = l3.children.as_ref().unwrap().iter().find(|n| n.name == "l4").unwrap();
        assert!(l4.is_unloaded_dir(), "l4 loaded but its children are still lazy");
    }

    #[test]
    fn visible_rows_track_expansion() {
        let dir = temp_fixture();
        let mut tree = FileTree::scan(dir.0.clone());
        let src_path = tree.roots.iter().find(|n| n.name == "src").unwrap().path.clone();

        // Collapsed: src present, its children not.
        let rows = tree.visible_rows();
        assert!(rows.iter().any(|r| r.name == "src" && r.depth == 0));
        assert!(!rows.iter().any(|r| r.name == "core.cc"));

        // Expand src → its children appear at depth 1.
        tree.toggle_dir(&src_path);
        let rows = tree.visible_rows();
        assert!(rows.iter().any(|r| r.name == "core.cc" && r.depth == 1));
    }

    #[test]
    fn watch_relevance_filters_ignored_paths() {
        let root = Path::new("/proj");
        // Kept.
        assert!(is_watch_relevant(&root.join("src/main.cpp")));
        assert!(is_watch_relevant(&root.join("util.h")));
        assert!(is_watch_relevant(&root.join(".jade/workspace.json"))); // exception
        // Dropped: ignored dir component, build-artifact ext, dotfile, dSYM.
        assert!(!is_watch_relevant(&root.join("cmake-build-jade/out.o")));
        assert!(!is_watch_relevant(&root.join("node_modules/x/index.js")));
        assert!(!is_watch_relevant(&root.join("src/main.o")));
        assert!(!is_watch_relevant(&root.join(".git/HEAD")));
        assert!(!is_watch_relevant(&root.join("app.dSYM/Contents")));
        assert!(!is_watch_relevant(&root.join("lib.dylib")));
    }

    #[test]
    fn debounce_coalesces_and_fires_after_window() {
        let mut d = WatchDebounce::new(250);
        assert!(!d.is_pending());
        assert!(!d.poll(0), "nothing pending → no fire");

        // A burst of events at t=0,100,200 keeps extending the window.
        d.on_event(0);
        assert!(d.is_pending());
        assert!(!d.poll(100), "only 100ms since last event");
        d.on_event(100);
        d.on_event(200);
        assert!(!d.poll(300), "only 100ms since the last (t=200) event");

        // 250ms after the final event → fires exactly once, then disarms.
        assert!(d.poll(450), "250ms elapsed since t=200");
        assert!(!d.is_pending());
        assert!(!d.poll(999), "already consumed");

        // A fresh event re-arms.
        d.on_event(1000);
        assert!(d.poll(1300));
    }

    #[test]
    fn refresh_preserves_expansion_and_picks_up_new_files() {
        let dir = temp_fixture();
        // Build a subtree beyond the initial depth and expand into it.
        let deep = dir.0.join("l1").join("l2").join("l3");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("a.cpp"), "").unwrap();

        let mut tree = FileTree::scan(dir.0.clone());
        let src_path = tree.roots.iter().find(|n| n.name == "src").unwrap().path.clone();
        let l3_path = deep.clone();
        tree.toggle_dir(&src_path); // expand src
        tree.toggle_dir(&dir.0.join("l1")); // expand the chain down to the
        tree.toggle_dir(&dir.0.join("l1").join("l2")); // deep (initially
        tree.toggle_dir(&l3_path); // unloaded) directory

        let expanded_before = tree.expanded_paths();
        assert!(expanded_before.contains(&src_path));
        assert!(expanded_before.contains(&l3_path));

        // Simulate an fs change: a new file appears under src.
        fs::write(dir.0.join("src").join("new.cpp"), "").unwrap();
        tree.refresh();

        // Expansion survived the re-scan.
        let expanded_after = tree.expanded_paths();
        assert!(expanded_after.contains(&src_path), "src still expanded");
        assert!(expanded_after.contains(&l3_path), "deep dir still expanded");

        // The new file is now visible under the (still-expanded) src.
        let rows = tree.visible_rows();
        assert!(rows.iter().any(|r| r.name == "new.cpp" && r.depth == 1));
        // And the deep file is still reachable (subtree reloaded).
        assert!(rows.iter().any(|r| r.name == "a.cpp"));
    }
}
