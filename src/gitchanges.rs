//! The work tree's changes in the history: the "Changes" row above the commits, and
//! its page, where files and hunks are staged, unstaged and discarded and a commit is
//! written. The lists are read on a worker when the page is wanted and again whenever
//! the status marks are read anew, so what the page shows follows the files.

use std::path::PathBuf;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align, Align2, Color32, CornerRadius, FontId, Key, Rect, ScrollArea, Sense, Stroke, Ui, UiBuilder, Vec2, pos2, vec2};

use crate::app::Action;
use crate::gitlog::{Change, Slot, spawn, take};
use crate::gitwork::{FileDiff, Op, Work};
use crate::listing::Env;
use crate::theme::Palette;

/// A file whose diff is open under its row.
struct Inline {
    staged: bool,
    path: String,
    diff: Option<FileDiff>,
    job: Option<Slot<FileDiff>>,
    /// The files changed since it was read: shown until the new one is in.
    stale: bool,
    /// Each hunk's wrapped rows.
    wrapped: Vec<Option<crate::inspector::Wrapped>>,
}

/// A row of the page's list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    Group(bool),
    File(bool, usize),
}

pub struct Changes {
    root: PathBuf,
    pub work: Option<Work>,
    /// What the index is compared with (HEAD, or the empty tree before a first commit).
    base: Option<String>,
    job: Option<Slot<Option<(Work, Option<String>)>>>,
    /// Read again once the running read is in.
    again: bool,
    /// The status answer last read for: a newer one means the files changed.
    seen: usize,
    /// The page is shown in place of the graph.
    pub open: bool,
    sel: usize,
    keyed: bool,
    reveal: bool,
    inline: Vec<Inline>,
    pub summary: String,
    pub body: String,
    pub amend: bool,
    /// What was typed before Amend filled the fields with the last commit's message.
    typed: Option<(String, String)>,
    amend_job: Option<Slot<Option<(String, String)>>>,
    op: Option<(Slot<Result<String, String>>, bool)>,
    queued: Option<Op>,
    /// The last change's outcome, said under the commit button.
    pub said: Option<Result<String, String>>,
    focus_summary: bool,
    /// A commit was just made: with nothing left, back to the graph.
    done: bool,
}

/// What a simulated user can check.
#[cfg(test)]
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Probe {
    pub open: bool,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub sel: usize,
    /// Open diffs: (staged, path, hunks).
    pub inline: Vec<(bool, String, usize)>,
    pub wrapped: Vec<usize>,
    pub busy: bool,
    pub said: Option<Result<String, String>>,
    pub amend: bool,
    pub summary: String,
}

impl Changes {
    pub fn new(root: PathBuf) -> Changes {
        Changes { root, work: None, base: None, job: None, again: false, seen: 0, open: false, sel: 0, keyed: false, reveal: false, inline: vec![], summary: String::new(), body: String::new(), amend: false, typed: None, amend_job: None, op: None, queued: None, said: None, focus_summary: false, done: false }
    }

    #[cfg(test)]
    pub fn probe(&self) -> Probe {
        let names = |v: &[Change]| v.iter().map(|c| c.path.clone()).collect();
        let w = self.work.as_ref();
        Probe {
            open: self.open,
            staged: w.map_or(vec![], |w| names(&w.staged)),
            unstaged: w.map_or(vec![], |w| names(&w.unstaged)),
            sel: self.sel,
            inline: self.inline.iter().map(|x| (x.staged, x.path.clone(), x.diff.as_ref().map_or(0, |d| d.hunks.len()))).collect(),
            wrapped: self.inline.iter().flat_map(|x| x.wrapped.iter().flatten().map(|w| w.rows.len())).collect(),
            busy: self.op.is_some() || self.queued.is_some() || self.job.is_some(),
            said: self.said.clone(),
            amend: self.amend,
            summary: self.summary.clone(),
        }
    }

    /// Whether there is anything to show a row for.
    pub fn any(&self) -> bool {
        self.work.as_ref().is_some_and(|w| !w.is_empty())
    }

    /// "2 staged · 3 not staged" under the Changes row.
    pub fn summary_line(&self) -> String {
        let Some(w) = &self.work else { return String::new() };
        let files = |n: usize| format!("{n} file{}", if n == 1 { "" } else { "s" });
        match (w.staged.len(), w.unstaged.len()) {
            (0, u) => format!("{} not staged", files(u)),
            (s, 0) => format!("{} staged", files(s)),
            (s, u) => format!("{s} staged · {u} not staged"),
        }
    }

    /// Read the lists again when the status marks (`repo`) were read anew.
    pub fn follow(&mut self, repo: usize, ctx: &egui::Context) {
        if repo != 0 && repo != self.seen {
            self.seen = repo;
            self.read(ctx);
        }
    }

    fn read(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            self.again = true;
            return;
        }
        let root = self.root.clone();
        self.job = Some(spawn(ctx, move || {
            let w = crate::gitwork::work(&root)?;
            let base = crate::gitwork::base(&root, w.head.as_deref());
            Some((w, base))
        }));
    }

    /// Start a change (the page's buttons and keys, and a confirmed discard).
    pub fn queue(&mut self, op: Op) {
        self.queued = Some(op);
    }

    fn busy(&self) -> bool {
        self.op.is_some() || self.queued.is_some()
    }

    /// Ctrl+Enter, from anywhere on the page.
    pub fn commit_now(&mut self) {
        if !self.busy() {
            self.queued = Some(Op::Commit { summary: self.summary.clone(), body: self.body.clone(), amend: self.amend });
        }
    }

    /// Take in what the workers finished. True when a commit was made (the graph has a
    /// new head).
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        if self.work.is_none() && self.job.is_none() && self.seen == 0 {
            self.seen = 1;
            self.read(ctx);
        }
        if let Some(r) = take(&self.job) {
            self.job = None;
            match r {
                Some((w, base)) => {
                    // Open diffs are read again; those whose file left their list close.
                    self.inline.retain(|x| if x.staged { &w.staged } else { &w.unstaged }.iter().any(|c| c.path == x.path));
                    for x in &mut self.inline {
                        x.stale = true;
                        x.job = None;
                    }
                    self.work = Some(w);
                    self.base = base;
                }
                None => self.work = Some(Work::default()),
            }
            if std::mem::take(&mut self.again) {
                self.read(ctx);
            }
            if std::mem::take(&mut self.done) && !self.any() {
                self.open = false;
            }
            self.sel = self.sel.min(self.rows().len().saturating_sub(1));
        }
        let mut committed = false;
        if let Some((r, commit)) = self.op.as_ref().and_then(|(s, c)| s.lock().ok()?.take().map(|r| (r, *c))) {
            self.op = None;
            if commit && r.is_ok() {
                committed = true;
                self.done = true;
                self.summary.clear();
                self.body.clear();
                self.amend = false;
                self.typed = None;
            }
            self.said = Some(r);
            crate::gitstatus::rescan(&self.root);
            self.read(ctx);
        }
        if let Some(op) = self.queued.take() {
            if self.op.is_none() {
                let commit = matches!(op, Op::Commit { .. });
                let root = self.root.clone();
                self.said = None;
                self.op = Some((spawn(ctx, move || crate::gitwork::run(&root, &op)), commit));
            }
        }
        if let Some(m) = take(&self.amend_job) {
            self.amend_job = None;
            if let Some((s, b)) = m.filter(|_| self.amend) {
                self.typed = Some((std::mem::replace(&mut self.summary, s), std::mem::replace(&mut self.body, b)));
            }
        }
        if let (Some(w), Some(base)) = (&self.work, &self.base) {
            for x in &mut self.inline {
                if (x.diff.is_none() || x.stale) && x.job.is_none() {
                    x.stale = false;
                    let list = if x.staged { &w.staged } else { &w.unstaged };
                    if let Some(c) = list.iter().find(|c| c.path == x.path).cloned() {
                        let (root, base, staged) = (self.root.clone(), base.clone(), x.staged);
                        x.job = Some(spawn(ctx, move || crate::gitwork::file_diff(&root, &c, staged, &base)));
                    }
                }
                if let Some(d) = take(&x.job) {
                    x.job = None;
                    if x.wrapped.len() != d.hunks.len() || x.diff.as_ref().is_some_and(|o| o.lines() != d.lines()) {
                        x.wrapped = (0..d.hunks.len()).map(|_| None).collect();
                    }
                    x.diff = Some(d);
                }
            }
        }
        committed
    }

    fn set_amend(&mut self, on: bool, ctx: &egui::Context) {
        self.amend = on;
        if on {
            // The last commit's message to start from, unless something is typed.
            if self.summary.trim().is_empty() && self.body.trim().is_empty() {
                let root = self.root.clone();
                self.amend_job = Some(spawn(ctx, move || crate::gitwork::head_message(&root)));
            }
        } else if let Some((s, b)) = self.typed.take() {
            self.summary = s;
            self.body = b;
        }
    }

    fn rows(&self) -> Vec<Row> {
        let Some(w) = &self.work else { return vec![] };
        let mut v = vec![];
        for (staged, list) in [(true, &w.staged), (false, &w.unstaged)] {
            if !list.is_empty() {
                v.push(Row::Group(staged));
                v.extend((0..list.len()).map(|i| Row::File(staged, i)));
            }
        }
        v
    }

    fn file(&self, staged: bool, i: usize) -> Option<&Change> {
        let w = self.work.as_ref()?;
        if staged { w.staged.get(i) } else { w.unstaged.get(i) }
    }

    fn toggle(&mut self, staged: bool, path: &str) {
        match self.inline.iter().position(|x| x.staged == staged && x.path == path) {
            Some(k) => {
                self.inline.remove(k);
            }
            None => self.inline.push(Inline { staged, path: path.to_string(), diff: None, job: None, stale: false, wrapped: vec![] }),
        }
    }

    /// Whether a key is the page's.
    pub fn takes(&self, k: Key) -> bool {
        matches!(k, Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown | Key::Home | Key::End | Key::Enter | Key::Escape | Key::Space | Key::Delete)
    }

    /// A key on the page: moving, opening a diff, staging (Space) and discarding
    /// (Delete, which asks first). Escape goes back to the graph.
    pub fn key(&mut self, k: Key, panel: u64, actions: &mut Vec<Action>) {
        self.keyed = true;
        let rows = self.rows();
        let n = rows.len();
        let d: isize = match k {
            Key::Escape => {
                self.open = false;
                return;
            }
            Key::ArrowDown => 1,
            Key::ArrowUp if self.sel == 0 || rows.iter().take(self.sel).all(|r| matches!(r, Row::Group(_))) => {
                // Above the first file: the summary.
                self.focus_summary = true;
                return;
            }
            Key::ArrowUp => -1,
            Key::PageDown => 10,
            Key::PageUp => -10,
            Key::Home => -(self.sel as isize),
            Key::End => n as isize,
            _ => 0,
        };
        if d != 0 {
            self.sel = self.sel.saturating_add_signed(d).min(n.saturating_sub(1));
            // Group headings are passed over.
            if matches!(rows.get(self.sel), Some(Row::Group(_))) {
                self.sel = if d > 0 || self.sel == 0 { (self.sel + 1).min(n.saturating_sub(1)) } else { self.sel - 1 };
            }
            self.reveal = true;
            return;
        }
        let Some(Row::File(staged, i)) = rows.get(self.sel).copied() else { return };
        let Some(f) = self.file(staged, i).cloned() else { return };
        match k {
            Key::Enter => self.toggle(staged, &f.path),
            Key::Space if !self.busy() => self.queue(stage_op(&f, staged)),
            Key::Delete if !staged && !self.busy() => actions.push(Action::Open(confirm_discard(panel, f))),
            _ => {}
        }
    }

    /// Whether a diff with lines is open (the Wrap button applies).
    pub fn wraps(&self) -> bool {
        self.inline.iter().any(|x| x.diff.as_ref().is_some_and(|d| d.lines() > 0))
    }
}

/// Stage a file from the unstaged list, or unstage one from the staged list (a rename
/// with the name it had).
fn stage_op(f: &Change, staged: bool) -> Op {
    let mut paths = vec![f.path.clone()];
    paths.extend(f.from.clone());
    if staged { Op::Unstage(paths) } else { Op::Stage(paths) }
}

/// Discarding asks first: what goes where.
fn confirm_discard(panel: u64, f: Change) -> crate::popups::Popup {
    let name = f.path.trim_end_matches('/').to_string();
    let (msg, ok) = if f.status == b'?' { (format!("Move {name} to the trash?"), "Move to Trash") } else { (format!("Discard the changes to {name}? They are kept in the stash."), "Discard") };
    crate::popups::Popup::confirm(
        msg,
        ok,
        Box::new(move |app: &mut crate::app::App| {
            if let Some(h) = app.panels.get_mut(&panel).and_then(|p| p.inspector.as_mut()).and_then(|i| i.git.as_mut()) {
                h.changes.queue(Op::Discard(f));
            }
        }),
    )
}

fn status_color(s: u8, pal: &Palette) -> Color32 {
    match s {
        b'?' => pal.text_dim,
        b'U' => Color32::from_rgb(0xc0, 0x61, 0xcb),
        b'A' => crate::gitstatus::GREEN,
        b'D' => pal.danger,
        b'R' | b'C' => crate::gitstatus::BLUE,
        _ => crate::gitstatus::AMBER,
    }
}

fn status_tip(f: &Change) -> String {
    match (f.status, &f.from) {
        (b'?', _) => "Untracked".into(),
        (b'U', _) => "Unmerged: staging marks it resolved".into(),
        (b'A', _) => "Added".into(),
        (b'D', _) => "Deleted".into(),
        (b'R', Some(from)) => format!("Renamed from {from}"),
        (b'C', Some(from)) => format!("Copied from {from}"),
        (b'T', _) => "Type changed (a file, a link or a submodule)".into(),
        _ => "Modified".into(),
    }
}

/// A small square button on a row: an icon, a tooltip, a name for the simulations.
#[allow(clippy::too_many_arguments)]
fn row_button(ui: &mut Ui, r: Rect, id: egui::Id, tip: &str, what: &'static str, pal: &Palette, paint: impl FnOnce(&egui::Painter, Rect, Color32)) -> bool {
    let b = ui.interact(r, id.with(what), Sense::click());
    crate::ui::note_control(ui, r, what);
    if b.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(4), pal.row_hover);
    }
    paint(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(12.0)), if b.hovered() { pal.text } else { pal.text_dim });
    b.on_hover_text(tip).clicked()
}

fn minus(p: &egui::Painter, r: Rect, c: Color32) {
    p.line_segment([pos2(r.left() + 1.5, r.center().y), pos2(r.right() - 1.5, r.center().y)], Stroke::new(1.5, c));
}

fn plus(p: &egui::Painter, r: Rect, c: Color32) {
    minus(p, r, c);
    p.line_segment([pos2(r.center().x, r.top() + 1.5), pos2(r.center().x, r.bottom() - 1.5)], Stroke::new(1.5, c));
}

/// A text button in a group's heading ("Stage all").
#[allow(clippy::too_many_arguments)]
fn text_button(ui: &mut Ui, id: egui::Id, right: f32, cy: f32, label: &str, what: &'static str, enabled: bool, font: &FontId, pal: &Palette) -> (bool, f32) {
    let g = ui.painter().layout_no_wrap(label.to_string(), font.clone(), pal.text_dim);
    let r = Rect::from_min_max(pos2(right - g.size().x - 14.0, cy - 10.0), pos2(right, cy + 10.0));
    let b = ui.interact(r, id.with(what), if enabled { Sense::click() } else { Sense::hover() });
    crate::ui::note_control(ui, r, what);
    if b.hovered() && enabled {
        ui.painter().rect_filled(r, CornerRadius::same(4), pal.row_hover);
    }
    let col = if !enabled { pal.text_dim.gamma_multiply(0.5) } else if b.hovered() { pal.text } else { pal.text_dim };
    ui.painter().galley(pos2(r.left() + 7.0, cy - g.size().y / 2.0), g, col);
    (enabled && b.clicked(), r.left())
}

enum Act {
    Toggle(bool, String),
    Op(Op),
    Discard(Change),
    Amend(bool),
}

/// The page: the commit box, then the staged and the unstaged files.
pub fn page(ui: &mut Ui, body: Rect, c: &mut Changes, wrap: bool, env: &mut Env) {
    let pal = *env.pal;
    let (fs, sp) = (env.cfg.font_size, env.cfg.spacing);
    let mut inner = ui.new_child(UiBuilder::new().max_rect(body.shrink2(vec2(6.0, 4.0))));
    let mut act = None;
    let reveal = std::mem::take(&mut c.reveal);
    let busy = c.busy();
    let ctx = ui.ctx().clone();
    let small = FontId::proportional(fs - 1.5);
    let rows = c.rows();
    // Out while drawing, so the rows can be changed beside it; back at the end.
    let work = c.work.take();
    ScrollArea::vertical().auto_shrink([false, false]).id_salt("git_changes").show(&mut inner, |ui| {
        ui.add_space(6.0);
        // The commit box, in a quiet card.
        egui::Frame::NONE.fill(pal.card()).stroke(Stroke::new(1.0, pal.card_line())).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            let font = FontId::proportional(fs);
            let s = ui.add(egui::TextEdit::singleline(&mut c.summary).hint_text("Summary").font(font.clone()).desired_width(f32::INFINITY).margin(egui::Margin::symmetric(6, 4)));
            crate::ui::note_control(ui, s.rect, "commit summary");
            if std::mem::take(&mut c.focus_summary) {
                s.request_focus();
            }
            let b = ui.add(egui::TextEdit::multiline(&mut c.body).hint_text("Description").font(FontId::proportional(fs - 0.5)).desired_rows(2).desired_width(f32::INFINITY).margin(egui::Margin::symmetric(6, 4)));
            crate::ui::note_control(ui, b.rect, "commit body");
            let staged = work.as_ref().map_or(0, |w| w.staged.len());
            let can = !busy && !c.summary.trim().is_empty() && (staged > 0 || c.amend);
            // The switch and its words, then the button at the right: on one row when
            // they fit, else the switch above the button.
            let amend = |ui: &mut Ui, act: &mut Option<Act>| {
                let mut on = c.amend;
                if crate::popups::switch(ui, &mut on, &pal) {
                    *act = Some(Act::Amend(on));
                }
                // The words switch it too.
                if ui.available_width() > 30.0 && ui.add(egui::Label::new(egui::RichText::new("Amend the last commit").size(fs - 1.0).color(pal.text_dim)).sense(Sense::click()).truncate()).clicked() {
                    *act = Some(Act::Amend(!c.amend));
                }
            };
            let button = |ui: &mut Ui, act: &mut Option<Act>| {
                let label = if c.amend { "Amend" } else { "Commit" };
                let b = crate::popups::primary_button(ui, label, can, &pal);
                crate::ui::note_control(ui, b.rect, "commit");
                let tip = if c.summary.trim().is_empty() { "Write a summary first" } else if staged == 0 && !c.amend { "Stage something first" } else { "Ctrl+Enter" };
                if b.on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                    *act = Some(Act::Op(Op::Commit { summary: c.summary.clone(), body: c.body.clone(), amend: c.amend }));
                }
                if busy {
                    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                    crate::ui::spinner(ui.painter(), r.center(), 5.0, pal.accent, ui.input(|i| i.time));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
                }
            };
            let wide = ui.available_width() > 120.0 + 12.0 * fs;
            if !wide {
                ui.horizontal(|ui| amend(ui, &mut act));
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    button(ui, &mut act);
                    if wide {
                        ui.add_space(6.0);
                        ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| amend(ui, &mut act));
                    }
                });
            });
            if let Some(r) = &c.said {
                let (t, col) = match r {
                    Ok(t) => (t.as_str(), crate::gitstatus::GREEN),
                    Err(t) => (t.as_str(), pal.danger),
                };
                ui.add(egui::Label::new(egui::RichText::new(t).size(fs - 1.0).color(col)).wrap());
            }
        });
        ui.add_space(6.0);
        let Some(w) = &work else {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Loading…").size(fs - 0.5).color(pal.text_dim));
            });
            return;
        };
        if w.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("No changes").size(fs - 0.5).color(pal.text_dim));
            });
            return;
        }
        let row_h = (fs + 4.0 + 4.0 * sp).round();
        let group_h = (fs + 14.0).round();
        let code = env.cfg.inspector_font_size;
        let spacing = env.cfg.preview_line_spacing;
        let line_h = crate::inspector::row_height(ui, code, spacing);
        let mono = FontId::monospace((fs - 2.5).max(8.0));
        let cw = ui.fonts_mut(|f| f.glyph_width(&mono, '0'));
        let cap = (body.height() * 0.62).max(line_h * 8.0);
        let hunk_h = |x: &Inline, k: usize, lines: usize| {
            let n = x.wrapped.get(k).and_then(|w| w.as_ref()).filter(|_| wrap).map_or(lines, |w| w.rows.len());
            (n as f32 * line_h + 12.0).min(cap)
        };
        let extra = |staged: bool, path: &str| -> f32 {
            let Some(x) = c.inline.iter().find(|x| x.staged == staged && x.path == path) else { return 0.0 };
            match &x.diff {
                Some(d) if !d.hunks.is_empty() => d.hunks.iter().enumerate().map(|(k, h)| row_h + hunk_h(x, k, h.lines.len()) + 4.0).sum::<f32>() + 6.0 + if d.cut { row_h } else { 0.0 },
                _ => row_h + 4.0,
            }
        };
        let heights: Vec<f32> = rows.iter().map(|r| match *r {
            Row::Group(_) => group_h,
            Row::File(s, i) => row_h + (if s { &w.staged } else { &w.unstaged }).get(i).map_or(0.0, |f| extra(s, &f.path)),
        }).collect();
        let widest = w.staged.iter().chain(&w.unstaged).filter_map(|f| f.lines).fold((0, 0), |a, (x, y)| (a.0.max(x), a.1.max(y)));
        let digits = |n: u32| n.to_string().len() as f32;
        let add_w = if widest.0 > 0 { (digits(widest.0) + 1.0) * cw } else { 0.0 };
        let del_w = if widest.1 > 0 { (digits(widest.1) + 1.0) * cw } else { 0.0 };
        let total: f32 = heights.iter().sum();
        let (all, _) = ui.allocate_exact_size(vec2(ui.available_width(), total + 10.0), Sense::hover());
        let clip = ui.clip_rect();
        let badge = (fs + 2.0).round();
        let (icon, gap) = (16.0, 7.0);
        let show_counts = all.width() > 230.0;
        let font = FontId::proportional(fs - 0.5);
        let keyed = c.keyed;
        let mut y = all.top();
        for (k, row) in rows.iter().enumerate() {
            let r = Rect::from_min_size(pos2(all.left(), y), vec2(all.width(), heights[k]));
            y += heights[k];
            if reveal && k == c.sel {
                ui.scroll_to_rect(Rect::from_min_size(r.min, vec2(r.width(), row_h)).expand2(vec2(0.0, 6.0)), None);
            }
            if r.bottom() < clip.top() || r.top() > clip.bottom() {
                continue;
            }
            let (staged, i) = match *row {
                Row::Group(staged) => {
                    // "Staged 2" or "Changes 3", and the button that moves them all.
                    let n = if staged { w.staged.len() } else { w.unstaged.len() };
                    let cy = r.center().y + 2.0;
                    let g = ui.painter().layout_no_wrap(if staged { "Staged".into() } else { "Changes".into() }, FontId::proportional(fs - 1.0), pal.text_dim);
                    let gw = g.size().x;
                    ui.painter().galley(pos2(r.left() + 8.0, cy - g.size().y / 2.0), g, pal.text_dim);
                    ui.painter().text(pos2(r.left() + 8.0 + gw + 6.0, cy), Align2::LEFT_CENTER, n.to_string(), small.clone(), pal.text_dim.gamma_multiply(0.7));
                    let (label, what, op) = if staged { ("Unstage all", "unstage all", Op::UnstageAll) } else { ("Stage all", "stage all", Op::StageAll) };
                    if r.width() > 200.0 && text_button(ui, ui.id().with("group"), r.right() - 4.0, cy, label, what, !busy, &small, &pal).0 {
                        act = Some(Act::Op(op));
                    }
                    continue;
                }
                Row::File(s, i) => (s, i),
            };
            let Some(f) = (if staged { w.staged.get(i) } else { w.unstaged.get(i) }) else { continue };
            let row = Rect::from_min_size(r.min, vec2(r.width(), row_h));
            let id = ui.id().with(("wfile", staged, &f.path));
            let resp = ui.interact(row, id, Sense::click());
            let on = k == c.sel && keyed;
            if on || resp.hovered() {
                ui.painter().rect_filled(row, CornerRadius::same(4), if on { pal.row_hover } else { pal.hover });
            }
            if resp.clicked() {
                c.sel = k;
                act = Some(Act::Toggle(staged, f.path.clone()));
            }
            let x0 = row.left() + 6.0;
            let sc = status_color(f.status, &pal);
            let br = Rect::from_center_size(pos2(x0 + badge / 2.0, row.center().y), Vec2::splat(badge));
            ui.painter().rect_filled(br, CornerRadius::same(4), sc.gamma_multiply(0.16));
            ui.painter().text(br.center(), Align2::CENTER_CENTER, (f.status as char).to_string(), FontId::monospace((fs - 3.0).max(8.0)), sc);
            ui.interact(br, id.with("tip"), Sense::hover()).on_hover_text(status_tip(f));
            // Right side: the row's buttons on hover, then the counts.
            let expanded = c.inline.iter().any(|x| x.staged == staged && x.path == f.path);
            let mut right = row.right() - 4.0;
            let lit = resp.hovered() || on || expanded || ui.rect_contains_pointer(row);
            let b1 = Rect::from_center_size(pos2(right - 11.0, row.center().y), Vec2::splat(20.0));
            let b2 = b1.translate(vec2(-24.0, 0.0));
            right -= if staged { 26.0 } else { 50.0 };
            if lit && !busy {
                if staged {
                    if row_button(ui, b1, id, "Unstage (Space)", "unstage file", &pal, minus) {
                        act = Some(Act::Op(stage_op(f, true)));
                    }
                } else {
                    if row_button(ui, b1, id, "Stage (Space)", "stage file", &pal, plus) {
                        act = Some(Act::Op(stage_op(f, false)));
                    }
                    let (tip, glyph) = if f.status == b'?' { ("Move to the trash (Delete)", "trash-2") } else { ("Discard changes (Delete)", "rotate-ccw") };
                    if row_button(ui, b2, id, tip, "discard file", &pal, |p, r, c| {
                        crate::theme::paint_lucide(p, r, glyph, c);
                    }) {
                        act = Some(Act::Discard(f.clone()));
                    }
                }
            }
            if show_counts {
                if let Some((a, d)) = f.lines {
                    if d > 0 {
                        ui.painter().text(pos2(right, row.center().y), Align2::RIGHT_CENTER, format!("−{d}"), mono.clone(), pal.danger);
                    }
                    right -= del_w + if del_w > 0.0 { 6.0 } else { 0.0 };
                    if a > 0 {
                        ui.painter().text(pos2(right, row.center().y), Align2::RIGHT_CENTER, format!("+{a}"), mono.clone(), crate::gitstatus::GREEN);
                    }
                    right -= add_w;
                }
            }
            // The name ("old → new" for a rename), the folder after it.
            let (dir, name) = f.path.trim_end_matches('/').rsplit_once('/').unwrap_or(("", f.path.trim_end_matches('/')));
            let mut job = LayoutJob::default();
            let nf = |c: Color32, strike: bool| TextFormat { font_id: font.clone(), color: c, strikethrough: if strike { Stroke::new(1.0, c) } else { Stroke::NONE }, valign: Align::Center, ..Default::default() };
            if let Some(from) = &f.from {
                let (fdir, fname) = from.rsplit_once('/').unwrap_or(("", from));
                job.append(if fdir == dir { fname } else { from }, 0.0, nf(pal.text_dim, false));
                job.append("→", 6.0, nf(pal.text_dim, false));
                job.append(name, 6.0, nf(pal.text, false));
            } else {
                job.append(name, 0.0, nf(if f.status == b'D' { pal.text_dim } else { pal.text }, f.status == b'D'));
            }
            let ir = Rect::from_min_size(pos2(x0 + badge + 6.0, row.center().y - icon / 2.0), Vec2::splat(icon));
            let mut ip = ui.painter().clone();
            if f.status == b'D' {
                ip.multiply_opacity(0.45);
            }
            if f.path.ends_with('/') {
                crate::icons::folder(&ip, ir, &c.root.join(&f.path));
            } else {
                crate::icons::file(&ip, ir, name);
            }
            let nx = ir.right() + gap;
            job.wrap = TextWrapping { max_width: (right - 10.0 - nx).max(8.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
            let g = ui.fonts_mut(|fo| fo.layout_job(job));
            let dx = nx + g.size().x + 10.0;
            ui.painter().galley(pos2(nx, row.center().y - g.size().y / 2.0), g, pal.text);
            if !dir.is_empty() && right - 10.0 - dx > 30.0 {
                let g = crate::listing::elided(ui, dir, small.clone(), right - 10.0 - dx, pal.text_dim);
                ui.painter().galley(pos2(dx, row.center().y - g.size().y / 2.0), g, pal.text_dim);
            }
            // Under the row: the open diff, hunk by hunk.
            let Some(x) = c.inline.iter_mut().find(|x| x.staged == staged && x.path == f.path) else { continue };
            let mut hy = row.bottom() + 2.0;
            let left = row.left() + 4.0;
            match &x.diff {
                None => {
                    crate::ui::spinner(ui.painter(), pos2(nx + 6.0, hy + row_h / 2.0), 5.0, pal.accent, ui.input(|i| i.time));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
                }
                Some(d) if d.hunks.is_empty() => {
                    ui.painter().text(pos2(nx, hy + row_h / 2.0), Align2::LEFT_CENTER, d.note.unwrap_or("No text changes"), small.clone(), pal.text_dim);
                }
                Some(d) => {
                    if !wrap {
                        x.wrapped.iter_mut().for_each(|w| *w = None);
                    }
                    for (k, h) in d.hunks.iter().enumerate() {
                        let head = Rect::from_min_max(pos2(left, hy), pos2(row.right() - 4.0, hy + row_h));
                        let mut hr = head.right() - 2.0;
                        if h.patch().is_some() && !busy && f.status != b'U' {
                            let (label, what) = if staged { ("Unstage hunk", "unstage hunk") } else { ("Stage hunk", "stage hunk") };
                            let (hit, l) = text_button(ui, id.with(("hunk", k)), hr, head.center().y, label, what, true, &small, &pal);
                            if hit {
                                act = Some(Act::Op(Op::Hunk { patch: h.patch().unwrap_or_default().to_vec(), staged }));
                            }
                            hr = l - 6.0;
                        }
                        let g = crate::listing::elided(ui, &h.head, mono.clone(), (hr - head.left() - 8.0).max(8.0), pal.text_dim);
                        ui.painter().galley(pos2(head.left() + 6.0, head.center().y - g.size().y / 2.0), g, pal.text_dim);
                        let bh = hunk_h(x, k, h.lines.len());
                        let b = Rect::from_min_max(pos2(left, head.bottom()), pos2(row.right() - 4.0, head.bottom() + bh));
                        hy = b.bottom() + 4.0;
                        if !ui.is_rect_visible(b) {
                            continue;
                        }
                        ui.painter().rect_filled(b, CornerRadius::same(6), pal.alt);
                        let mut cui = ui.new_child(UiBuilder::new().max_rect(b).id_salt(("whunk", staged, &f.path, k)));
                        cui.set_clip_rect(b.intersect(clip));
                        let wrapped = if wrap { x.wrapped.get_mut(k) } else { None };
                        crate::inspector::text_view(&mut cui, b, &h.lines, false, h.spans.as_deref(), false, wrapped, code, &pal, true, spacing);
                    }
                    if d.cut {
                        ui.painter().text(pos2(left + 6.0, hy + row_h / 2.0), Align2::LEFT_CENTER, "Cut at 1 MB: stage the whole file", small.clone(), pal.text_dim);
                    }
                }
            }
        }
    });
    c.work = work;
    match act {
        Some(Act::Toggle(staged, path)) => c.toggle(staged, &path),
        Some(Act::Op(op)) => c.queue(op),
        Some(Act::Discard(f)) => env.actions.push(Action::Open(confirm_discard(env.panel_id, f))),
        Some(Act::Amend(on)) => c.set_amend(on, &ctx),
        None => {}
    }
}
