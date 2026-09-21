//! CPU counter configuration: pick the performance events to record, write
//! an Instruments template for them, and hand out the xctrace command.
//!
//! Instruments records the CPU counters, and only a template tells it which
//! events. A template is an NSKeyedArchiver plist whose CPU Counters
//! instrument keeps its options as a JSON blob; the event list inside is one
//! base64 keyed archive per event. A stock template has no such blob, so a
//! template the user saved once from the GUI is the prototype: this module
//! clones its event record for each chosen event and switches it to time
//! sampling. The event catalog comes from Apple's kperfdata framework, the
//! same source Instruments uses.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine as _;
use gpui::FocusHandle;

use crate::app::JadeApp;

/// At most this many programmable events fit on an Apple core.
pub const MAX_EVENTS: usize = 8;

/// One event from the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct EventInfo {
    pub name: String,
    pub description: String,
    /// Bit i set = the event can run on counter i.
    pub mask: u32,
}

/// The default set: what a cache and branch study of one process wants.
pub const DEFAULT_EVENTS: [&str; 8] = [
    "L1D_CACHE_MISS_LD",
    "L1D_CACHE_MISS_ST",
    "LD_UNIT_UOP",
    "ST_UNIT_UOP",
    "L1D_TLB_MISS",
    "L2_TLB_MISS_DATA",
    "INST_BRANCH",
    "BRANCH_MISPRED_NONSPEC",
];

/// The event catalog for this machine's cores, loaded once. Empty when the
/// framework is missing or refuses, which leaves the picker free-text.
pub fn catalog() -> &'static [EventInfo] {
    static CATALOG: OnceLock<Vec<EventInfo>> = OnceLock::new();
    CATALOG.get_or_init(|| load_catalog().unwrap_or_default())
}

#[repr(C)]
struct KpepEvent {
    name: *const std::ffi::c_char,
    description: *const std::ffi::c_char,
    errata: *const std::ffi::c_char,
    alias: *const std::ffi::c_char,
    fallback: *const std::ffi::c_char,
    mask: u32,
    number: u8,
    umask: u8,
    reserved: u8,
    is_fixed: u8,
}

fn load_catalog() -> Option<Vec<EventInfo>> {
    use std::ffi::{c_char, c_int, c_void, CStr};
    type DbCreate = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int;
    type EventsCount = unsafe extern "C" fn(*mut c_void, *mut u32) -> c_int;
    type Events = unsafe extern "C" fn(*mut c_void, *mut *mut KpepEvent, usize) -> c_int;
    // SAFETY: kperfdata is Apple's own framework; the symbols and the event
    // record layout are the ones its public header `kpep.h` declares.
    unsafe {
        let lib = libloading::Library::new("/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata").ok()?;
        let db_create: libloading::Symbol<DbCreate> = lib.get(b"kpep_db_create\0").ok()?;
        let events_count: libloading::Symbol<EventsCount> = lib.get(b"kpep_db_events_count\0").ok()?;
        let events: libloading::Symbol<Events> = lib.get(b"kpep_db_events\0").ok()?;
        let mut db: *mut c_void = std::ptr::null_mut();
        if db_create(std::ptr::null(), &mut db) != 0 || db.is_null() {
            return None;
        }
        let mut n: u32 = 0;
        if events_count(db, &mut n) != 0 || n == 0 {
            return None;
        }
        let mut ptrs: Vec<*mut KpepEvent> = vec![std::ptr::null_mut(); n as usize];
        if events(db, ptrs.as_mut_ptr(), ptrs.len() * std::mem::size_of::<*mut KpepEvent>()) != 0 {
            return None;
        }
        let text = |p: *const c_char| {
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        let mut out = Vec::with_capacity(n as usize);
        for &e in &ptrs {
            if e.is_null() {
                continue;
            }
            let ev = &*e;
            let name = text(ev.name);
            if name.is_empty() {
                continue;
            }
            out.push(EventInfo {
                name,
                description: text(ev.description),
                mask: ev.mask,
            });
        }
        // The db is process-lifetime state; kperfdata has no matching free
        // that is safe to call while the events are referenced.
        std::mem::forget(lib);
        Some(out)
    }
}

/// Catalog rows matching a query: prefix matches first, then substring, on
/// the name or the description. Case-insensitive. Selected names are left
/// out. At most `limit` rows.
pub fn search(catalog: &[EventInfo], query: &str, selected: &[String], limit: usize) -> Vec<EventInfo> {
    let q = query.trim().to_ascii_lowercase();
    let mut prefix = Vec::new();
    let mut rest = Vec::new();
    for e in catalog {
        if selected.iter().any(|s| s == &e.name) {
            continue;
        }
        let name = e.name.to_ascii_lowercase();
        if q.is_empty() || name.starts_with(&q) {
            prefix.push(e.clone());
        } else if name.contains(&q) || e.description.to_ascii_lowercase().contains(&q) {
            rest.push(e.clone());
        }
    }
    prefix.extend(rest);
    prefix.truncate(limit);
    prefix
}

/// Whether every chosen event can get its own counter. Each event names the
/// counters it may run on; a set fits when a distinct counter exists for
/// each. Unknown events count as unconstrained.
pub fn fits(catalog: &[EventInfo], selected: &[String]) -> bool {
    if selected.len() > MAX_EVENTS {
        return false;
    }
    let masks: Vec<u32> = selected
        .iter()
        .map(|s| catalog.iter().find(|e| &e.name == s).map(|e| e.mask).unwrap_or(u32::MAX))
        .collect();
    fn assign(masks: &[u32], i: usize, used: u32) -> bool {
        if i == masks.len() {
            return true;
        }
        let mut free = masks[i] & !used;
        while free != 0 {
            let bit = free & free.wrapping_neg();
            if assign(masks, i + 1, used | bit) {
                return true;
            }
            free &= !bit;
        }
        false
    }
    assign(&masks, 0, 0)
}

/// Events in the order a greedy counter allocator needs: the ones with the
/// fewest legal counters first, so a wide-mask event never takes the only
/// counter a narrow one could use. Instruments assigns in list order and
/// reports "conflicting events" otherwise. Unknown events go last. Stable.
pub fn allocation_order(catalog: &[EventInfo], events: &[String]) -> Vec<String> {
    let mut v: Vec<(u32, usize, String)> = events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mask = catalog.iter().find(|c| &c.name == e).map(|c| c.mask).unwrap_or(u32::MAX);
            (mask.count_ones(), i, e.clone())
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, _, e)| e).collect()
}

/// Instruments' per-user template folder.
pub fn templates_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Instruments/Templates"))
}

/// The newest saved template that carries a CPU Counters event list. The
/// GUI writes that list; a stock template has none.
pub fn find_prototype(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("tracetemplate"))
        .filter(|p| read_template(p).ok().and_then(|t| find_config(&t)).is_some())
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p))
        .collect();
    candidates.sort();
    candidates.pop().map(|(_, p)| p)
}

fn read_template(path: &Path) -> Result<plist::Value, String> {
    plist::Value::from_file(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Index of the `$objects` entry holding the CPU Counters JSON options.
fn find_config(template: &plist::Value) -> Option<usize> {
    let objects = template.as_dictionary()?.get("$objects")?.as_array()?;
    objects.iter().position(|v| match v {
        plist::Value::Data(d) => d.starts_with(b"{") && d.windows(20).any(|w| w == b"allEventsAndFormulas"),
        _ => false,
    })
}

/// Write a template for `events` next to the prototype's shape. Time
/// sampling on, the trigger event left in place but unused (the options
/// decoder requires the key). Returns the written path.
pub fn write_template(prototype: &Path, out: &Path, events: &[String]) -> Result<PathBuf, String> {
    if events.is_empty() {
        return Err("choose at least one event".to_string());
    }
    if events.len() > MAX_EVENTS {
        return Err(format!("at most {MAX_EVENTS} events fit"));
    }
    let mut template = read_template(prototype)?;
    let idx = find_config(&template).ok_or("the prototype template has no CPU Counters event list")?;
    let objects = template
        .as_dictionary_mut()
        .and_then(|d| d.get_mut("$objects"))
        .and_then(|v| v.as_array_mut())
        .ok_or("malformed template")?;
    let raw = match &objects[idx] {
        plist::Value::Data(d) => d.clone(),
        _ => unreachable!(),
    };
    let mut cfg: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| format!("options JSON: {e}"))?;
    let proto_blob = cfg["allEventsAndFormulas"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or("the prototype has no event record to clone")?
        .to_string();
    let engine = base64::engine::general_purpose::STANDARD;
    let proto_bytes = engine.decode(&proto_blob).map_err(|e| format!("event record: {e}"))?;
    let mut blobs = Vec::new();
    for e in &allocation_order(catalog(), events) {
        let mut record: plist::Value = plist::from_bytes(&proto_bytes).map_err(|e| format!("event record: {e}"))?;
        set_event_strings(&mut record, e)?;
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &record).map_err(|e| format!("event record: {e}"))?;
        blobs.push(serde_json::Value::String(engine.encode(bytes)));
    }
    cfg["allEventsAndFormulas"] = serde_json::Value::Array(blobs);
    cfg["sampleByTime"] = serde_json::Value::Bool(true);
    if cfg.get("pmiEventAliasOrMnemonic").is_none() {
        cfg["pmiEventAliasOrMnemonic"] = serde_json::Value::String("ARM_BR_MIS_PRED".into());
    }
    objects[idx] = plist::Value::Data(serde_json::to_vec(&cfg).map_err(|e| e.to_string())?);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    template
        .to_file_binary(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(out.to_path_buf())
}

/// An event record holds two plain strings among its archived objects: the
/// mnemonic (upper case with underscores) and the description. Replace both.
fn set_event_strings(record: &mut plist::Value, event: &str) -> Result<(), String> {
    let objects = record
        .as_dictionary_mut()
        .and_then(|d| d.get_mut("$objects"))
        .and_then(|v| v.as_array_mut())
        .ok_or("event record is not a keyed archive")?;
    let mut mnemonic_at = None;
    let mut description_at = None;
    for (i, v) in objects.iter().enumerate() {
        let Some(s) = v.as_string() else { continue };
        if s == "$null" {
            continue;
        }
        if is_mnemonic(s) && mnemonic_at.is_none() {
            mnemonic_at = Some(i);
        } else if description_at.is_none() && s.len() > 3 {
            description_at = Some(i);
        }
    }
    let m = mnemonic_at.ok_or("event record has no mnemonic")?;
    objects[m] = plist::Value::String(event.to_string());
    if let Some(d) = description_at {
        objects[d] = plist::Value::String(event.to_string());
    }
    Ok(())
}

fn is_mnemonic(s: &str) -> bool {
    s.len() > 2 && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && s.contains('_')
}

/// The recording command for the copy button. The program must outlive the
/// time limit: xctrace drops the counter stream when its target exits first.
pub fn record_command(template: &Path, output: &Path, program: Option<&Path>) -> String {
    let prog = program.map(|p| shell_quote(&p.display().to_string())).unwrap_or_else(|| "<program>".to_string());
    format!(
        "xcrun xctrace record --template {} --time-limit 6s --output {} --launch -- {}",
        shell_quote(&template.display().to_string()),
        shell_quote(&output.display().to_string()),
        prog
    )
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Picker state on the app.
#[derive(Debug, Clone, Default)]
pub struct CountersState {
    /// Chosen event mnemonics, in order.
    pub events: Vec<String>,
    /// The search box text.
    pub query: String,
    /// True while the search box holds keyboard focus.
    pub editing: bool,
    /// The last written template, if any.
    pub template: Option<PathBuf>,
    /// One line of feedback under the buttons.
    pub status: Option<String>,
}

impl CountersState {
    pub fn with_events(events: Vec<String>) -> Self {
        Self {
            events,
            ..Default::default()
        }
    }
}

impl JadeApp {
    /// The focus handle of the event search box, created on first use.
    pub fn counters_handle(&mut self, cx: &mut gpui::Context<Self>) -> FocusHandle {
        self.counters_focus.get_or_insert_with(|| cx.focus_handle()).clone()
    }

    pub fn counters_add(&mut self, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() || self.counters.events.iter().any(|e| e == &name) {
            return;
        }
        if self.counters.events.len() >= MAX_EVENTS {
            self.counters.status = Some(format!("at most {MAX_EVENTS} events fit"));
            return;
        }
        self.counters.events.push(name);
        self.counters.query.clear();
        self.counters.status = None;
        self.counters.template = None;
        self.save_ui_state();
    }

    pub fn counters_remove(&mut self, name: &str) {
        self.counters.events.retain(|e| e != name);
        self.counters.template = None;
        self.save_ui_state();
    }

    /// Apply one keystroke to the search box. Enter adds the top match, or
    /// the typed text when the catalog is empty. Esc leaves the box.
    /// Returns true when consumed.
    pub fn counters_key(&mut self, ks: &gpui::Keystroke) -> bool {
        let m = ks.modifiers;
        match ks.key.as_str() {
            "enter" => {
                let top = search(catalog(), &self.counters.query, &self.counters.events, 1)
                    .into_iter()
                    .next()
                    .map(|e| e.name)
                    .or_else(|| (catalog().is_empty() && !self.counters.query.trim().is_empty()).then(|| self.counters.query.clone()));
                if let Some(name) = top {
                    self.counters_add(&name);
                }
            }
            "escape" => {
                self.counters.editing = false;
                self.counters.query.clear();
            }
            "backspace" => {
                self.counters.query.pop();
            }
            _ => {
                let printable = ks.key_char.is_some() && !m.platform && !m.control && !m.alt && !m.function;
                if !printable {
                    return false;
                }
                self.counters.query.push_str(ks.key_char.as_deref().unwrap_or(""));
            }
        }
        true
    }

    /// Write `Jade Counters.tracetemplate` for the chosen events.
    pub fn counters_write_template(&mut self) {
        let Some(dir) = templates_dir() else {
            self.counters.status = Some("no home directory".into());
            return;
        };
        let Some(proto) = find_prototype(&dir) else {
            self.counters.status = Some("save any CPU Counters template from Instruments once; it is the prototype".into());
            return;
        };
        if !fits(catalog(), &self.counters.events) {
            self.counters.status = Some("these events cannot share the counters; drop one".into());
            return;
        }
        let out = dir.join("Jade Counters.tracetemplate");
        match write_template(&proto, &out, &self.counters.events) {
            Ok(p) => {
                self.counters.status = Some(format!("wrote {}", p.file_name().unwrap_or_default().to_string_lossy()));
                self.counters.template = Some(p);
            }
            Err(e) => self.counters.status = Some(e),
        }
    }

    /// The xctrace command for the chosen template and the last built
    /// program, for the clipboard.
    pub fn counters_command(&self) -> String {
        let template = self
            .counters
            .template
            .clone()
            .or_else(|| templates_dir().map(|d| d.join("Jade Counters.tracetemplate")))
            .unwrap_or_else(|| PathBuf::from("Jade Counters.tracetemplate"));
        let program = self.last_build.as_ref().and_then(|b| b.executable.clone());
        let stem = program
            .as_ref()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "run".to_string());
        record_command(&template, &self.workspace_root.join(format!("{stem}.trace")), program.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Vec<EventInfo> {
        vec![
            EventInfo { name: "FIXED_CYCLES".into(), description: "cycles".into(), mask: 0x001 },
            EventInfo { name: "INST_BRANCH".into(), description: "branches".into(), mask: 0x0fc },
            EventInfo { name: "BRANCH_MISPRED_NONSPEC".into(), description: "mispredicted".into(), mask: 0x0fc },
            EventInfo { name: "L1D_CACHE_MISS_LD".into(), description: "Loads that missed L1".into(), mask: 0x3fc },
            EventInfo { name: "ONLY_TWO".into(), description: "narrow".into(), mask: 0x004 },
            EventInfo { name: "ONLY_TWO_B".into(), description: "narrow too".into(), mask: 0x004 },
        ]
    }

    #[test]
    fn search_prefers_prefix_then_substring_and_skips_selected() {
        let c = cat();
        let r = search(&c, "l1", &[], 5);
        assert_eq!(r[0].name, "L1D_CACHE_MISS_LD");
        let r = search(&c, "miss", &[], 5);
        assert_eq!(r.len(), 1, "name substring; its description matches the same row");
        let r = search(&c, "narrow", &[], 5);
        assert_eq!(r.len(), 2, "description substring");
        let r = search(&c, "branch", &["INST_BRANCH".to_string()], 5);
        assert_eq!(r.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["BRANCH_MISPRED_NONSPEC"]);
        assert_eq!(search(&c, "", &[], 2).len(), 2);
    }

    #[test]
    fn fit_needs_a_distinct_counter_per_event() {
        let c = cat();
        assert!(fits(&c, &["INST_BRANCH".into(), "L1D_CACHE_MISS_LD".into()]));
        assert!(fits(&c, &["ONLY_TWO".into()]));
        assert!(!fits(&c, &["ONLY_TWO".into(), "ONLY_TWO_B".into()]), "both need counter 2");
        assert!(fits(&c, &["UNKNOWN_EVENT".into()]), "unknown events are unconstrained");
        let nine: Vec<String> = (0..9).map(|i| format!("E{i}")).collect();
        assert!(!fits(&c, &nine));
    }

    #[test]
    fn allocation_order_puts_narrow_masks_first_and_is_stable() {
        let c = cat();
        let events: Vec<String> = ["L1D_CACHE_MISS_LD", "INST_BRANCH", "ONLY_TWO", "UNKNOWN", "BRANCH_MISPRED_NONSPEC"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            allocation_order(&c, &events),
            vec!["ONLY_TWO", "INST_BRANCH", "BRANCH_MISPRED_NONSPEC", "L1D_CACHE_MISS_LD", "UNKNOWN"]
        );
    }

    #[test]
    fn record_command_quotes_paths_with_spaces() {
        let cmd = record_command(
            Path::new("/Users/x/Library/Application Support/Instruments/Templates/Jade Counters.tracetemplate"),
            Path::new("/w/ob.trace"),
            Some(Path::new("/w/build/orderbook_bench")),
        );
        assert!(cmd.starts_with("xcrun xctrace record --template '/Users/x/Library/Application Support/"));
        assert!(cmd.ends_with("--output /w/ob.trace --launch -- /w/build/orderbook_bench"));
        assert!(record_command(Path::new("t"), Path::new("o"), None).ends_with("-- <program>"));
    }

    /// A prototype shaped like a GUI-saved template: one event record whose
    /// keyed archive holds a mnemonic and a description.
    fn synthetic_prototype(dir: &Path) -> PathBuf {
        let record = plist::Value::Dictionary({
            let mut d = plist::Dictionary::new();
            d.insert("$version".into(), plist::Value::Integer(100000.into()));
            d.insert("$archiver".into(), plist::Value::String("NSKeyedArchiver".into()));
            d.insert(
                "$objects".into(),
                plist::Value::Array(vec![
                    plist::Value::String("$null".into()),
                    plist::Value::String("L1D_CACHE_MISS_LD".into()),
                    plist::Value::String("Loads that missed the L1 Data Cache".into()),
                ]),
            );
            d
        });
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &record).unwrap();
        let blob = base64::engine::general_purpose::STANDARD.encode(bytes);
        let cfg = serde_json::json!({
            "configurationType": {"manual": {}},
            "sampleByTime": false,
            "pmiEventAliasOrMnemonic": "ARM_BR_MIS_PRED",
            "pmiThreshold": 1000000,
            "allEventsAndFormulas": [blob],
        });
        let template = plist::Value::Dictionary({
            let mut d = plist::Dictionary::new();
            d.insert(
                "$objects".into(),
                plist::Value::Array(vec![
                    plist::Value::String("$null".into()),
                    plist::Value::Data(serde_json::to_vec(&cfg).unwrap()),
                ]),
            );
            d
        });
        let path = dir.join("Proto.tracetemplate");
        template.to_file_binary(&path).unwrap();
        path
    }

    #[test]
    fn write_template_clones_one_record_per_event_with_time_sampling() {
        let dir = std::env::temp_dir().join(format!("jade-counters-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proto = synthetic_prototype(&dir);
        assert_eq!(find_prototype(&dir).as_deref(), Some(proto.as_path()));
        let out = dir.join("Out.tracetemplate");
        let events = vec!["INST_BRANCH".to_string(), "BRANCH_MISPRED_NONSPEC".to_string()];
        write_template(&proto, &out, &events).unwrap();

        let written = plist::Value::from_file(&out).unwrap();
        let idx = find_config(&written).unwrap();
        let raw = written.as_dictionary().unwrap()["$objects"].as_array().unwrap()[idx].as_data().unwrap().to_vec();
        let cfg: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(cfg["sampleByTime"], true);
        assert_eq!(cfg["pmiEventAliasOrMnemonic"], "ARM_BR_MIS_PRED");
        let blobs = cfg["allEventsAndFormulas"].as_array().unwrap();
        assert_eq!(blobs.len(), 2);
        let second: plist::Value =
            plist::from_bytes(&base64::engine::general_purpose::STANDARD.decode(blobs[1].as_str().unwrap()).unwrap()).unwrap();
        let strings: Vec<String> = second.as_dictionary().unwrap()["$objects"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_string().map(|s| s.to_string()))
            .collect();
        assert_eq!(strings, vec!["$null", "BRANCH_MISPRED_NONSPEC", "BRANCH_MISPRED_NONSPEC"]);

        assert!(write_template(&proto, &out, &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_loads_on_apple_silicon_or_is_empty() {
        // Either the framework answered with real events, or the picker
        // degrades to free text. Both are valid outcomes of this call.
        let c = catalog();
        if !c.is_empty() {
            assert!(c.iter().any(|e| e.name == "FIXED_CYCLES"), "{} events, no FIXED_CYCLES", c.len());
        }
    }
}

#[cfg(test)]
mod real_template_tests {
    use super::*;

    /// Writes a template from the newest GUI-saved prototype into
    /// `$JADE_TEMPLATE_OUT`. Run: `JADE_TEMPLATE_OUT=/tmp/x.tracetemplate
    /// cargo test -p jade write_real -- --ignored`.
    #[test]
    #[ignore]
    fn write_real_template() {
        let out = PathBuf::from(std::env::var("JADE_TEMPLATE_OUT").expect("JADE_TEMPLATE_OUT"));
        let proto = find_prototype(&templates_dir().unwrap()).expect("a GUI-saved CPU Counters template");
        let events: Vec<String> = DEFAULT_EVENTS.iter().map(|s| s.to_string()).collect();
        assert!(fits(catalog(), &events));
        let p = write_template(&proto, &out, &events).unwrap();
        eprintln!("prototype {} -> {}", proto.display(), p.display());
    }
}
