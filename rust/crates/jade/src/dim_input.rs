//! Numeric rows×cols field editing state — the tensor shape hint / dim-cap
//! editors ported from `weight-grid-3d.ts`'s `makeDimInput`/`syncDimInputs`/
//! `applyShape` (lines 250-287, 455-482) and `telemetry-panel.ts`'s
//! `toggleShapeEditor`/`apply` (lines 496-559). Both surfaces only ever edit a
//! **buffer's** shape (`telemetryRegistry.setShape('buffer', ...)` hardcodes the
//! kind in the TS too), so this state is buffer-name-scoped, not `Kind`-generic.
//!
//! Pure logic, no GPUI: a captured-keystroke buffer in the same style as
//! [`crate::quick_open::QuickOpenState`] — this app has no full text-input
//! widget, so digits are appended/popped one keystroke at a time and the caller
//! (`app.rs` + the two render sites) paints the live buffer plus a caret.

/// Which of the two fields currently has the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimField {
    Rows,
    Cols,
}

/// A live editing session for one buffer's rows×cols hint. `Some` on
/// `JadeApp::dim_edit` means an editor — in the wg3d toolbar OR the telemetry
/// sidebar's inline row editor, never both — is open for `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DimEditState {
    pub name: String,
    pub rows: String,
    pub cols: String,
    pub field: DimField,
}

/// What the caller should do after a keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimKeyAction {
    /// Re-render; nothing else to do (typed/backspaced/switched field, or an
    /// Enter that didn't parse — matches `applyShape`'s early return on
    /// invalid input: silently ignored, editor stays open).
    None,
    /// Escape: discard the in-progress text and close the editor.
    Cancel,
    /// Enter with both fields parsing to ints ≥ 1: apply the hint.
    Commit(u32, u32),
}

impl DimEditState {
    /// Start a session, prefilled from the stored hint (`init_rows`/`init_cols`,
    /// both empty when there is none — the placeholder text for an empty field
    /// is supplied separately by the caller at render time, per
    /// `syncDimInputs`'s placeholder fallback to the latest frame's source dims).
    pub fn new(
        name: impl Into<String>,
        init_rows: String,
        init_cols: String,
        field: DimField,
    ) -> Self {
        DimEditState {
            name: name.into(),
            rows: init_rows,
            cols: init_cols,
            field,
        }
    }

    fn current_mut(&mut self) -> &mut String {
        match self.field {
            DimField::Rows => &mut self.rows,
            DimField::Cols => &mut self.cols,
        }
    }

    /// Parse both fields; `Some((rows, cols))` only when both are ints ≥ 1
    /// (`applyShape`'s `Number.isFinite(rows) && ... && rows < 1` guard).
    pub fn try_commit(&self) -> Option<(u32, u32)> {
        let rows = self.rows.trim().parse::<u32>().ok()?;
        let cols = self.cols.trim().parse::<u32>().ok()?;
        if rows >= 1 && cols >= 1 {
            Some((rows, cols))
        } else {
            None
        }
    }

    /// Apply one captured keystroke. `key` is the GPUI `Keystroke::key`;
    /// `key_char` the printable character if any; `printable` is true when no
    /// command/control/alt/function modifier is set (mirrors
    /// `quick_open::QuickOpenState::on_key`'s contract). Only ASCII digits are
    /// ever appended — the fields are integer rows/cols, no decimals/signs.
    pub fn on_key(&mut self, key: &str, key_char: Option<&str>, printable: bool) -> DimKeyAction {
        match key {
            "escape" => DimKeyAction::Cancel,
            "enter" | "return" => match self.try_commit() {
                Some((r, c)) => DimKeyAction::Commit(r, c),
                None => DimKeyAction::None,
            },
            "tab" => {
                self.field = match self.field {
                    DimField::Rows => DimField::Cols,
                    DimField::Cols => DimField::Rows,
                };
                DimKeyAction::None
            }
            "backspace" => {
                self.current_mut().pop();
                DimKeyAction::None
            }
            _ => {
                if printable {
                    if let Some(ch) = key_char {
                        let mut chars = ch.chars();
                        if let (Some(d), None) = (chars.next(), chars.next()) {
                            if d.is_ascii_digit() {
                                self.current_mut().push(d);
                            }
                        }
                    }
                }
                DimKeyAction::None
            }
        }
    }
}

/// Resolve what a field should display: the live text, or — when empty — the
/// placeholder (and whether the result IS a placeholder, for muted styling),
/// mirroring `syncDimInputs`'s `input.value = ''; input.placeholder = ...`.
pub fn display_value(text: &str, placeholder: &str) -> (String, bool) {
    if text.is_empty() {
        (placeholder.to_string(), true)
    } else {
        (text.to_string(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_append_letters_ignored() {
        let mut s = DimEditState::new("W", String::new(), String::new(), DimField::Rows);
        for ch in ["2", "5", "x", "6"] {
            s.on_key(ch, Some(ch), true);
        }
        assert_eq!(s.rows, "256"); // 'x' rejected
        assert_eq!(s.cols, "");
    }

    #[test]
    fn backspace_pops_active_field() {
        let mut s = DimEditState::new("W", "12".into(), "34".into(), DimField::Cols);
        s.on_key("backspace", None, false);
        assert_eq!(s.cols, "3");
        assert_eq!(s.rows, "12"); // untouched
    }

    #[test]
    fn tab_switches_active_field() {
        let mut s = DimEditState::new("W", String::new(), String::new(), DimField::Rows);
        assert_eq!(s.on_key("tab", None, false), DimKeyAction::None);
        assert_eq!(s.field, DimField::Cols);
        s.on_key("7", Some("7"), true);
        assert_eq!(s.cols, "7");
        assert_eq!(s.rows, "");
    }

    #[test]
    fn enter_commits_valid_shape() {
        let mut s = DimEditState::new("W", "256".into(), "384".into(), DimField::Cols);
        assert_eq!(
            s.on_key("enter", None, false),
            DimKeyAction::Commit(256, 384)
        );
    }

    #[test]
    fn enter_on_empty_or_zero_is_a_noop() {
        let mut empty = DimEditState::new("W", String::new(), String::new(), DimField::Rows);
        assert_eq!(empty.on_key("enter", None, false), DimKeyAction::None);

        let mut zero = DimEditState::new("W", "0".into(), "4".into(), DimField::Rows);
        assert_eq!(zero.on_key("enter", None, false), DimKeyAction::None);
    }

    #[test]
    fn escape_cancels() {
        let mut s = DimEditState::new("W", "1".into(), "2".into(), DimField::Rows);
        assert_eq!(s.on_key("escape", None, false), DimKeyAction::Cancel);
    }

    #[test]
    fn modified_keys_do_not_type() {
        let mut s = DimEditState::new("W", String::new(), String::new(), DimField::Rows);
        // e.g. ⌘ combos arrive as printable=false — must not type.
        s.on_key("2", None, false);
        assert_eq!(s.rows, "");
    }

    #[test]
    fn display_value_falls_back_to_placeholder() {
        assert_eq!(display_value("", "128"), ("128".to_string(), true));
        assert_eq!(display_value("64", "128"), ("64".to_string(), false));
    }
}
