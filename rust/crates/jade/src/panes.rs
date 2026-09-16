//! Split editor panes (window management).
//!
//! The center area holds one or more panes side by side. Each pane has its
//! own tab strip and tab set. Exactly one pane has the keyboard. The focused
//! pane's live editor is [`JadeApp::editor`]; every other pane keeps its
//! [`EditorState`] here and swaps it in on focus ([`JadeApp::focus_pane`]),
//! the same stash-and-swap the project subtabs use for a project switch.
//!
//! A Markdown tab in a pane shows as formatted blocks or as raw source
//! (`md_rendered`). The toggle appears only with two or more panes. With one
//! pane the right-docked preview keeps its role.
//!
//! A file is open in one pane at most: an open of a path that another pane
//! holds focuses that pane instead of a second buffer.

use std::path::{Path, PathBuf};

use crate::app::JadeApp;
use crate::editor_view::{EditorState, OpenTab};
use crate::highlight::TokenPalette;
use crate::panels::code_view::{LINE_H, PAD_TOP};
use crate::workspace_state::{PaneState, TabState, WorkspaceUi};

/// The maximum number of side-by-side panes.
pub const MAX_PANES: usize = 4;

/// The payload of a tab drag (gpui `on_drag`): which tab left which pane.
/// Drop targets are the tab chips (insert before), the strips (append), and
/// the pane drop zones (move into the pane, or split to its right).
#[derive(Clone, Debug)]
pub struct TabDrag {
    pub pane: usize,
    pub index: usize,
    pub name: String,
}

/// The ghost that follows the pointer during a tab drag: the tab name on
/// the elevated surface with a hairline.
pub struct TabDragPreview {
    pub name: String,
    pub bg: gpui::Rgba,
    pub fg: gpui::Rgba,
    pub line: gpui::Rgba,
}

impl gpui::Render for TabDragPreview {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::prelude::*;
        gpui::div()
            .px(gpui::px(10.))
            .h(gpui::px(26.))
            .flex()
            .items_center()
            .bg(self.bg)
            .border_1()
            .border_color(self.line)
            .text_color(self.fg)
            .text_size(gpui::px(12.))
            .font_family(crate::fonts::ui_family())
            .child(self.name.clone())
    }
}

/// One editor pane.
pub struct SplitPane {
    /// The pane's editor while it is in the background. `None` for the
    /// focused pane: its editor lives in [`JadeApp::editor`].
    pub editor: Option<EditorState>,
    /// A Markdown tab shows as formatted blocks (true) or raw source (false).
    pub md_rendered: bool,
    /// Scroll handle for the read-only code list of a background pane.
    pub scroll: gpui::UniformListScrollHandle,
    /// Scroll handle for the formatted block list of a background pane.
    pub md_scroll: gpui::ScrollHandle,
    /// Width share of the pane row (flex grow). Equal shares by default;
    /// a divider drag moves share between two neighbors.
    pub weight: f32,
}

/// The narrowest a pane can go in px (a divider drag stops there).
pub const MIN_PANE_W: f32 = 160.0;

impl SplitPane {
    /// The slot of the focused pane (its editor is `JadeApp::editor`).
    pub fn focused() -> Self {
        Self::with(None)
    }

    /// A background pane that owns `editor`.
    pub fn background(editor: EditorState) -> Self {
        Self::with(Some(editor))
    }

    fn with(editor: Option<EditorState>) -> Self {
        SplitPane {
            editor,
            md_rendered: true,
            scroll: gpui::UniformListScrollHandle::new(),
            md_scroll: gpui::ScrollHandle::new(),
            weight: 1.0,
        }
    }

    /// The active tab of a background pane (`None` for the focused slot).
    pub fn active_tab(&self) -> Option<&OpenTab> {
        self.editor.as_ref().and_then(|e| e.active_tab())
    }

    /// The top visible display row of the background code list.
    fn scroll_top(&self) -> usize {
        let scrolled = -f32::from(self.scroll.0.borrow().base_handle.offset().y);
        (((scrolled - PAD_TOP) / LINE_H).ceil().max(0.0)) as usize
    }
}

impl JadeApp {
    /// The index of the pane that has the keyboard.
    pub fn focused_pane(&self) -> usize {
        self.panes
            .iter()
            .position(|p| p.editor.is_none())
            .unwrap_or(0)
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// True with two or more panes.
    pub fn split_mode(&self) -> bool {
        self.panes.len() > 1
    }

    /// The active tab of pane `idx`, focused or not.
    pub fn pane_active_tab(&self, idx: usize) -> Option<&OpenTab> {
        if idx == self.focused_pane() {
            self.editor.active_tab()
        } else {
            self.panes.get(idx)?.active_tab()
        }
    }

    /// True when pane `idx` shows its Markdown tab as formatted blocks. Only
    /// in split mode: with one pane the docked preview shows the blocks.
    pub fn pane_shows_md(&self, idx: usize) -> bool {
        self.split_mode()
            && self.panes.get(idx).is_some_and(|p| p.md_rendered)
            && self
                .pane_active_tab(idx)
                .is_some_and(|t| crate::panels::md_view::is_markdown(&t.path))
    }

    /// Show pane `idx`'s Markdown tab as formatted blocks or raw source.
    pub fn set_pane_md(&mut self, idx: usize, rendered: bool) {
        let Some(pane) = self.panes.get_mut(idx) else {
            return;
        };
        pane.md_rendered = rendered;
        if !rendered && idx == self.focused_pane() {
            self.md_edit = false;
        }
    }

    /// Open a new empty pane to the right of the focused one and focus it.
    /// The next file open lands there.
    pub fn split_pane(&mut self) {
        if self.panes.len() >= MAX_PANES {
            return;
        }
        let idx = self.focused_pane() + 1;
        let editor = EditorState::new(self.editor_palette());
        self.panes.insert(idx, SplitPane::background(editor));
        self.focus_pane(idx);
    }

    /// The pixel offset of a code list (x and y, both at most 0).
    fn list_offset(h: &gpui::UniformListScrollHandle) -> gpui::Point<gpui::Pixels> {
        h.0.borrow().base_handle.offset()
    }

    /// Set the pixel offset of a code list. The list paints at this offset
    /// on the next frame (gpui clamps it to the content then).
    fn set_list_offset(h: &gpui::UniformListScrollHandle, p: gpui::Point<gpui::Pixels>) {
        h.0.borrow().base_handle.set_offset(p);
    }

    /// Snapshot a background pane's page position into its active tab, the
    /// same as `stash_scroll` does for the live editor.
    fn stash_pane_scroll(&mut self, idx: usize) {
        let top = self.panes[idx].scroll_top();
        if let Some(tab) = self.panes[idx]
            .editor
            .as_mut()
            .and_then(|e| e.active_tab_mut())
        {
            tab.scroll_top = top;
        }
    }

    /// Point a background pane's code list at its active tab's remembered
    /// page row, after the pane's active tab changed.
    fn apply_pane_scroll(&mut self, idx: usize) {
        let Some(pane) = self.panes.get(idx) else {
            return;
        };
        let top = pane.active_tab().map(|t| t.scroll_top).unwrap_or(0);
        let x = Self::list_offset(&pane.scroll).x;
        Self::set_list_offset(
            &pane.scroll,
            gpui::point(x, gpui::px(-(top as f32 * LINE_H))),
        );
    }

    /// Take a background pane's editor out of its slot as the live editor.
    /// The pane's active tab remembers its page row first. The live code
    /// list and the live formatted Markdown view take the slot's exact pixel
    /// offsets, so the pane shows the same pixels after the swap.
    fn take_pane_editor(&mut self, idx: usize) -> Option<EditorState> {
        self.stash_pane_scroll(idx);
        let editor = self.panes[idx].editor.take()?;
        Self::set_list_offset(&self.code_scroll, Self::list_offset(&self.panes[idx].scroll));
        self.md_scroll.set_offset(self.panes[idx].md_scroll.offset());
        Some(editor)
    }

    /// Park the live editor in pane slot `idx`. `code_offset` and
    /// `md_offset` are the live offsets, read before the incoming pane
    /// replaced them; they move into the slot's own handles.
    fn park_editor(
        &mut self,
        idx: usize,
        editor: EditorState,
        code_offset: gpui::Point<gpui::Pixels>,
        md_offset: gpui::Point<gpui::Pixels>,
    ) {
        Self::set_list_offset(&self.panes[idx].scroll, code_offset);
        self.panes[idx].md_scroll.set_offset(md_offset);
        self.panes[idx].editor = Some(editor);
    }

    /// Shared bookkeeping after the live editor changed: build target, LSP
    /// `didOpen`, focus, popups, and the find bar. The scroll offsets came
    /// across with the swap; a caller whose active tab changed applies the
    /// tab's remembered page itself.
    fn after_pane_swap(&mut self) {
        self.md_edit = false;
        // The formatted view keeps the offset that came across with the
        // swap. Mark the caret sync as done for the incoming tab, so the
        // next render does not scroll the view to the caret's block.
        self.md_synced = self
            .editor
            .active_tab()
            .map(|t| (t.path.clone(), t.caret_point().row));
        self.active_file = self.editor.active_path();
        match self.active_file.clone() {
            Some(path) => self.after_open_active(&path),
            None => {
                self.dismiss_popups();
                self.pending_editor_focus = true;
            }
        }
        self.find_resync();
    }

    /// Give pane `idx` the keyboard: swap its editor in as the live one and
    /// park the outgoing editor in the old slot. Both panes keep their exact
    /// scroll offsets.
    pub fn focus_pane(&mut self, idx: usize) {
        let from = self.focused_pane();
        if idx == from || idx >= self.panes.len() {
            return;
        }
        self.stash_scroll();
        let code_offset = Self::list_offset(&self.code_scroll);
        let md_offset = self.md_scroll.offset();
        let Some(incoming) = self.take_pane_editor(idx) else {
            return;
        };
        let outgoing = std::mem::replace(&mut self.editor, incoming);
        self.park_editor(from, outgoing, code_offset, md_offset);
        self.after_pane_swap();
    }

    /// Close pane `idx` and every tab in it (silent, like a tab close). The
    /// last pane cannot close. Closing the focused pane moves the keyboard
    /// to the pane that slid into its slot, else the new last one.
    pub fn close_pane(&mut self, idx: usize) {
        if self.panes.len() < 2 || idx >= self.panes.len() {
            return;
        }
        if idx == self.focused_pane() {
            let outgoing = std::mem::take(&mut self.editor);
            self.lsp_close_tabs(&outgoing);
            self.panes.remove(idx);
            let next = idx.min(self.panes.len() - 1);
            self.editor = self.take_pane_editor(next).unwrap_or_default();
            self.after_pane_swap();
        } else {
            let pane = self.panes.remove(idx);
            if let Some(editor) = &pane.editor {
                self.lsp_close_tabs(editor);
            }
        }
    }

    /// The editor of pane `idx`: the live one for the focused slot.
    fn pane_editor_mut(&mut self, idx: usize) -> &mut EditorState {
        let parked = self.panes.get(idx).is_some_and(|p| p.editor.is_some());
        if parked {
            self.panes[idx].editor.as_mut().expect("checked above")
        } else {
            &mut self.editor
        }
    }

    fn pane_editor(&self, idx: usize) -> &EditorState {
        self.panes
            .get(idx)
            .and_then(|p| p.editor.as_ref())
            .unwrap_or(&self.editor)
    }

    /// Move tab `from_index` of pane `from_pane` into pane `to_pane`, before
    /// tab `before` (else at the end), and give it the keyboard. Inside one
    /// pane this is a reorder. A pane that loses its last tab closes.
    pub fn move_tab(
        &mut self,
        from_pane: usize,
        from_index: usize,
        to_pane: usize,
        before: Option<usize>,
    ) {
        let n = self.panes.len();
        if from_pane >= n || to_pane >= n {
            return;
        }
        if from_pane == to_pane {
            if let Some(to) = before {
                self.pane_editor_mut(from_pane).reorder(from_index, to);
            }
            return;
        }
        // Every active tab remembers its page row before the tab sets change.
        self.stash_scroll();
        for i in [from_pane, to_pane] {
            if self.panes[i].editor.is_some() {
                self.stash_pane_scroll(i);
            }
        }
        let source_active = self.pane_editor(from_pane).active_path();
        let Some(tab) = self.pane_editor_mut(from_pane).detach(from_index) else {
            return;
        };
        self.pane_editor_mut(to_pane).insert_tab(tab, before);
        let mut target = to_pane;
        let mut source = Some(from_pane);
        if self.pane_editor(from_pane).tabs.is_empty() && self.split_mode() {
            self.close_pane(from_pane);
            source = None;
            if from_pane < target {
                target -= 1;
            }
        }
        if target == self.focused_pane() {
            self.after_pane_swap();
        } else {
            self.focus_pane(target);
        }
        self.show_moved_tab();
        // The source pane is in the background now: if it lost its active
        // tab, its list moves to the page of the tab that took over.
        if let Some(src) = source {
            if src != target && self.pane_editor(src).active_path() != source_active {
                self.apply_pane_scroll(src);
            }
        }
    }

    /// The moved tab is the live editor's active tab now: show it at its
    /// remembered page row, and let the formatted view sync to its caret.
    fn show_moved_tab(&mut self) {
        self.apply_scroll();
        self.md_synced = None;
    }

    /// Move tab `from_index` of pane `from_pane` into a new pane to the right
    /// of pane `after` and give it the keyboard. At the pane limit the tab
    /// moves into pane `after` instead.
    pub fn split_with_tab(&mut self, from_pane: usize, from_index: usize, after: usize) {
        let n = self.panes.len();
        if from_pane >= n || after >= n {
            return;
        }
        if n >= MAX_PANES {
            self.move_tab(from_pane, from_index, after, None);
            return;
        }
        self.stash_scroll();
        if self.panes[from_pane].editor.is_some() {
            self.stash_pane_scroll(from_pane);
        }
        let source_active = self.pane_editor(from_pane).active_path();
        let Some(tab) = self.pane_editor_mut(from_pane).detach(from_index) else {
            return;
        };
        let mut editor = EditorState::new(self.editor_palette());
        editor.insert_tab(tab, None);
        let mut idx = after + 1;
        self.panes.insert(idx, SplitPane::background(editor));
        // The insert shifted the slots at or after `idx` by one.
        let src = if from_pane >= idx {
            from_pane + 1
        } else {
            from_pane
        };
        let mut source = Some(src);
        if self.pane_editor(src).tabs.is_empty() {
            self.close_pane(src);
            source = None;
            if src < idx {
                idx -= 1;
            }
        }
        self.focus_pane(idx);
        self.show_moved_tab();
        if let Some(src) = source {
            if self.pane_editor(src).active_path() != source_active {
                self.apply_pane_scroll(src);
            }
        }
    }

    /// A divider drag: move width share between pane `left` and its right
    /// neighbor so the divider follows the pointer. `dx` is the pointer
    /// travel in px since the drag start; `(w0, w1)` are the two shares at
    /// the start; `row_w` is the pane row's width in px. Either pane stops
    /// at [`MIN_PANE_W`].
    pub fn resize_panes(&mut self, left: usize, dx: f32, w0: f32, w1: f32, row_w: f32) {
        let right = left + 1;
        if right >= self.panes.len() || row_w <= 0.0 {
            return;
        }
        let total: f32 = self.panes.iter().map(|p| p.weight).sum();
        let px_per_share = row_w / total.max(f32::EPSILON);
        let min = MIN_PANE_W / px_per_share;
        let pair = w0 + w1;
        if pair <= 2.0 * min {
            return; // no room to move either way
        }
        let new_left = (w0 + dx / px_per_share).clamp(min, pair - min);
        self.panes[left].weight = new_left;
        self.panes[right].weight = pair - new_left;
    }

    /// Give every pane the same width (divider double-click).
    pub fn equalize_panes(&mut self) {
        for p in &mut self.panes {
            p.weight = 1.0;
        }
    }

    /// Where `path` is open in a background pane: `(pane, tab index)`.
    pub fn find_in_background_panes(&self, path: &Path) -> Option<(usize, usize)> {
        self.panes.iter().enumerate().find_map(|(pi, p)| {
            p.editor
                .as_ref()
                .and_then(|e| e.index_of(path))
                .map(|ti| (pi, ti))
        })
    }

    /// The tab for `path` in any pane (diagnostics routing).
    pub fn any_tab_mut_for(&mut self, path: &Path) -> Option<&mut OpenTab> {
        if let Some(i) = self.editor.index_of(path) {
            return self.editor.tabs.get_mut(i);
        }
        self.panes
            .iter_mut()
            .find_map(|p| p.editor.as_mut().and_then(|e| e.tab_mut_for(path)))
    }

    /// Every tab in every pane, mutable.
    pub fn all_tabs_mut(&mut self) -> impl Iterator<Item = &mut OpenTab> {
        let live = self.editor.tabs.iter_mut();
        let parked = self
            .panes
            .iter_mut()
            .filter_map(|p| p.editor.as_mut())
            .flat_map(|e| e.tabs.iter_mut());
        live.chain(parked)
    }

    /// Swap the token palette in every pane (theme toggle).
    pub fn set_all_palettes(&mut self, palette: TokenPalette) {
        self.editor.set_palette(palette);
        for p in &mut self.panes {
            if let Some(e) = &mut p.editor {
                e.set_palette(palette);
            }
        }
    }

    /// Reset the layout to one pane (the live editor).
    pub fn reset_panes(&mut self) {
        self.panes = vec![SplitPane::focused()];
    }

    /// The pane layout for the persisted `ui` blob, in display order. Empty
    /// with one pane: `open_tabs` already describes it.
    pub fn pane_states(&self) -> Vec<PaneState> {
        if !self.split_mode() {
            return Vec::new();
        }
        let tabs_of = |e: &EditorState| {
            e.tabs
                .iter()
                .map(|t| TabState {
                    path: t.path.display().to_string(),
                    is_dirty: t.buffer.is_dirty(),
                })
                .collect()
        };
        self.panes
            .iter()
            .map(|p| {
                let e = p.editor.as_ref().unwrap_or(&self.editor);
                PaneState {
                    open_tabs: tabs_of(e),
                    active_tab_index: e.active.map(|i| i as i64),
                    md_rendered: p.md_rendered,
                    weight: p.weight as f64,
                }
            })
            .collect()
    }

    /// Rebuild the pane layout from the persisted blob. With fewer than two
    /// restorable panes the single-pane restore (`open_tabs`) stands. Panes
    /// whose files are all gone drop out.
    pub fn restore_panes(&mut self, ui: &WorkspaceUi) {
        self.reset_panes();
        if ui.panes.len() < 2 {
            return;
        }
        let palette = self.editor_palette();
        let mut editors: Vec<(EditorState, bool, f32)> = ui
            .panes
            .iter()
            .map(|ps| {
                let mut e = EditorState::new(palette);
                for t in &ps.open_tabs {
                    let p = PathBuf::from(&t.path);
                    if p.is_file() {
                        let _ = e.open(&p);
                    }
                }
                if let Some(i) = ps.active_tab_index {
                    if i >= 0 && (i as usize) < e.tabs.len() {
                        e.switch(i as usize);
                    }
                }
                let weight = if ps.weight.is_finite() && ps.weight > 0.0 {
                    ps.weight as f32
                } else {
                    1.0
                };
                (e, ps.md_rendered, weight)
            })
            .collect();
        editors.retain(|(e, _, _)| !e.tabs.is_empty());
        if editors.len() < 2 {
            return;
        }
        let last = editors.len() as i64 - 1;
        let focused = ui.focused_pane.unwrap_or(0).clamp(0, last) as usize;
        self.panes = editors
            .into_iter()
            .map(|(e, md, weight)| {
                let mut p = SplitPane::background(e);
                p.md_rendered = md;
                p.weight = weight;
                p
            })
            .collect();
        self.editor = self.panes[focused].editor.take().unwrap_or_default();
    }
}
