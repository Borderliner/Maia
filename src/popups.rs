//! Floating UIs: GoTo, palette, options, menus, dialogs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{self, Align2, Color32, CornerRadius, Event, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

use crate::app::{Action, App};
use crate::commands::{self, ALL, Binding, Cat, Chord, Cmd};
use crate::config::{CharKeyAction, InspectorLayout, MiddleClick, SaveScope, SavedView, RecentsStore, Startup, ThumbMode, NewTabShows};
use crate::ctxmenu::CtxState;
use crate::fsx;
use crate::fuzzy;
use crate::ops::{Conflict, OpKind};
use crate::rename::{Case, DateKind};
use crate::tab;
use crate::theme::{self, Glyph, Palette, ThemeName, paint_glyph};

pub struct ConflictItem {
    pub src: PathBuf,
    pub dst: PathBuf,
}

pub struct GoItem {
    pub label: String,
    pub sub: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub header: bool,
    /// Drives: used fraction for the fill bar.
    pub usage: Option<f32>,
}

pub struct GoToState {
    pub input: String,
    last: Option<String>,
    pub sel: usize,
    pub items: Vec<GoItem>,
    focus: bool,
    dir_cache: Option<(PathBuf, Vec<(String, bool)>)>,
    /// Folder listing being read in the background for path completion.
    dir_job: Option<(PathBuf, std::sync::mpsc::Receiver<Vec<(String, bool)>>)>,
    /// Sidebar snapshot generation the items were built from.
    snap_gen: u64,
    back: Vec<String>,
    fwd: Vec<String>,
    /// Entry to highlight after the item list is rebuilt (after Back / Up).
    highlight: Option<PathBuf>,
}

pub struct Capture {
    pub cmd: Cmd,
    pub seq: Vec<Chord>,
    pub last: Instant,
}

pub struct PaletteState {
    pub query: String,
    pub sel: usize,
    pub capture: Option<Capture>,
    pub conflict: Option<(Cmd, Binding, Cmd)>,
    focus: bool,
}

pub struct OptState {
    pub query: String,
    focus: bool,
    /// Keyboard-selected row (index among the visible options).
    sel: usize,
    scroll_to_sel: bool,
}

pub struct PathDropState {
    pub panel: u64,
    pub dir: PathBuf,
    pub next: Option<PathBuf>,
    pub query: String,
    pub sel: usize,
    pub entries: Vec<PathBuf>,
    /// File details per entry, read once when the dropdown opens (not every frame).
    pub info: std::collections::HashMap<PathBuf, fsx::Entry>,
    pub anchor: Option<Pos2>,
    focus: bool,
}

pub struct ConflictState {
    pub srcs: Vec<PathBuf>,
    pub dest: PathBuf,
    pub mv: bool,
    pub items: Vec<ConflictItem>,
    pub idx: usize,
    pub decide: HashMap<PathBuf, Conflict>,
    pub apply_all: bool,
}

pub struct PropState {
    pub paths: Vec<PathBuf>,
    pub mode: Option<u32>,
    /// File details read at most once a second (not on every frame).
    snap: Option<PropSnap>,
}

struct PropSnap {
    at: Instant,
    single: Option<(std::fs::Metadata, fsx::Entry)>,
    vol: Option<(u64, u64)>,
    link: Option<String>,
    /// Several items: (is a folder, size of a file).
    multi: Vec<(bool, u64)>,
}

impl PropState {
    fn refresh(&mut self) {
        let fresh = self.snap.as_ref().map(|s| s.at.elapsed() < Duration::from_secs(1) || self.paths.len() > 500).unwrap_or(false);
        if fresh {
            return;
        }
        let mut s = PropSnap { at: Instant::now(), single: None, vol: None, link: None, multi: vec![] };
        if self.paths.len() == 1 {
            let p = &self.paths[0];
            if let (Ok(m), Some(e)) = (std::fs::symlink_metadata(p), fsx::Entry::from_path(p)) {
                s.link = e.is_link.then(|| std::fs::read_link(p).map(|t| t.display().to_string()).unwrap_or_default());
                s.single = Some((m, e));
                s.vol = fsx::disk_space(p);
            }
        } else {
            s.multi = self.paths.iter().map(|p| match std::fs::metadata(p) {
                Ok(m) if m.is_dir() => (true, 0),
                Ok(m) => (false, m.len()),
                Err(_) => (false, 0),
            }).collect();
        }
        self.snap = Some(s);
    }
}

pub enum Popup {
    None,
    GoTo(GoToState),
    Palette(PaletteState),
    Ctx(CtxState),
    Options(OptState),
    Types { panel: u64, query: String },
    TabSearch { query: String, sel: usize },
    SavedView { panel: u64 },
    Zoom { panel: u64, anchor: Pos2 },
    Layouts { delete: bool },
    Prompt { title: String, text: String, on_ok: Option<Box<dyn FnOnce(&mut App, String)>> },
    Confirm { msg: String, ok: String, on_ok: Option<Box<dyn FnOnce(&mut App)>> },
    Conflict(ConflictState),
    Properties(PropState),
    OpenWith { paths: Vec<PathBuf>, query: String, all: bool },
    RunExec { path: PathBuf },
    MainMenu,
    QuickMenu,
    PanelMenu { panel: u64, anchor: Pos2 },
    PathMenu { panel: u64, path: PathBuf, anchor: Pos2 },
    PathDropdown(PathDropState),
    History { panel: u64, anchor: Pos2 },
    RenameMenu { panel: u64, pos: Pos2 },
    FindReplace { panel: u64 },
    About,
    TabMenu { panel: u64, idx: usize, pos: Pos2 },
    TabsOptions { panel: u64, pos: Pos2 },
    GroupMenu { panel: u64, label: String, pos: Pos2 },
    DropMenu { paths: Vec<PathBuf>, dest: PathBuf, pos: Pos2 },
    Connect(ConnectState),
}

/// Connect to a network place: address, and a login when the server wants one.
pub struct ConnectState {
    pub uri: String,
    pub user: String,
    pub password: String,
    pub anonymous: bool,
    pub error: Option<String>,
    focus: bool,
}

impl Popup {

    /// Variant name (simulation failure reports).
    #[cfg(test)]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Popup::None => "None",
            Popup::GoTo(..) => "GoTo",
            Popup::Palette(..) => "Palette",
            Popup::Ctx(..) => "Ctx",
            Popup::Options(..) => "Options",
            Popup::Types { .. } => "Types",
            Popup::TabSearch { .. } => "TabSearch",
            Popup::SavedView { .. } => "SavedView",
            Popup::Zoom { .. } => "Zoom",
            Popup::Layouts { .. } => "Layouts",
            Popup::Prompt { .. } => "Prompt",
            Popup::Confirm { .. } => "Confirm",
            Popup::Conflict(..) => "Conflict",
            Popup::Properties(..) => "Properties",
            Popup::OpenWith { .. } => "OpenWith",
            Popup::RunExec { .. } => "RunExec",
            Popup::Connect(..) => "Connect",
            Popup::MainMenu => "MainMenu",
            Popup::QuickMenu => "QuickMenu",
            Popup::PanelMenu { .. } => "PanelMenu",
            Popup::PathMenu { .. } => "PathMenu",
            Popup::PathDropdown(..) => "PathDropdown",
            Popup::History { .. } => "History",
            Popup::RenameMenu { .. } => "RenameMenu",
            Popup::FindReplace { .. } => "FindReplace",
            Popup::About => "About",
            Popup::TabMenu { .. } => "TabMenu",
            Popup::TabsOptions { .. } => "TabsOptions",
            Popup::GroupMenu { .. } => "GroupMenu",
            Popup::DropMenu { .. } => "DropMenu",
        }
    }
    pub fn goto(input: String) -> Popup {
        Popup::GoTo(GoToState { input, last: None, sel: 0, items: vec![], focus: true, dir_cache: None, dir_job: None, snap_gen: 0, back: vec![], fwd: vec![], highlight: None })
    }
    pub fn palette() -> Popup {
        Popup::Palette(PaletteState { query: String::new(), sel: 0, capture: None, conflict: None, focus: true })
    }
    pub fn options() -> Popup {
        Popup::Options(OptState { query: String::new(), focus: true, sel: 0, scroll_to_sel: false })
    }
    pub fn types(panel: u64) -> Popup {
        Popup::Types { panel, query: String::new() }
    }
    pub fn tab_search() -> Popup {
        Popup::TabSearch { query: String::new(), sel: 0 }
    }
    pub fn saved_view(panel: u64) -> Popup {
        Popup::SavedView { panel }
    }
    pub fn zoom(panel: u64, anchor: Pos2) -> Popup {
        Popup::Zoom { panel, anchor }
    }
    pub fn layouts(delete: bool) -> Popup {
        Popup::Layouts { delete }
    }
    pub fn prompt(title: &str, text: String, on_ok: Box<dyn FnOnce(&mut App, String)>) -> Popup {
        Popup::Prompt { title: title.into(), text, on_ok: Some(on_ok) }
    }
    pub fn confirm(msg: String, ok: &str, on_ok: Box<dyn FnOnce(&mut App)>) -> Popup {
        Popup::Confirm { msg, ok: ok.into(), on_ok: Some(on_ok) }
    }
    pub fn conflict(srcs: Vec<PathBuf>, dest: PathBuf, mv: bool, items: Vec<ConflictItem>) -> Popup {
        Popup::Conflict(ConflictState { srcs, dest, mv, items, idx: 0, decide: HashMap::new(), apply_all: false })
    }
    pub fn properties(paths: Vec<PathBuf>) -> Popup {
        Popup::Properties(PropState { paths, mode: None, snap: None })
    }
    pub fn open_with(paths: Vec<PathBuf>) -> Popup {
        Popup::OpenWith { paths, query: String::new(), all: false }
    }
    pub fn connect(uri: String, error: Option<String>) -> Popup {
        Popup::Connect(ConnectState { uri, user: String::new(), password: String::new(), anonymous: false, error, focus: true })
    }

    pub fn run_exec(path: PathBuf) -> Popup {
        Popup::RunExec { path }
    }
    pub fn panel_menu(panel: u64, anchor: Pos2) -> Popup {
        Popup::PanelMenu { panel, anchor }
    }
    pub fn path_menu(panel: u64, path: PathBuf, anchor: Pos2) -> Popup {
        Popup::PathMenu { panel, path, anchor }
    }
    pub fn history(panel: u64, anchor: Pos2) -> Popup {
        Popup::History { panel, anchor }
    }
    pub fn rename_menu(panel: u64, pos: Pos2) -> Popup {
        Popup::RenameMenu { panel, pos }
    }
    pub fn path_dropdown(panel: u64, dir: PathBuf, next: Option<PathBuf>) -> Popup {
        let entries = subdirs(&dir);
        let info = entries.iter().take(5000).filter_map(|p| fsx::Entry::from_path(p).map(|e| (p.clone(), e))).collect();
        let sel = next.as_ref().and_then(|n| entries.iter().position(|e| e == n)).unwrap_or(0);
        Popup::PathDropdown(PathDropState { panel, dir, next, query: String::new(), sel, entries, info, anchor: None, focus: true })
    }

    pub fn is_capturing(&self) -> bool {
        matches!(self, Popup::Palette(p) if p.capture.is_some())
    }

    pub fn kind_cmd(&self) -> Option<Cmd> {
        match self {
            Popup::Palette(_) => Some(Cmd::Palette),
            Popup::GoTo(_) => Some(Cmd::GoTo),
            Popup::Options(_) => Some(Cmd::Options),
            _ => None,
        }
    }
}

/// Folder contents for the breadcrumb dropdown: folders first, then files (hidden ones skipped).
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(bool, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().take(10_000).map(|d| (d.file_type().map(|t| t.is_dir()).unwrap_or(false) || d.path().is_dir(), d.path())).collect())
        .unwrap_or_default();
    v.retain(|(_, p)| !p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false));
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| tab::natural_cmp(&a.1.to_string_lossy().to_lowercase(), &b.1.to_string_lossy().to_lowercase())));
    v.into_iter().map(|(_, p)| p).collect()
}

// ------------------------------------------------------------------ helpers

/// Consume navigation keys for list popups.
struct Keys {
    up: bool,
    down: bool,
    enter: bool,
    ctrl_enter: bool,
    tab: bool,
    esc: bool,
    left: bool,
    right: bool,
    alt_left: bool,
    alt_right: bool,
    backspace: bool,
    page_up: bool,
    page_down: bool,
}

fn take_keys(ctx: &egui::Context) -> Keys {
    ctx.input_mut(|i| Keys {
        up: i.consume_key(Modifiers::NONE, Key::ArrowUp),
        down: i.consume_key(Modifiers::NONE, Key::ArrowDown),
        ctrl_enter: i.consume_key(Modifiers::CTRL, Key::Enter),
        enter: i.consume_key(Modifiers::NONE, Key::Enter),
        tab: i.consume_key(Modifiers::NONE, Key::Tab),
        esc: i.consume_key(Modifiers::NONE, Key::Escape),
        alt_left: i.consume_key(Modifiers::ALT, Key::ArrowLeft),
        alt_right: i.consume_key(Modifiers::ALT, Key::ArrowRight),
        left: false,
        right: false,
        backspace: i.key_pressed(Key::Backspace),
        page_up: i.consume_key(Modifiers::NONE, Key::PageUp),
        page_down: i.consume_key(Modifiers::NONE, Key::PageDown),
    })
}

fn step(sel: &mut usize, n: usize, k: &Keys, skip: impl Fn(usize) -> bool) {
    if n == 0 {
        *sel = 0;
        return;
    }
    let mut d: isize = 0;
    if k.down {
        d = 1;
    }
    if k.up {
        d = -1;
    }
    if k.page_down {
        d = 10;
    }
    if k.page_up {
        d = -10;
    }
    if d == 0 {
        *sel = (*sel).min(n - 1);
        if skip(*sel) {
            // Move off headers.
            if let Some(j) = (*sel..n).find(|&j| !skip(j)) {
                *sel = j;
            }
        }
        return;
    }
    let mut s = (*sel as isize + d).clamp(0, n as isize - 1) as usize;
    while skip(s) {
        let ns = s as isize + d.signum();
        if ns < 0 || ns >= n as isize {
            break;
        }
        s = ns as usize;
    }
    *sel = s;
}

fn popup_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::NONE
        .fill(pal.raised)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(8))
        .shadow(egui::epaint::Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(90) })
}

/// Top-centered popup. Returns (inner, rect).
fn centered<R>(ctx: &egui::Context, id: &str, width: f32, pal: &Palette, add: impl FnOnce(&mut Ui) -> R) -> (R, Rect) {
    let screen = ctx.content_rect();
    let w = width.min(screen.width() - 24.0);
    note_layer(Id::new(id));
    let r = egui::Area::new(Id::new(id))
        .order(egui::Order::Foreground)
        .fixed_pos(pos2(screen.center().x - w / 2.0, screen.top() + 70.0))
        .show(ctx, |ui| {
            popup_frame(pal)
                .show(ui, |ui| {
                    ui.set_width(w - 16.0);
                    add(ui)
                })
                .inner
        });
    crate::ui::note_popup(r.response.rect);
    (r.inner, r.response.rect)
}

fn anchored<R>(ctx: &egui::Context, id: &str, pos: Pos2, width: f32, pal: &Palette, add: impl FnOnce(&mut Ui) -> R) -> (R, Rect) {
    let screen = ctx.content_rect();
    let x = pos.x.min(screen.right() - width - 8.0).max(screen.left() + 4.0);
    let r = egui::Area::new(Id::new(id)).order(egui::Order::Foreground).fixed_pos(pos2(x, pos.y)).constrain(true).show(ctx, |ui| {
        popup_frame(pal)
            .inner_margin(egui::Margin::same(5))
            .show(ui, |ui| {
                ui.set_width(width - 10.0);
                add(ui)
            })
            .inner
    });
    crate::ui::note_popup(r.response.rect);
    (r.inner, r.response.rect)
}

fn clicked_outside(ctx: &egui::Context, rect: Rect) -> bool {
    ctx.input(|i| i.pointer.any_pressed()) && ctx.pointer_interact_pos().map(|p| !rect.contains(p)).unwrap_or(false)
}

fn search_field(ui: &mut Ui, text: &mut String, hint: &str, focus: &mut bool, pal: &Palette, glyph: Glyph) -> egui::Response {
    let resp = ui
        .horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 22.0), Sense::hover());
            paint_glyph(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(14.0)), glyph, pal.text_dim);
            ui.add(egui::TextEdit::singleline(text).hint_text(hint).frame(false).desired_width(ui.available_width() - 8.0).font(FontId::proportional(15.0)))
        })
        .inner;
    if *focus {
        resp.request_focus();
        *focus = false;
    }
    ui.add_space(2.0);
    ui.painter().line_segment([pos2(ui.min_rect().left(), ui.cursor().top()), pos2(ui.min_rect().right(), ui.cursor().top())], Stroke::new(1.0, pal.border));
    ui.add_space(4.0);
    resp
}

fn hints(ui: &mut Ui, items: &[(&str, &str)], pal: &Palette) {
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (k, label) in items {
            if k.is_empty() {
                ui.label(egui::RichText::new(*label).size(11.0).color(pal.text_dim));
                continue;
            }
            let g = ui.painter().layout_no_wrap(k.to_string(), FontId::proportional(10.5), pal.text_dim);
            let (r, _) = ui.allocate_exact_size(g.size() + vec2(10.0, 6.0), Sense::hover());
            ui.painter().rect(r, CornerRadius::same(4), pal.bg, Stroke::new(1.0, pal.border), StrokeKind::Inside);
            ui.painter().galley(r.min + vec2(5.0, 3.0), g, pal.text_dim);
            crate::ui::note_control(ui, r, "key hint");
            // A key that shares the next one's label ("Up" "Down" to navigate) has none of
            // its own; an empty label threw off the wrapped row and the chips overlapped.
            if !label.is_empty() {
                ui.label(egui::RichText::new(*label).size(11.0).color(pal.text_dim));
                ui.add_space(6.0);
            }
        }
    });
}

/// Full-width selectable row; returns its response. `paint` draws inside the rect.
fn row(ui: &mut Ui, h: f32, selected: bool, pal: &Palette, paint: impl FnOnce(&Ui, Rect)) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if selected {
        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
    } else if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
    }
    paint(ui, r);
    resp
}

fn highlight_job(text: &str, q: &fuzzy::Query, font: FontId, col: Color32, accent: Color32) -> egui::text::LayoutJob {
    let hits: std::collections::HashSet<usize> = fuzzy::positions(q, text).map(|(_, p)| p.into_iter().collect()).unwrap_or_default();
    let mut job = egui::text::LayoutJob::default();
    for (i, ch) in text.chars().enumerate() {
        let c = if hits.contains(&i) { accent } else { col };
        job.append(&ch.to_string(), 0.0, egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() });
    }
    job
}

fn menu_item(ui: &mut Ui, label: &str, chip: Option<String>, pal: &Palette) -> bool {
    let resp = row(ui, 26.0, false, pal, |ui, r| {
        // Fit the row (leaving room for the shortcut chip); paths keep their tail.
        let chip_w = chip.as_ref().map(|c| ui.painter().layout_no_wrap(c.clone(), FontId::proportional(11.0), pal.text_dim).size().x + 22.0).unwrap_or(0.0);
        let room = (r.width() - 20.0 - chip_w).max(24.0);
        let font = FontId::proportional(13.0);
        let g = if label.contains('/') { crate::listing::elided_tail(ui, label, font, room, pal.text) } else { crate::listing::elided(ui, label, font, room, pal.text) };
        ui.painter().galley(pos2(r.left() + 10.0, r.center().y - g.size().y / 2.0), g, pal.text);
        if let Some(c) = &chip {
            crate::ui::paint_chip(ui.painter(), pos2(r.right() - 6.0, r.center().y), c, pal);
        }
    });
    resp.clicked()
}

fn menu_sep(ui: &mut Ui, pal: &Palette) {
    ui.add_space(3.0);
    let y = ui.cursor().top();
    ui.painter().line_segment([pos2(ui.min_rect().left() + 6.0, y), pos2(ui.min_rect().right() - 6.0, y)], Stroke::new(1.0, pal.border));
    ui.add_space(4.0);
}

fn cmd_item(ui: &mut Ui, app: &App, c: Cmd) -> bool {
    cmd_row(ui, app, c)
}

// ------------------------------------------------------------------ entry point

thread_local! {
    /// Layers drawn by popups this frame, and the last open popup's shapes (for the
    /// close fade): (shapes, fade start time).
    static POPUP_LAYERS: std::cell::RefCell<Vec<egui::LayerId>> = const { std::cell::RefCell::new(Vec::new()) };
    static FADE: std::cell::RefCell<(Vec<egui::epaint::ClippedShape>, Option<f64>)> = const { std::cell::RefCell::new((Vec::new(), None)) };
}

const CLOSE_FADE: f64 = 0.08;

fn note_layer(id: Id) {
    POPUP_LAYERS.with(|l| l.borrow_mut().push(egui::LayerId::new(egui::Order::Foreground, id)));
}

/// A closed dialog (palette, GoTo, Options and other centered popups) fades out over
/// 80 ms: its last frame is replayed with falling opacity on a layer that takes no input.
/// Menus close instantly: they are opened and dismissed too often for a fade.
fn close_fade(ctx: &egui::Context, animations: bool, open_now: bool) {
    let now = ctx.input(|i| i.time);
    let layers: Vec<egui::LayerId> = POPUP_LAYERS.with(|l| std::mem::take(&mut *l.borrow_mut()));
    FADE.with(|f| {
        let mut f = f.borrow_mut();
        if open_now {
            // Remember what the open popup looks like this frame.
            let shapes: Vec<egui::epaint::ClippedShape> = ctx.graphics(|g| layers.iter().filter_map(|l| g.get(*l)).flat_map(|pl| pl.all_entries().cloned()).collect());
            *f = (shapes, None);
            return;
        }
        if f.0.is_empty() || !animations {
            f.0.clear();
            return;
        }
        let start = *f.1.get_or_insert(now);
        let t = ((now - start) / CLOSE_FADE) as f32;
        if t >= 1.0 {
            f.0.clear();
            f.1 = None;
            return;
        }
        let mut painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("popup_close_fade")));
        painter.set_opacity((1.0 - t) * (1.0 - t));
        for cs in &f.0 {
            painter.with_clip_rect(cs.clip_rect).add(cs.shape.clone());
        }
        ctx.request_repaint();
    });
}

pub fn draw(app: &mut App, ctx: &egui::Context) {
    if matches!(app.popup, Popup::None) {
        close_fade(ctx, app.cfg.animations, false);
        return;
    }
    let mut popup = std::mem::replace(&mut app.popup, Popup::None);
    let keep = match &mut popup {
        Popup::None => false,
        Popup::GoTo(st) => goto(app, ctx, st),
        Popup::Palette(st) => palette(app, ctx, st),
        Popup::Ctx(st) => crate::ctxmenu::draw(app, ctx, st),
        Popup::Options(st) => options(app, ctx, st),
        Popup::Types { panel, query } => types(app, ctx, *panel, query),
        Popup::TabSearch { query, sel } => tab_search(app, ctx, query, sel),
        Popup::SavedView { panel } => saved_view(app, ctx, *panel),
        Popup::Zoom { panel, anchor } => zoom(app, ctx, *panel, *anchor),
        Popup::Layouts { delete } => layouts(app, ctx, *delete),
        Popup::Prompt { title, text, on_ok } => prompt(app, ctx, title, text, on_ok),
        Popup::Confirm { msg, ok, on_ok } => confirm(app, ctx, msg, ok, on_ok),
        Popup::Conflict(st) => conflict(app, ctx, st),
        Popup::Properties(st) => properties(app, ctx, st),
        Popup::OpenWith { paths, query, all } => open_with(app, ctx, paths, query, all),
        Popup::RunExec { path } => run_exec(app, ctx, path),
        Popup::Connect(st) => connect(app, ctx, st),
        Popup::MainMenu => main_menu(app, ctx),
        Popup::QuickMenu => quick_menu(app, ctx, None),
        Popup::PanelMenu { panel, anchor } => {
            let (p, a) = (*panel, *anchor);
            quick_menu(app, ctx, Some((p, a)))
        }
        Popup::PathMenu { panel, path, anchor } => path_menu(app, ctx, *panel, path, *anchor),
        Popup::PathDropdown(st) => path_dropdown(app, ctx, st),
        Popup::History { panel, anchor } => history(app, ctx, *panel, *anchor),
        Popup::RenameMenu { panel, pos } => rename_menu(app, ctx, *panel, *pos),
        Popup::FindReplace { panel } => find_replace(app, ctx, *panel),
        Popup::About => about(app, ctx),
        Popup::TabMenu { panel, idx, pos } => tab_menu(app, ctx, *panel, *idx, *pos),
        Popup::TabsOptions { panel, pos } => tabs_options(app, ctx, *panel, *pos),
        Popup::GroupMenu { panel, label, pos } => group_menu(app, ctx, *panel, label, *pos),
        Popup::DropMenu { paths, dest, pos } => drop_menu(app, ctx, paths, dest, *pos),
    };
    if keep && matches!(app.popup, Popup::None) {
        app.popup = popup;
    } else if !keep {
        // Give the keyboard back to the listing.
        ctx.memory_mut(|m| {
            if let Some(f) = m.focused() {
                m.surrender_focus(f);
            }
        });
    }
    close_fade(ctx, app.cfg.animations, !matches!(app.popup, Popup::None));
}

// ------------------------------------------------------------------ GoTo

fn is_pathlike(s: &str) -> bool {
    s.starts_with('/') || s.starts_with('~') || s.starts_with('$')
}

fn goto_items(app: &App, st: &mut GoToState, known: &crate::sidebar::Known, ctx: &egui::Context) {
    let input = st.input.clone();
    let mut items = vec![];
    let label_of = |p: &Path| -> String {
        if tab::is_computer(p) {
            "Computer".into()
        } else {
            p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
        }
    };
    // Short location hint: the parent folder, with the home folder as ~.
    let home = fsx::home();
    let sub_of = |p: &Path| -> String {
        if tab::is_computer(p) {
            return String::new();
        }
        let s = p.parent().map(|x| x.display().to_string()).unwrap_or_default();
        let h = home.display().to_string();
        if s.starts_with(&h) { format!("~{}", &s[h.len()..]) } else { s }
    };
    let push = |items: &mut Vec<GoItem>, p: &Path| {
        items.push(GoItem { label: label_of(p), sub: sub_of(p), path: p.to_path_buf(), is_dir: tab::is_computer(p) || known.dirs.contains(p), header: false, usage: None });
    };
    let header = |items: &mut Vec<GoItem>, s: &str| items.push(GoItem { label: s.into(), sub: String::new(), path: PathBuf::new(), is_dir: false, header: true, usage: None });
    if input.trim().is_empty() {
        let collapsed = |g: &str| app.ud.collapsed_groups.iter().any(|x| x == &format!("goto:{g}"));
        // Same groups, order and icons as the sidebar.
        let recents: Vec<PathBuf> = known.recents.iter().take(10).cloned().collect();
        if !recents.is_empty() {
            header(&mut items, "Recents");
            if !collapsed("Recents") {
                for p in &recents {
                    push(&mut items, p);
                }
            }
        }
        if !app.ud.bookmarks.is_empty() {
            header(&mut items, "Bookmarks");
            if !collapsed("Bookmarks") {
                for b in &app.ud.bookmarks {
                    push(&mut items, b);
                }
            }
        }
        header(&mut items, "Storage");
        if !collapsed("Storage") {
            for (path, label, t, f) in &known.mounts {
                let usage = (*t > 0).then(|| (t - f) as f32 / *t as f32);
                items.push(GoItem { label: label.clone(), sub: String::new(), path: path.clone(), is_dir: true, header: false, usage });
            }
        }
        header(&mut items, "Places");
        if !collapsed("Places") {
            for (_, p, _) in &known.places {
                push(&mut items, p);
            }
        }
    } else if is_pathlike(&input) {
        let expanded = fsx::expand_path(&input);
        let (dir, prefix) = if input.ends_with('/') { (expanded.clone(), String::new()) } else { (expanded.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/")), expanded.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()) };
        // Read the folder off the UI thread (a dead network mount must not freeze typing);
        // the list fills in when the read lands.
        if st.dir_cache.as_ref().map(|(d, _)| *d != dir).unwrap_or(true) && st.dir_job.as_ref().map(|(d, _)| *d != dir).unwrap_or(true) {
            let (tx, rx) = std::sync::mpsc::channel();
            let (d2, ctx) = (dir.clone(), ctx.clone());
            std::thread::spawn(move || {
                let list = std::fs::read_dir(&d2)
                    .map(|rd| {
                        rd.flatten()
                            .take(50_000)
                            .map(|d| {
                                let is_dir = d.file_type().map(|t| t.is_dir() || (t.is_symlink() && d.path().is_dir())).unwrap_or(false);
                                (d.file_name().to_string_lossy().into_owned(), is_dir)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let _ = tx.send(list);
                ctx.request_repaint();
            });
            st.dir_job = Some((dir.clone(), rx));
        }
        let empty = vec![];
        let list = st.dir_cache.as_ref().filter(|(d, _)| *d == dir).map(|(_, l)| l).unwrap_or(&empty);
        let lp = prefix.to_lowercase();
        let q = fuzzy::Query::new(&prefix);
        let mut scored: Vec<(i32, &String, bool)> = list
            .iter()
            .filter(|(n, _)| app.cfg.show_hidden || !n.starts_with('.') || prefix.starts_with('.'))
            .filter_map(|(n, d)| {
                let ln = n.to_lowercase();
                if lp.is_empty() {
                    Some((0, n, *d))
                } else if ln.starts_with(&lp) {
                    Some((1000 - n.len() as i32, n, *d))
                } else {
                    fuzzy::score(&q, &ln).map(|s| (s, n, *d))
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.2.cmp(&a.2)).then_with(|| tab::natural_cmp(&a.1.to_lowercase(), &b.1.to_lowercase())));
        if lp.is_empty() {
            scored.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| tab::natural_cmp(&a.1.to_lowercase(), &b.1.to_lowercase())));
        }
        for (_, n, d) in scored.into_iter().take(300) {
            let p = dir.join(n);
            items.push(GoItem { label: n.clone(), sub: format!("› {}", dir.display()), path: p, is_dir: d, header: false, usage: None });
        }
    } else {
        // Fuzzy across places, bookmarks, recents, mounts and folders seen in open tabs.
        let q = fuzzy::Query::new(&input);
        let mut cands: Vec<(PathBuf, i32)> = vec![];
        let mut seen = std::collections::HashSet::new();
        let mut add = |p: PathBuf, bonus: i32, cands: &mut Vec<(PathBuf, i32)>| {
            if seen.insert(p.clone()) {
                cands.push((p, bonus));
            }
        };
        for (i, r) in app.ud.recents.iter().enumerate() {
            add(r.clone(), 40 - (i as i32).min(40), &mut cands);
        }
        for b in &app.ud.bookmarks {
            add(b.clone(), 30, &mut cands);
        }
        for (_, p, _) in &known.places {
            add(p.clone(), 25, &mut cands);
        }
        for m in &known.mounts {
            add(m.0.clone(), 10, &mut cands);
        }
        let mut tab_dirs = std::collections::HashSet::new();
        let mut budget = 200_000usize;
        for p in app.panels.values() {
            for t in &p.tabs {
                for e in &t.entries {
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    if e.is_dir {
                        tab_dirs.insert(e.path.clone());
                        add(e.path.clone(), 5, &mut cands);
                    }
                }
            }
        }
        let mut scored: Vec<(i32, PathBuf)> = cands
            .into_iter()
            .filter_map(|(p, bonus)| {
                let name = p.file_name()?.to_string_lossy().to_lowercase();
                fuzzy::score(&q, &name).map(|s| (s + bonus, p))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        // Only folders known to exist (from the background snapshot or open tabs): no stat calls here.
        for (_, p) in scored.into_iter().filter(|(_, p)| tab::is_computer(p) || known.dirs.contains(p) || tab_dirs.contains(p)).take(200) {
            push(&mut items, &p);
        }
    }
    st.items = items;
}

fn goto(app: &mut App, ctx: &egui::Context, st: &mut GoToState) -> bool {
    let pal = app.pal;
    let mut landed = false;
    if let Some((d, rx)) = &st.dir_job {
        if let Ok(list) = rx.try_recv() {
            st.dir_cache = Some((d.clone(), list));
            st.dir_job = None;
            landed = true;
        }
    }
    let same_input = st.last.as_deref() == Some(st.input.as_str());
    if !same_input || landed || crate::sidebar::generation() != st.snap_gen {
        // Background data changed under an unchanged query: keep the highlighted row.
        let keep = same_input.then(|| st.items.get(st.sel).filter(|i| !i.header).map(|i| i.path.clone())).flatten();
        let known = crate::sidebar::known(app, ctx);
        st.snap_gen = known.generation;
        goto_items(app, st, &known, ctx);
        st.last = Some(st.input.clone());
        let want = st.highlight.take().or(keep);
        st.sel = want.and_then(|h| st.items.iter().position(|i| !i.header && i.path == h)).unwrap_or_else(|| if same_input { st.sel.min(st.items.len().saturating_sub(1)) } else { 0 });
    }
    let k = take_keys(ctx);
    let n = st.items.len();
    {
        let items = &st.items;
        step(&mut st.sel, n, &k, |i| items[i].header);
    }
    let mut close = false;
    let mut go: Option<(PathBuf, bool, bool)> = None; // path, is_dir, new_tab
    if k.esc {
        return false;
    }
    // A network address connects (and opens) the place.
    if (k.enter || k.ctrl_enter) && crate::network::is_uri(&st.input) {
        let panel = app.active;
        app.connect(crate::network::normalize(&st.input), crate::network::Login { user: String::new(), password: String::new(), anonymous: false }, Some((panel, k.ctrl_enter)));
        return false;
    }
    let cur = st.items.get(st.sel).filter(|i| !i.header).map(|i| (i.path.clone(), i.is_dir));
    if k.enter || k.ctrl_enter {
        match &cur {
            Some((p, d)) => go = Some((p.clone(), *d, k.ctrl_enter)),
            None => {
                let p = fsx::expand_path(&st.input);
                if p.exists() {
                    go = Some((p.clone(), p.is_dir(), k.ctrl_enter));
                }
            }
        }
    }
    if k.tab {
        if let Some((p, true)) = &cur {
            st.back.push(st.input.clone());
            st.input = if tab::is_computer(p) { "/".into() } else { format!("{}/", p.display()).replace("//", "/") };
            st.focus = true;
        }
    }
    // Backspace at a separator goes up one level.
    if k.backspace && st.input.ends_with('/') && st.input.len() > 1 {
        let trimmed = st.input.trim_end_matches('/');
        if let Some(i) = trimmed.rfind('/') {
            st.input = trimmed[..=i].to_string();
            ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Backspace));
            st.focus = true;
        }
    }
    let (_, rect) = centered(ctx, "goto", 620.0, &pal, |ui| {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 22.0), Sense::hover());
            let g = if !is_pathlike(&st.input) { Glyph::Search } else if fsx::mounts().iter().any(|m| m.path == fsx::expand_path(st.input.trim_end_matches('/'))) || st.input == "/" { Glyph::Drive } else { Glyph::Folder };
            paint_glyph(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(14.0)), g, pal.text_dim);
            let w = ui.available_width() - 4.0 * 28.0;
            let resp = ui.add(egui::TextEdit::singleline(&mut st.input).hint_text("Open or expand a folder…").frame(false).desired_width(w).font(FontId::proportional(15.0)));
            if st.focus {
                resp.request_focus();
                if let Some(mut s) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
                    let c = egui::text::CCursor::new(st.input.chars().count());
                    s.cursor.set_char_range(Some(egui::text::CCursorRange::one(c)));
                    s.store(ui.ctx(), resp.id);
                }
                st.focus = false;
            }
            if crate::ui::glyph_button(ui, Glyph::Back, "Back", !st.back.is_empty(), &pal).clicked() {
                if let Some(b) = st.back.pop() {
                    let was = std::mem::replace(&mut st.input, b);
                    st.highlight = Some(fsx::expand_path(was.trim_end_matches('/')));
                    st.fwd.push(was);
                    st.focus = true;
                }
            }
            if crate::ui::glyph_button(ui, Glyph::Forward, "Forward", !st.fwd.is_empty(), &pal).clicked() {
                if let Some(f) = st.fwd.pop() {
                    st.back.push(std::mem::replace(&mut st.input, f));
                    st.focus = true;
                }
            }
            if crate::ui::glyph_button(ui, Glyph::Up, "Up", is_pathlike(&st.input), &pal).clicked() {
                let p = fsx::expand_path(st.input.trim_end_matches('/'));
                if let Some(parent) = p.parent() {
                    st.back.push(st.input.clone());
                    st.highlight = Some(p.clone());
                    st.input = format!("{}/", parent.display()).replace("//", "/");
                    st.focus = true;
                }
            }
            if crate::ui::glyph_button(ui, Glyph::Close, "Close (Esc)", true, &pal).clicked() {
                close = true;
            }
        });
        ui.add_space(2.0);
        let q = if is_pathlike(&st.input) {
            let s = st.input.rsplit('/').next().unwrap_or("").to_string();
            fuzzy::Query::new(&s)
        } else {
            fuzzy::Query::new(&st.input)
        };
        let row_h = 30.0;
        egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show_rows(ui, row_h, st.items.len(), |ui, range| {
            for i in range {
                let it = &st.items[i];
                if it.header {
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                    let key = format!("goto:{}", it.label);
                    let collapsed = app.ud.collapsed_groups.contains(&key);
                    let hc = if resp.hovered() { pal.text } else { pal.text_dim };
                    let g = match it.label.as_str() {
                        "Recents" => Glyph::Clock,
                        "Bookmarks" => Glyph::Bookmark,
                        "Storage" => Glyph::Drive,
                        _ => Glyph::Folder,
                    };
                    paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.left() + 16.0, r.center().y), Vec2::splat(14.0)), g, hc);
                    ui.painter().text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, &it.label, FontId::proportional(13.0), hc);
                    let c = pos2(r.right() - 14.0, r.center().y + 2.0);
                    if collapsed {
                        paint_glyph(ui.painter(), Rect::from_center_size(c, Vec2::splat(9.0)), Glyph::ChevronRight, pal.text_dim);
                    } else {
                        ui.painter().add(egui::Shape::line(vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)], Stroke::new(1.3, pal.text_dim)));
                    }
                    if resp.clicked() {
                        if collapsed {
                            app.ud.collapsed_groups.retain(|g| *g != key);
                        } else {
                            app.ud.collapsed_groups.push(key);
                        }
                        app.ud_dirty = true;
                        st.last = None;
                    }
                    continue;
                }
                let sel = i == st.sel;
                let in_root = st.input.trim().is_empty();
                let resp = row(ui, row_h, sel, &pal, |ui, r| {
                    // Root items are indented under their group header.
                    let x0 = r.left() + if in_root { 30.0 } else { 6.0 };
                    let ic = Rect::from_center_size(pos2(x0 + 10.0, r.center().y), Vec2::splat(16.0));
                    if tab::is_computer(&it.path) || it.usage.is_some() {
                        paint_glyph(ui.painter(), ic, Glyph::Drive, if sel { Color32::WHITE } else { pal.text_dim });
                    } else if !(it.is_dir && crate::icons::dir(ui.painter(), ic, &it.path)) {
                        let ext = it.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                        theme::paint_icon(ui.painter(), ic, theme::categorize(&ext, it.is_dir, false), "", false);
                    }
                    let tc = if sel { Color32::WHITE } else { pal.text };
                    let tx = x0 + 26.0;
                    let mut job = highlight_job(&it.label, &q, FontId::proportional(13.0), tc, if sel { Color32::WHITE } else { pal.accent });
                    // One line ending in "…" inside the row; with a location hint, keep room for it.
                    let room = r.right() - tx - if it.sub.is_empty() { 8.0 } else { 90.0 };
                    job.wrap = egui::text::TextWrapping { max_width: room.max(40.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                    let g = ui.fonts_mut(|f| f.layout_job(job));
                    let gw = g.size().x;
                    let ty = if it.usage.is_some() { r.center().y - g.size().y / 2.0 - 4.0 } else { r.center().y - g.size().y / 2.0 };
                    ui.painter().galley(pos2(tx, ty), g, tc);
                    if let Some(u) = it.usage {
                        let bar = Rect::from_min_size(pos2(tx, r.center().y + 6.0), vec2(90.0, 4.0));
                        ui.painter().rect_filled(bar, CornerRadius::same(1), pal.border);
                        ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * u.clamp(0.0, 1.0), bar.height())), CornerRadius::same(1), if sel { Color32::WHITE } else { pal.accent });
                    }
                    if !it.sub.is_empty() {
                        let chev = Rect::from_center_size(pos2(tx + gw + 10.0, r.center().y), Vec2::splat(9.0));
                        let sc = if sel { Color32::from_white_alpha(170) } else { pal.text_dim.gamma_multiply(0.75) };
                        paint_glyph(ui.painter(), chev, Glyph::ChevronRight, sc);
                        let sub = crate::listing::elided(ui, &it.sub, FontId::proportional(11.5), (r.right() - chev.right() - 10.0).max(20.0), sc);
                        ui.painter().galley(pos2(chev.right() + 4.0, r.center().y - sub.size().y / 2.0), sub, sc);
                    }
                });
                if sel && (k.up || k.down || k.page_down || k.page_up) {
                    resp.scroll_to_me(None);
                }
                if resp.clicked() {
                    go = Some((it.path.clone(), it.is_dir, ui.input(|i| i.modifiers.ctrl)));
                }
            }
        });
        if st.items.is_empty() {
            ui.label(egui::RichText::new(if is_pathlike(&st.input) { "No matching entries" } else { "No matches" }).color(pal.text_dim));
        }
        if is_pathlike(&st.input) {
            hints(ui, &[("↑↓", "navigate"), ("Enter", "use"), ("Tab", "expand"), ("Ctrl+Enter", "open in new tab"), ("Esc", "dismiss")], &pal);
        } else {
            hints(ui, &[("↑↓", "navigate"), ("Enter", "use"), ("Tab", "expand"), ("Esc", "dismiss")], &pal);
        }
    });
    if let Some((p, is_dir, new_tab)) = go {
        let id = app.active;
        if new_tab {
            let (path, select) = if is_dir { (p, None) } else { (p.parent().map(Path::to_path_buf).unwrap_or_else(fsx::home), Some(p)) };
            app.actions.push(Action::NewTab { panel: id, path, select, focus: true });
        } else if is_dir {
            app.actions.push(Action::Navigate { panel: id, path: p });
        } else {
            app.actions.push(Action::OpenFile(p));
        }
        return false;
    }
    !(close || clicked_outside(ctx, rect))
}

// ------------------------------------------------------------------ palette

fn applicable(app: &App, c: Cmd) -> bool {
    let Some(p) = app.panels.get(&app.active) else { return true };
    let t = p.tab();
    let panels = app.panels.len();
    match c {
        Cmd::Back => t.can_back() || t.flatten,
        Cmd::Forward => t.can_forward(),
        Cmd::ClosePanel | Cmd::FocusNextPanel | Cmd::FocusPrevPanel | Cmd::CopyToOther | Cmd::MoveToOther => panels > 1,
        Cmd::InspectorLineNumbers | Cmd::InspectorMinimap => p.inspector.is_some(),
        Cmd::ComparePanels | Cmd::SelectDifferences | Cmd::SyncPanels => panels > 1,
        Cmd::OpenLinkTarget => t.target_entries().first().map(|e| e.is_link).unwrap_or(false),
        Cmd::RestoreTrash => t.path.starts_with(crate::sidebar::trash_dir()),
        Cmd::Undo => !app.undo.is_empty(),
        Cmd::ReopenTab => !app.closed_tabs.is_empty(),
        Cmd::Rename | Cmd::Copy | Cmd::Cut | Cmd::Trash | Cmd::DeletePermanent | Cmd::Duplicate | Cmd::CreateLink => !t.targets_idx().is_empty(),
        Cmd::LoadLayout | Cmd::DeleteLayout => !app.ud.layouts.is_empty(),
        Cmd::SortFullAsc | Cmd::SortFullDesc => tab::is_computer(&t.path) && !(t.settings.sort == crate::config::SortKey::Usage && t.settings.desc == (c == Cmd::SortFullDesc)),
        Cmd::SortNameAsc | Cmd::SortNameDesc => !(t.settings.sort == crate::config::SortKey::Name && t.settings.desc == (c == Cmd::SortNameDesc)),
        Cmd::SortSizeAsc | Cmd::SortSizeDesc => !(t.settings.sort == crate::config::SortKey::Size && t.settings.desc == (c == Cmd::SortSizeDesc)),
        Cmd::CloseOtherTabs => p.tabs.len() > 1,
        Cmd::CloseRightTabs | Cmd::MoveTabRight => p.active + 1 < p.tabs.len(),
        Cmd::CloseLeftTabs | Cmd::MoveTabLeft => p.active > 0,
        Cmd::TabNewOrRecent | Cmd::ToggleFlatten | Cmd::OpenTerminal => !tab::is_computer(&t.path),
        Cmd::OpenTabInWindow => p.tabs.len() > 0,
        Cmd::GroupDeleted | Cmd::GroupOriginal => crate::trashcan::is_trash_files(&t.path),
        Cmd::GroupFree | Cmd::GroupFs | Cmd::GroupFull => tab::is_computer(&t.path),
        Cmd::GroupItems | Cmd::GroupUsage | Cmd::GroupName | Cmd::GroupType | Cmd::GroupDate | Cmd::GroupCreated | Cmd::GroupAccessed | Cmd::GroupSize => !tab::is_computer(&t.path) && !t.tree_active(),
        Cmd::GroupExpandAll => t.settings.group != crate::config::GroupBy::None && !t.collapsed_groups.is_empty(),
        Cmd::GroupCollapseAll => t.settings.group != crate::config::GroupBy::None,
        _ => true,
    }
}

/// A menu row for a command: dimmed (and inert) when it can't apply.
fn cmd_row(ui: &mut Ui, app: &App, c: Cmd) -> bool {
    let pal = app.pal;
    let enabled = applicable(app, c);
    let chips: Vec<String> = app.keymap.bindings(c).iter().take(3).map(commands::binding_to_string).collect();
    let resp = row(ui, 26.0, false, &pal, |ui, r| {
        ui.painter().text(pos2(r.left() + 10.0, r.center().y), Align2::LEFT_CENTER, c.info().title, FontId::proportional(13.0), if enabled { pal.text } else { pal.text_dim.gamma_multiply(0.6) });
        let mut x = r.right() - 6.0;
        for ch in chips.iter().rev() {
            x -= crate::ui::paint_chip(ui.painter(), pos2(x, r.center().y), ch, &pal) + 3.0;
        }
    });
    enabled && resp.clicked()
}

fn palette(app: &mut App, ctx: &egui::Context, st: &mut PaletteState) -> bool {
    let mut toggle_compact = false;
    let pal = app.pal;
    // ---------------- hotkey capture
    if let Some(cap) = st.capture.as_mut() {
        let events = ctx.input(|i| i.events.clone());
        let mods = ctx.input(|i| i.modifiers);
        let mut finish = false;
        let mut cancel = false;
        for ev in &events {
            match ev {
                Event::Key { key, pressed: true, modifiers, repeat: false, .. } => match key {
                    Key::Escape if modifiers.is_none() => cancel = true,
                    Key::Enter if modifiers.is_none() && !cap.seq.is_empty() => finish = true,
                    Key::Backspace if modifiers.is_none() => cap.seq.clear(),
                    _ => {
                        cap.seq.push(Chord::from_key(*key, *modifiers));
                        cap.last = Instant::now();
                    }
                },
                Event::Copy => cap.seq.push(Chord::from_key(Key::C, Modifiers { ctrl: true, shift: mods.shift, ..Default::default() })),
                Event::Cut => cap.seq.push(Chord::from_key(Key::X, Modifiers { ctrl: true, shift: mods.shift, ..Default::default() })),
                Event::Paste(_) => cap.seq.push(Chord::from_key(Key::V, Modifiers { ctrl: true, shift: mods.shift, ..Default::default() })),
                Event::PointerButton { button, pressed: true, modifiers, .. } => {
                    if let Some(c) = Chord::from_mouse(*button, *modifiers) {
                        cap.seq.push(c);
                        cap.last = Instant::now();
                    }
                }
                _ => {}
            }
        }
        ctx.input_mut(|i| i.events.clear());
        if !cap.seq.is_empty() && cap.last.elapsed().as_secs_f32() > 1.2 {
            finish = true;
        }
        if !cap.seq.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        let cmd = cap.cmd;
        let seq_txt = commands::binding_to_string(&cap.seq);
        centered(ctx, "capture", 420.0, &pal, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("New hotkey for “{}”", cmd.info().title)).strong());
                ui.add_space(10.0);
                ui.label(egui::RichText::new(if seq_txt.is_empty() { "Press keys…".to_string() } else { seq_txt.clone() }).size(20.0).color(pal.accent));
                ui.add_space(10.0);
                hints(ui, &[("Enter", "save"), ("Backspace", "clear"), ("Esc", "cancel")], &pal);
                ui.label(egui::RichText::new("Sequences (e.g. G then H) are allowed; pause to finish.").small().color(pal.text_dim));
            });
        });
        if cancel {
            st.capture = None;
        } else if finish {
            let binding = cap.seq.clone();
            st.capture = None;
            match app.keymap.conflicts(&binding, cmd) {
                Some(other) => st.conflict = Some((cmd, binding, other)),
                None => add_binding(app, cmd, binding),
            }
        }
        return true;
    }
    if let Some((cmd, binding, other)) = st.conflict.clone() {
        let mut keep_open = true;
        centered(ctx, "hk_conflict", 440.0, &pal, |ui| {
            ui.label(format!("{} is already bound to “{}”.", commands::binding_to_string(&binding), other.info().title));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Replace").clicked() {
                    remove_binding(app, other, &binding);
                    add_binding(app, cmd, binding.clone());
                    st.conflict = None;
                }
                if ui.button("Add anyway").clicked() {
                    add_binding(app, cmd, binding.clone());
                    st.conflict = None;
                }
                if ui.button("Cancel").clicked() {
                    st.conflict = None;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            st.conflict = None;
        }
        let _ = &mut keep_open;
        return true;
    }
    // ---------------- list
    let k = take_keys(ctx);
    if k.esc {
        return false;
    }
    enum PRow {
        Header(Cat, bool),
        Cmd(Cmd, Option<Cat>),
    }
    let q = fuzzy::Query::new(&st.query);
    let mut rows: Vec<PRow> = vec![];
    if q.is_empty() {
        for cat in Cat::ORDER {
            let collapsed = app.ud.collapsed_groups.iter().any(|g| g == cat.name());
            let cmds: Vec<Cmd> = if cat == Cat::Quick {
                app.ud.quick.iter().filter_map(|id| Cmd::from_id(id)).collect()
            } else {
                ALL.iter().copied().filter(|c| c.info().cat == cat).collect()
            };
            if cmds.is_empty() {
                continue;
            }
            rows.push(PRow::Header(cat, collapsed));
            if !collapsed {
                rows.extend(cmds.into_iter().map(|c| PRow::Cmd(c, None)));
            }
        }
    } else {
        let alias_hits: Vec<Cmd> = app.ud.aliases.iter().filter(|(_, a)| a.eq_ignore_ascii_case(st.query.trim())).filter_map(|(id, _)| Cmd::from_id(id)).collect();
        let mut scored: Vec<(i32, Cmd)> = ALL
            .iter()
            .filter_map(|&c| {
                let i = c.info();
                let ts = fuzzy::score(&q, &i.title.to_lowercase()).map(|s| s + 100);
                let cs = fuzzy::score(&q, &format!("{} {}", i.cat.name(), i.title).to_lowercase()).map(|s| s + 50);
                let ds = fuzzy::score(&q, &i.desc.to_lowercase());
                [ts, cs, ds].into_iter().flatten().max().map(|s| (s, c))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        for c in alias_hits {
            rows.push(PRow::Cmd(c, Some(c.info().cat)));
        }
        for (_, c) in scored {
            if !rows.iter().any(|r| matches!(r, PRow::Cmd(x, _) if *x == c)) {
                rows.push(PRow::Cmd(c, Some(c.info().cat)));
            }
        }
    }
    let n = rows.len();
    // Category rows are selectable too (Enter folds or unfolds them).
    step(&mut st.sel, n, &k, |_| false);
    let mut run: Option<Cmd> = None;
    let mut toggle_group: Option<Cat> = None;
    let mut star: Option<Cmd> = None;
    let mut add_key: Option<Cmd> = None;
    let mut remove: Option<(Cmd, Binding)> = None;
    let mut reset: Option<Cmd> = None;
    // F2 names the selected command's alias, as it renames a file.
    let mut alias: Option<Cmd> = match rows.get(st.sel) {
        Some(PRow::Cmd(c, _)) if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F2)) => Some(*c),
        _ => None,
    };
    if k.enter {
        match rows.get(st.sel) {
            Some(PRow::Cmd(c, _)) => run = Some(*c),
            Some(PRow::Header(cat, _)) => toggle_group = Some(*cat),
            None => {}
        }
    }
    let mut close = false;
    let (_, rect) = centered(ctx, "palette", 640.0, &pal, |ui| {
        ui.horizontal(|ui| {
            let before = st.query.clone();
            let w = ui.available_width() - 58.0;
            ui.scope(|ui| {
                ui.set_width(w);
                search_field(ui, &mut st.query, "Select a command…", &mut st.focus, &pal, Glyph::Search);
            });
            if st.query != before {
                st.sel = 0;
            }
            if compact_button(ui, app.cfg.compact_lists, &pal).clicked() {
                toggle_compact = true;
            }
            if crate::ui::glyph_button(ui, Glyph::Close, "Close (Esc)", true, &pal).clicked() {
                close = true;
            }
        });
        egui::ScrollArea::vertical().max_height(460.0).auto_shrink([false, true]).show(ui, |ui| {
            for (i, r) in rows.iter().enumerate() {
                match r {
                    PRow::Header(cat, collapsed) => {
                        // Category row: icon and name on the left, fold chevron on the right.
                        let resp = row(ui, if app.cfg.compact_lists { 28.0 } else { 32.0 }, i == st.sel, &pal, |ui, r| {
                            let col = if i == st.sel { Color32::WHITE } else { pal.text };
                            paint_cat_glyph(ui.painter(), Rect::from_center_size(pos2(r.left() + 13.0, r.center().y), Vec2::splat(13.0)), *cat, if i == st.sel { col } else { pal.text_dim });
                            ui.painter().text(pos2(r.left() + 30.0, r.center().y), Align2::LEFT_CENTER, cat.name(), FontId::proportional(13.0), col);
                            let c = pos2(r.right() - 16.0, r.center().y);
                            if *collapsed {
                                paint_glyph(ui.painter(), Rect::from_center_size(c, Vec2::splat(9.0)), Glyph::ChevronRight, pal.text_dim);
                            } else {
                                ui.painter().add(egui::Shape::line(vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)], Stroke::new(1.3, pal.text_dim)));
                            }
                        });
                        if resp.clicked() {
                            toggle_group = Some(*cat);
                        }
                    }
                    PRow::Cmd(c, cat_prefix) => {
                        let info = c.info();
                        let sel = i == st.sel;
                        let enabled = applicable(app, *c);
                        let starred = app.ud.quick.iter().any(|q| q == info.id);
                        let chips = app.keymap.bindings(*c).to_vec();
                        let overridden = app.cfg.hotkeys.contains_key(info.id);
                        let compact = app.cfg.compact_lists;
                        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), if compact { 28.0 } else { 40.0 }), Sense::click());
                        let hovered = resp.hovered() || ui.rect_contains_pointer(r);
                        if sel {
                            ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
                        } else if hovered {
                            ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                        }
                        if sel && (k.up || k.down || k.page_down || k.page_up) {
                            resp.scroll_to_me(None);
                        }
                        let tcol = if enabled { pal.text } else { pal.text_dim.gamma_multiply(0.7) };
                        // Search results carry a category column (icon, name, ›) before the title.
                        let mut tx = r.left() + 10.0;
                        if let Some(cat) = cat_prefix {
                            let cy = r.center().y;
                            paint_cat_glyph(ui.painter(), Rect::from_center_size(pos2(r.left() + 13.0, cy), Vec2::splat(12.0)), *cat, pal.text_dim);
                            let cg = crate::listing::elided(ui, cat.name(), FontId::proportional(12.0), 70.0, pal.text_dim);
                            ui.painter().galley(pos2(r.left() + 28.0, cy - cg.size().y / 2.0), cg, pal.text_dim);
                            paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.left() + 108.0, cy), Vec2::splat(8.0)), Glyph::ChevronRight, pal.text_dim);
                            tx = r.left() + 122.0;
                        }
                        let mut job = highlight_job(info.title, &q, FontId::proportional(13.5), tcol, pal.accent);
                        if let Some(a) = app.ud.aliases.get(info.id) {
                            job.append(&format!("“{a}”"), 8.0, egui::TextFormat { font_id: FontId::proportional(12.0), color: pal.text_dim, ..Default::default() });
                        }
                        let g = ui.fonts_mut(|f| f.layout_job(job));
                        if compact {
                            ui.painter().galley(pos2(tx, r.center().y - g.size().y / 2.0), g, tcol);
                        } else {
                            ui.painter().galley(pos2(tx, r.top() + 4.0), g, tcol);
                            // Matches are highlighted in the description too.
                            let mut dj = highlight_job(info.desc, &q, FontId::proportional(11.5), pal.text_dim, pal.accent.gamma_multiply(0.85));
                            dj.wrap = egui::text::TextWrapping { max_width: (r.right() - tx) * 0.7, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                            let dg = ui.fonts_mut(|f| f.layout_job(dj));
                            ui.painter().galley(pos2(tx, r.top() + 22.0), dg, pal.text_dim);
                        }
                        // Right side: chips, then (on hover/selection) + and ☆.
                        let mut x = r.right() - 8.0;
                        for b in chips.iter().rev() {
                            let label = commands::binding_to_string(b);
                            let w = crate::ui::paint_chip(ui.painter(), pos2(x, r.center().y), &label, &pal);
                            let cr = Rect::from_min_max(pos2(x - w, r.center().y - 9.0), pos2(x, r.center().y + 9.0));
                            let cresp = ui.interact(cr, Id::new(("chip", i, &label)), Sense::click()).on_hover_text("Click to remove this hotkey");
                            if cresp.clicked() {
                                remove = Some((*c, b.clone()));
                            }
                            x -= w + 4.0;
                        }
                        if sel || hovered {
                            if overridden {
                                let rr = Rect::from_center_size(pos2(x - 10.0, r.center().y), Vec2::splat(20.0));
                                let rresp = ui.interact(rr, Id::new(("reset", i)), Sense::click()).on_hover_text("Reset hotkeys to default");
                                paint_glyph(ui.painter(), rr.shrink(3.0), Glyph::Refresh, if rresp.hovered() { pal.text } else { pal.text_dim });
                                if rresp.clicked() {
                                    reset = Some(*c);
                                }
                                x -= 24.0;
                            }
                            let pr = Rect::from_center_size(pos2(x - 10.0, r.center().y), Vec2::splat(20.0));
                            let presp = ui.interact(pr, Id::new(("addkey", i)), Sense::click()).on_hover_text("Add a hotkey");
                            if presp.hovered() {
                                ui.painter().rect_filled(pr, CornerRadius::same(4), pal.raised);
                            }
                            paint_glyph(ui.painter(), pr.shrink(4.0), Glyph::Plus, pal.text_dim);
                            if presp.clicked() {
                                add_key = Some(*c);
                            }
                            x -= 24.0;
                            let ag = ui.painter().layout_no_wrap("alias".into(), FontId::proportional(11.0), pal.text_dim);
                            let ar = Rect::from_min_max(pos2(x - ag.size().x - 8.0, r.center().y - 10.0), pos2(x, r.center().y + 10.0));
                            crate::ui::note_control(ui, ar, "alias button");
                            let aresp = ui.interact(ar, Id::new(("alias", i)), Sense::click()).on_hover_text("Set the alias that finds this command (F2)");
                            if aresp.hovered() {
                                ui.painter().rect_filled(ar, CornerRadius::same(4), pal.raised);
                            }
                            ui.painter().galley(pos2(ar.left() + 4.0, r.center().y - ag.size().y / 2.0), ag, pal.text_dim);
                            if aresp.clicked() {
                                alias = Some(*c);
                            }
                            x = ar.left() - 4.0;
                            let sr = Rect::from_center_size(pos2(x - 10.0, r.center().y), Vec2::splat(20.0));
                            let sresp = ui.interact(sr, Id::new(("star", i)), Sense::click()).on_hover_text(if starred { "Remove from Quick" } else { "Add to Quick" });
                            crate::ui::star(ui.painter(), sr.center(), 6.5, if starred { pal.accent } else { pal.text_dim }, starred);
                            if sresp.clicked() {
                                star = Some(*c);
                            }
                        } else if starred {
                            crate::ui::star(ui.painter(), pos2(x - 10.0, r.center().y), 5.0, pal.accent.gamma_multiply(0.7), true);
                        }
                        if resp.clicked() && remove.is_none() && add_key.is_none() && star.is_none() && reset.is_none() && alias.is_none() {
                            run = Some(*c);
                        }
                    }
                }
            }
        });
        hints(ui, &[("↑↓", "navigate"), ("Enter", "run"), ("☆", "quick"), ("+", "add hotkey"), ("F2", "alias"), ("Esc", "close")], &pal);
    });
    if toggle_compact {
        app.cfg.compact_lists = !app.cfg.compact_lists;
        app.set_config_changed();
    }
    if let Some(cat) = toggle_group {
        let name = cat.name().to_string();
        if let Some(i) = app.ud.collapsed_groups.iter().position(|g| *g == name) {
            app.ud.collapsed_groups.remove(i);
        } else {
            app.ud.collapsed_groups.push(name);
        }
        app.ud_dirty = true;
    }
    if let Some(c) = star {
        let id = c.info().id.to_string();
        if let Some(i) = app.ud.quick.iter().position(|q| *q == id) {
            app.ud.quick.remove(i);
        } else {
            app.ud.quick.push(id);
        }
        app.ud_dirty = true;
    }
    if let Some(c) = alias {
        let text = app.ud.aliases.get(c.info().id).cloned().unwrap_or_default();
        edit_alias(app, c, text, format!("Alias for “{}” (empty removes it)", c.info().title));
        return false;
    }
    if let Some(c) = add_key {
        st.capture = Some(Capture { cmd: c, seq: vec![], last: Instant::now() });
        return true;
    }
    if let Some((c, b)) = remove {
        remove_binding(app, c, &b);
    }
    if let Some(c) = reset {
        app.cfg.hotkeys.remove(c.info().id);
        app.set_config_changed();
    }
    if let Some(c) = run {
        if applicable(app, c) {
            app.actions.push(Action::Cmd(c));
        }
        return false;
    }
    !(close || clicked_outside(ctx, rect))
}

/// Set, change or (with empty text) clear a command's alias. An alias another command
/// already answers to is refused: typing it could only ever find one of them first.
pub fn set_alias(ud: &mut crate::config::UserData, c: Cmd, text: &str) -> Result<(), Cmd> {
    let (id, t) = (c.info().id, text.trim());
    if let Some(other) = ud.aliases.iter().filter(|(k, a)| *k != id && a.eq_ignore_ascii_case(t)).find_map(|(k, _)| Cmd::from_id(k)) {
        return Err(other);
    }
    if t.is_empty() {
        ud.aliases.remove(id);
    } else {
        ud.aliases.insert(id.to_string(), t.to_string());
    }
    Ok(())
}

/// The alias prompt; a refused alias brings it back with the reason, and saving returns
/// to the palette.
fn edit_alias(app: &mut App, c: Cmd, text: String, title: String) {
    app.popup = Popup::prompt(&title, text, Box::new(move |app: &mut App, t: String| match set_alias(&mut app.ud, c, &t) {
        Ok(()) => {
            app.ud_dirty = true;
            app.popup = Popup::palette();
        }
        Err(o) => edit_alias(app, c, t.clone(), format!("“{}” already finds “{}”: pick another for “{}”", t.trim(), o.info().title, c.info().title)),
    }));
}

fn current_bindings(app: &App, c: Cmd) -> Vec<String> {
    app.keymap.bindings(c).iter().map(commands::binding_to_string).collect()
}

fn add_binding(app: &mut App, c: Cmd, b: Binding) {
    let mut v = current_bindings(app, c);
    let s = commands::binding_to_string(&b);
    if !v.contains(&s) {
        v.push(s);
    }
    app.cfg.hotkeys.insert(c.info().id.to_string(), v);
    app.set_config_changed();
}

fn remove_binding(app: &mut App, c: Cmd, b: &Binding) {
    let s = commands::binding_to_string(b);
    let v: Vec<String> = current_bindings(app, c).into_iter().filter(|x| *x != s).collect();
    app.cfg.hotkeys.insert(c.info().id.to_string(), v);
    app.set_config_changed();
}

// ------------------------------------------------------------------ options

#[derive(Clone, Copy, PartialEq)]
enum Opt {
    Theme,
    Font,
    InspectorFontSize,
    RectButtons,
    AssetCache,
    FollowSystem,
    Round,
    FontSize,
    Spacing,
    Animations,
    Tooltips,
    AltRows,
    InactiveOpacity,
    Breadcrumbs,
    CompactPaths,
    ShowExt,
    ShowHidden,
    ShowSystem,
    FoldersFirst,
    HighlightRecents,
    RecentsOnTop,
    TrackRecents,
    CalcSizes,
    SizesSsd,
    SizesHdd,
    SizesNet,
    SelectionBoxes,
    InspectorFont,
    SystemIcons,
    Thumbnails,
    ThumbFill,
    ScrollBeyond,
    DateFormat,
    ActOnPress,
    SingleClick,
    SingleClickFolders,
    OpenFoldersNewTab,
    DblClickParent,
    ReverseScroll,
    CharKey,
    MiddleClick,
    InspectorLayout,
    Startup,
    NewTab,
    DefaultFm,
    SingleInstance,
}

const OPTS: &[(Opt, &str, &str, &str)] = &[
    (Opt::Theme, "Appearance", "Color scheme", "Overall colors of the window."),
    (Opt::Font, "Appearance", "Font", "UI font family; empty uses the system font."),
    (Opt::FollowSystem, "Appearance", "Follow system light/dark", "Switch to the light scheme when the desktop is light."),
    (Opt::Round, "Appearance", "Round corners", "Rounded panels, buttons and selections."),
    (Opt::FontSize, "Appearance", "Font size", "Size of all text."),
    (Opt::SystemIcons, "Appearance", "System icon theme", "Use the desktop's file and folder icons; off uses Maia's built-in icons."),
    (Opt::InspectorFont, "Appearance", "Inspector font", "Monospace family for text previews; empty uses the system monospace font."),
    (Opt::InspectorFontSize, "Appearance", "Inspector font size", "Text size in inspector previews."),
    (Opt::RectButtons, "Appearance", "Rectangular window buttons", "Square instead of rounded minimize / maximize / close."),
    (Opt::Spacing, "Appearance", "Spacing", "From compact (more rows) to comfortable."),
    (Opt::Animations, "Appearance", "Animations", "Turn every animation on or off."),
    (Opt::Tooltips, "Appearance", "Tooltips", "Show hints when hovering buttons."),
    (Opt::AltRows, "Appearance", "Alternating row colors", "Stripe rows in Details view."),
    (Opt::InactiveOpacity, "Appearance", "Inactive panel opacity", "How visible unfocused panels stay."),
    (Opt::Breadcrumbs, "Appearance", "Path breadcrumbs", "Clickable path segments instead of plain text."),
    (Opt::CompactPaths, "Appearance", "Compact paths", "Show ~ for your home folder."),
    (Opt::ShowExt, "Listing", "Show file extensions", "Display .txt, .png and so on."),
    (Opt::ShowHidden, "Listing", "Show hidden files", "Show dotfiles."),
    (Opt::ShowSystem, "Listing", "Show system files", "Show backups (*~, *.bak), sockets and device files."),
    (Opt::FoldersFirst, "Listing", "Folders first", "List folders before files."),
    (Opt::HighlightRecents, "Listing", "Highlight recents", "Mark recently used items with a dot."),
    (Opt::RecentsOnTop, "Listing", "Recents on top", "Sort recently used items first."),
    (Opt::TrackRecents, "Listing", "Remember recents", "Keep visited folders and opened files: across restarts, for this session, or not at all."),
    (Opt::CalcSizes, "Listing", "Calculate folder sizes", "Scan folders in the background to show their sizes and item counts."),
    (Opt::SizesSsd, "Listing", "Solid-state drives", "Scan folders on SSDs and other fast storage."),
    (Opt::SizesHdd, "Listing", "Spinning hard drives", "Scan folders on rotational disks (slower, noisier)."),
    (Opt::SizesNet, "Listing", "Network locations", "Scan folders on NFS, SMB and other remote mounts."),
    (Opt::Thumbnails, "Listing", "Load thumbnails", "Where image previews are generated for icon views."),
    (Opt::SelectionBoxes, "Mouse & keyboard", "Selection boxes", "Checkboxes on items for picking several with the mouse."),
    (Opt::ThumbFill, "Listing", "Thumbnail fill", "Crop thumbnails to fill the square instead of fitting."),
    (Opt::ScrollBeyond, "Listing", "Scroll beyond last item", "Allow scrolling past the end of the list."),
    (Opt::AssetCache, "Listing", "Asset cache size", "Memory for thumbnails and previews (MB)."),
    (Opt::DateFormat, "Listing", "Date format", "strftime pattern, e.g. %Y-%m-%d %H:%M."),
    (Opt::ActOnPress, "Mouse & keyboard", "Act on press", "Select on mouse down instead of release."),
    (Opt::SingleClick, "Mouse & keyboard", "Single click to open", "Open items with one click."),
    (Opt::SingleClickFolders, "Mouse & keyboard", "…folders only", "Single click opens folders but not files."),
    (Opt::OpenFoldersNewTab, "Mouse & keyboard", "Open folders in new tab", "Double-clicked folders open in a new tab."),
    (Opt::DblClickParent, "Mouse & keyboard", "Double-click background goes up", "Double-click empty space to open the parent."),
    (Opt::ReverseScroll, "Mouse & keyboard", "Reverse scroll direction", "Invert the mouse wheel."),
    (Opt::CharKey, "Mouse & keyboard", "Character keys", "What typing letters in the list does."),
    (Opt::MiddleClick, "Mouse & keyboard", "Middle-click on folder", "Open in a split or a new tab."),
    (Opt::InspectorLayout, "Inspector & startup", "Inspector layout", "Where the inspector docks."),
    (Opt::Startup, "Inspector & startup", "On startup", "What to show when Maia starts."),
    (Opt::NewTab, "Inspector & startup", "New tab shows", "Folder opened by a new tab."),
    (Opt::DefaultFm, "System", "Default file manager", "Open folders from other apps in Maia."),
    (Opt::SingleInstance, "System", "Reuse the open window", "Folders from other apps and new launches open as tabs here."),
];

/// Choice control: a box showing the value and a chevron; click steps
/// to the next value, right-click (or Shift+click) to the previous one.
fn choice<T: PartialEq + Clone>(ui: &mut Ui, id: &str, cur: &mut T, opts: &[(T, &str)]) -> bool {
    let _ = id;
    let i = opts.iter().position(|(v, _)| v == cur).unwrap_or(0);
    let label = opts.get(i).map(|(_, l)| *l).unwrap_or("—");
    let (r, resp) = ui.allocate_exact_size(vec2(130.0, 20.0), Sense::click());
    let v = ui.visuals();
    let (fill, stroke, text) = (v.widgets.inactive.bg_fill, v.widgets.noninteractive.bg_stroke, v.text_color());
    let fill = if resp.hovered() { v.widgets.hovered.bg_fill } else { fill };
    ui.painter().rect(r, CornerRadius::same(4), fill, stroke, StrokeKind::Inside);
    ui.painter().text(r.center() - vec2(6.0, 0.0), Align2::CENTER_CENTER, label, FontId::proportional(11.5), text);
    paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.right() - 10.0, r.center().y), Vec2::splat(10.0)), Glyph::ChevronRight, text.gamma_multiply(0.7));
    let names: Vec<&str> = opts.iter().map(|(_, l)| *l).collect();
    let resp = resp.on_hover_text(names.join(" · "));
    let back = resp.secondary_clicked() || (resp.clicked() && ui.input(|i| i.modifiers.shift));
    if (resp.clicked() || back) && !opts.is_empty() {
        let n = opts.len();
        let j = if back { (i + n - 1) % n } else { (i + 1) % n };
        *cur = opts[j].0.clone();
        return true;
    }
    false
}

/// Switch: a pill whose knob carries a check (on) or a cross (off).
fn toggle(ui: &mut Ui, on: &mut bool, pal: &Palette) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(32.0, 16.0), Sense::click());
    crate::ui::note_control(ui, r, "switch");
    let t = ui.ctx().animate_bool(resp.id, *on);
    let lerp_c = |a: Color32, b: Color32| Color32::from_rgb(egui::lerp(a.r() as f32..=b.r() as f32, t) as u8, egui::lerp(a.g() as f32..=b.g() as f32, t) as u8, egui::lerp(a.b() as f32..=b.b() as f32, t) as u8);
    let off_track = Color32::from_rgb(((pal.border.r() as u16 * 2 + pal.text_dim.r() as u16) / 3) as u8, ((pal.border.g() as u16 * 2 + pal.text_dim.g() as u16) / 3) as u8, ((pal.border.b() as u16 * 2 + pal.text_dim.b() as u16) / 3) as u8);
    ui.painter().rect_filled(r, CornerRadius::same(8), lerp_c(off_track, pal.accent));
    let x = egui::lerp(r.left() + 8.0..=r.right() - 8.0, t);
    let c = pos2(x, r.center().y);
    ui.painter().circle_filled(c, 6.0, lerp_c(pal.bg, Color32::from_rgb(0xe8, 0xf4, 0xfa)));
    let st = Stroke::new(1.3, lerp_c(pal.text_dim, pal.accent));
    if t > 0.5 {
        ui.painter().add(egui::Shape::line(vec![c + vec2(-2.6, 0.2), c + vec2(-0.8, 2.0), c + vec2(2.8, -1.8)], st));
    } else {
        ui.painter().line_segment([c + vec2(-2.0, -2.0), c + vec2(2.0, 2.0)], st);
        ui.painter().line_segment([c + vec2(-2.0, 2.0), c + vec2(2.0, -2.0)], st);
    }
    if resp.clicked() {
        *on = !*on;
        return true;
    }
    false
}

fn stepper(ui: &mut Ui, v: &mut f32, step: f32, min: f32, max: f32, pal: &Palette) -> bool {
    let mut changed = false;
    if ui.add(egui::Button::new("+").min_size(vec2(22.0, 20.0))).clicked() {
        *v = (*v + step).min(max);
        changed = true;
    }
    ui.label(egui::RichText::new(format!("{v:.1}")).color(pal.text));
    if ui.add(egui::Button::new("−").min_size(vec2(22.0, 20.0))).clicked() {
        *v = (*v - step).max(min);
        changed = true;
    }
    changed
}

/// Is option `o` at its default value? `reset` restores it.
fn opt_default(c: &mut crate::config::Config, o: Opt, reset: bool) -> bool {
    let d = crate::config::Config::default();
    macro_rules! f {
        ($($field:ident),*) => {{
            let same = true $(&& c.$field == d.$field)*;
            if reset { $(c.$field = d.$field.clone();)* }
            same
        }};
    }
    match o {
        Opt::Theme => f!(theme),
        // Lives in the desktop's settings, not Maia's: nothing to reset.
        Opt::DefaultFm => true,
        Opt::SingleInstance => f!(single_instance),
        Opt::Font => f!(font_family),
        Opt::InspectorFontSize => f!(inspector_font_size),
        Opt::RectButtons => f!(rect_buttons),
        Opt::AssetCache => f!(asset_cache_mb),
        Opt::FollowSystem => f!(follow_system_theme),
        Opt::Round => f!(round_corners),
        Opt::FontSize => f!(font_size),
        Opt::Spacing => f!(spacing),
        Opt::Animations => f!(animations),
        Opt::Tooltips => f!(tooltips),
        Opt::AltRows => f!(alt_rows),
        Opt::InactiveOpacity => f!(inactive_opacity),
        Opt::Breadcrumbs => f!(breadcrumbs),
        Opt::CompactPaths => f!(compact_paths),
        Opt::ShowExt => f!(show_ext),
        Opt::ShowHidden => f!(show_hidden),
        Opt::ShowSystem => f!(show_system),
        Opt::FoldersFirst => f!(folders_first),
        Opt::HighlightRecents => f!(highlight_recents),
        Opt::RecentsOnTop => f!(recents_on_top),
        Opt::TrackRecents => f!(recents_store),
        Opt::CalcSizes => f!(folder_sizes),
        Opt::SizesSsd => f!(sizes_ssd),
        Opt::SizesHdd => f!(sizes_hdd),
        Opt::SizesNet => f!(sizes_network),
        Opt::SelectionBoxes => f!(selection_boxes),
        Opt::SystemIcons => f!(system_icons),
        Opt::InspectorFont => f!(inspector_font),
        Opt::Thumbnails => f!(thumbnails),
        Opt::ThumbFill => f!(thumb_fill),
        Opt::ScrollBeyond => f!(scroll_beyond),
        Opt::DateFormat => f!(date_format),
        Opt::ActOnPress => f!(act_on_press),
        Opt::SingleClick => f!(single_click),
        Opt::SingleClickFolders => f!(single_click_folders_only),
        Opt::OpenFoldersNewTab => f!(open_folders_new_tab),
        Opt::DblClickParent => f!(dblclick_bg_parent),
        Opt::ReverseScroll => f!(reverse_scroll),
        Opt::CharKey => f!(char_key_action),
        Opt::MiddleClick => f!(middle_click),
        Opt::InspectorLayout => f!(inspector_layout),
        Opt::Startup => f!(startup),
        Opt::NewTab => f!(new_tab),
    }
}

fn option_control(ui: &mut Ui, app: &mut App, o: Opt) -> bool {
    let pal = app.pal;
    let layouts: Vec<String> = app.ud.layouts.keys().cloned().collect();
    let mut app_preview = None;
    let changed = option_control_inner(ui, app, o, &pal, &layouts, &mut app_preview);
    if app_preview.is_some() {
        app.preview_theme = app_preview;
    }
    changed
}

fn option_control_inner(ui: &mut Ui, app: &mut App, o: Opt, pal: &Palette, layouts: &[String], app_preview: &mut Option<ThemeName>) -> bool {
    let pal = *pal;
    if o == Opt::DefaultFm {
        let mut on = app.default_fm() == Some(true);
        if toggle(ui, &mut on, &pal) {
            app.set_default_fm(on);
        }
        return false;
    }
    let c = &mut app.cfg;
    match o {
        Opt::Font => ui.add(egui::TextEdit::singleline(&mut c.font_family).hint_text("System font").desired_width(150.0)).lost_focus(),
        Opt::InspectorFontSize => stepper(ui, &mut c.inspector_font_size, 0.5, 8.0, 28.0, &pal),
        Opt::RectButtons => toggle(ui, &mut c.rect_buttons, &pal),
        Opt::AssetCache => ui.add(egui::Slider::new(&mut c.asset_cache_mb, 32..=2048).suffix(" MB").logarithmic(true)).changed(),
        Opt::Theme => {
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for t in ThemeName::ALL {
                    let p = theme::palette(t);
                    // Cell with room for the selection ring around the swatch.
                    let (cell, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
                    if resp.hovered() {
                        *app_preview = Some(t);
                    }
                    let r = Rect::from_center_size(cell.center(), vec2(24.0, 18.0));
                    // The theme's own window: panel color, its border, its accent bar centred
                    // near the bottom (equal margins on both sides).
                    ui.painter().rect_filled(r, CornerRadius::same(5), p.panel);
                    ui.painter().rect_stroke(r, CornerRadius::same(5), Stroke::new(1.0, p.border), StrokeKind::Inside);
                    let bar = Rect::from_center_size(pos2(r.center().x, r.bottom() - 5.0), vec2(r.width() - 10.0, 3.0));
                    ui.painter().rect_filled(bar, CornerRadius::same(2), p.accent);
                    // Selected: a ring outside the swatch with a small gap; hover: a faint one.
                    if c.theme == t {
                        ui.painter().rect_stroke(r.expand(2.5), CornerRadius::same(7), Stroke::new(1.5, pal.accent), StrokeKind::Middle);
                    } else if resp.hovered() {
                        ui.painter().rect_stroke(r.expand(2.5), CornerRadius::same(7), Stroke::new(1.0, pal.text_dim.gamma_multiply(0.6)), StrokeKind::Middle);
                    }
                    if resp.on_hover_text(t.label()).clicked() {
                        c.theme = t;
                        changed = true;
                    }
                }
            });
            changed
        }
        Opt::FollowSystem => toggle(ui, &mut c.follow_system_theme, &pal),
        Opt::Round => toggle(ui, &mut c.round_corners, &pal),
        Opt::FontSize => stepper(ui, &mut c.font_size, 0.5, 9.0, 24.0, &pal),
        Opt::Spacing => ui.add(egui::Slider::new(&mut c.spacing, 0.8..=1.6).step_by(0.05)).changed(),
        Opt::Animations => toggle(ui, &mut c.animations, &pal),
        Opt::Tooltips => toggle(ui, &mut c.tooltips, &pal),
        Opt::AltRows => toggle(ui, &mut c.alt_rows, &pal),
        Opt::InactiveOpacity => ui.add(egui::Slider::new(&mut c.inactive_opacity, 0.4..=1.0)).changed(),
        Opt::Breadcrumbs => toggle(ui, &mut c.breadcrumbs, &pal),
        Opt::CompactPaths => toggle(ui, &mut c.compact_paths, &pal),
        Opt::ShowExt => toggle(ui, &mut c.show_ext, &pal),
        Opt::ShowHidden => toggle(ui, &mut c.show_hidden, &pal),
        Opt::ShowSystem => toggle(ui, &mut c.show_system, &pal),
        Opt::FoldersFirst => toggle(ui, &mut c.folders_first, &pal),
        Opt::HighlightRecents => toggle(ui, &mut c.highlight_recents, &pal),
        Opt::RecentsOnTop => toggle(ui, &mut c.recents_on_top, &pal),
        Opt::TrackRecents => choice(ui, "recents", &mut c.recents_store, &[(RecentsStore::Persistent, "Persistent"), (RecentsStore::Session, "This session"), (RecentsStore::Off, "Off")]),
        Opt::CalcSizes => toggle(ui, &mut c.folder_sizes, &pal),
        Opt::SizesSsd => toggle(ui, &mut c.sizes_ssd, &pal),
        Opt::SizesHdd => toggle(ui, &mut c.sizes_hdd, &pal),
        Opt::SizesNet => toggle(ui, &mut c.sizes_network, &pal),
        Opt::SelectionBoxes => toggle(ui, &mut c.selection_boxes, &pal),
        Opt::SystemIcons => toggle(ui, &mut c.system_icons, &pal),
        Opt::SingleInstance => toggle(ui, &mut c.single_instance, &pal),
        Opt::DefaultFm => false,
        Opt::InspectorFont => {
            let r = ui.add(egui::TextEdit::singleline(&mut c.inspector_font).hint_text("monospace").desired_width(150.0));
            r.changed()
        }
        Opt::Thumbnails => choice(ui, "thumbs", &mut c.thumbnails, &[(ThumbMode::LocalDisks, "Local disks"), (ThumbMode::AllDrives, "All drives"), (ThumbMode::Never, "Never")]),
        Opt::ThumbFill => toggle(ui, &mut c.thumb_fill, &pal),
        Opt::ScrollBeyond => toggle(ui, &mut c.scroll_beyond, &pal),
        Opt::DateFormat => ui.add(egui::TextEdit::singleline(&mut c.date_format).desired_width(150.0)).changed(),
        Opt::ActOnPress => toggle(ui, &mut c.act_on_press, &pal),
        Opt::SingleClick => toggle(ui, &mut c.single_click, &pal),
        Opt::SingleClickFolders => toggle(ui, &mut c.single_click_folders_only, &pal),
        Opt::OpenFoldersNewTab => toggle(ui, &mut c.open_folders_new_tab, &pal),
        Opt::DblClickParent => toggle(ui, &mut c.dblclick_bg_parent, &pal),
        Opt::ReverseScroll => toggle(ui, &mut c.reverse_scroll, &pal),
        Opt::CharKey => choice(ui, "charkey", &mut c.char_key_action, &[(CharKeyAction::TriggerHotkey, "Hotkeys, else filter"), (CharKeyAction::Filter, "Filter"), (CharKeyAction::Jump, "Jump to name")]),
        Opt::MiddleClick => choice(ui, "middle", &mut c.middle_click, &[(MiddleClick::OpenInSplit, "Open in split"), (MiddleClick::OpenInNewTab, "Open in new tab")]),
        Opt::InspectorLayout => choice(ui, "insp", &mut c.inspector_layout, &[(InspectorLayout::Auto, "Auto"), (InspectorLayout::Right, "Right"), (InspectorLayout::Bottom, "Bottom")]),
        Opt::Startup => {
            let mut opts: Vec<(Startup, String)> = vec![(Startup::LastSession, "Last session".into()), (Startup::Home, "Home folder".into())];
            for l in layouts {
                opts.push((Startup::Layout(l.clone()), format!("Layout: {l}")));
            }
            let refs: Vec<(Startup, &str)> = opts.iter().map(|(a, b)| (a.clone(), b.as_str())).collect();
            choice(ui, "startup", &mut c.startup, &refs)
        }
        Opt::NewTab => choice(ui, "newtab", &mut c.new_tab, &[(NewTabShows::Current, "Current folder"), (NewTabShows::Home, "Home"), (NewTabShows::Computer, "Computer")]),
    }
}

fn options(app: &mut App, ctx: &egui::Context, st: &mut OptState) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let q = fuzzy::Query::new(&st.query);
    let mut close = false;
    let mut changed = false;
    let mut cmd = None;
    // Keyboard: Up/Down pick a row, Enter flips a switch (or resets text fields' focus).
    let keys = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::Enter)));
    // Best matches first: the title (prefix, word, substring, fuzzy) outranks the
    // description; a loose subsequence across everything only fills in at the end
    // (on its own it matched half the list and Enter flipped the wrong switch).
    let needle = st.query.trim().to_lowercase();
    let rank = |title: &str, desc: &str, group: &str| -> Option<i32> {
        if needle.is_empty() {
            return Some(0);
        }
        let (t, d) = (title.to_lowercase(), desc.to_lowercase());
        if t.starts_with(&needle) {
            Some(3000)
        } else if t.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&needle)) {
            Some(2500)
        } else if t.contains(&needle) {
            Some(2000)
        } else if let Some(sc) = fuzzy::score(&q, &t) {
            Some(1000 + sc)
        } else if d.contains(&needle) || group.to_lowercase().contains(&needle) {
            Some(500)
        } else {
            fuzzy::score(&q, &format!("{t} {d}")).map(|sc| sc / 4)
        }
    };
    let mut ranked: Vec<(i32, usize)> = OPTS.iter().enumerate().filter_map(|(i, (_, group, title, desc))| rank(title, desc, group).map(|r| (r, i))).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let visible: Vec<usize> = ranked.into_iter().map(|(_, i)| i).collect();
    if keys.0 && st.sel > 0 {
        st.sel -= 1;
        st.scroll_to_sel = true;
    }
    if keys.1 && st.sel + 1 < visible.len() {
        st.sel += 1;
        st.scroll_to_sel = true;
    }
    st.sel = st.sel.min(visible.len().saturating_sub(1));
    if keys.2 {
        if let Some(&i) = visible.get(st.sel) {
            if OPTS[i].0 == Opt::DefaultFm {
                let on = app.default_fm() == Some(true);
                app.set_default_fm(!on);
            } else if opt_flip(&mut app.cfg, OPTS[i].0) {
                changed = true;
            }
        }
    }
    let compact = app.cfg.compact_lists;
    let mut toggle_compact = false;
    let (_, rect) = centered(ctx, "options", 660.0, &pal, |ui| {
        ui.horizontal(|ui| {
            let w = ui.available_width() - 58.0;
            ui.scope(|ui| {
                ui.set_width(w);
                search_field(ui, &mut st.query, "Select an option…", &mut st.focus, &pal, Glyph::Search);
            });
            if compact_button(ui, compact, &pal).clicked() {
                toggle_compact = true;
            }
            if crate::ui::glyph_button(ui, Glyph::Close, "Close (Esc)", true, &pal).clicked() {
                close = true;
            }
        });
        egui::ScrollArea::vertical().max_height(480.0).auto_shrink([false, true]).show(ui, |ui| {
            let mut last_group = "";
            for (vi, &oi) in visible.iter().enumerate() {
                let (o, group, title, desc) = &OPTS[oi];
                // A flat list: sections only add a little space.
                if *group != last_group {
                    if !last_group.is_empty() {
                        ui.add_space(4.0);
                    }
                    last_group = group;
                }
                // Sub-options sit indented under their switch and fade while it is off.
                let sub_of_sizes = matches!(o, Opt::SizesSsd | Opt::SizesHdd | Opt::SizesNet);
                let indent = if sub_of_sizes { 22.0 } else { 0.0 };
                let inert = sub_of_sizes && !app.cfg.folder_sizes;
                let h = if compact { 26.0 } else { 36.0 };
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
                if ui.rect_contains_pointer(r) {
                    ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                }
                if vi == st.sel {
                    ui.painter().rect_stroke(r.shrink(0.5), CornerRadius::same(6), Stroke::new(1.0, pal.accent.gamma_multiply(0.8)), StrokeKind::Inside);
                    if st.scroll_to_sel {
                        ui.scroll_to_rect(r, None);
                        st.scroll_to_sel = false;
                    }
                }
                let tc = if inert { pal.text_dim.gamma_multiply(0.6) } else { pal.text };
                if compact {
                    ui.painter().text(pos2(r.left() + 10.0 + indent, r.center().y), Align2::LEFT_CENTER, *title, FontId::proportional(12.5), tc);
                } else {
                    ui.painter().text(pos2(r.left() + 10.0 + indent, r.top() + 11.0), Align2::LEFT_CENTER, *title, FontId::proportional(12.5), tc);
                    ui.painter().text(pos2(r.left() + 10.0 + indent, r.top() + 25.0), Align2::LEFT_CENTER, *desc, FontId::proportional(10.5), pal.text_dim.gamma_multiply(if inert { 0.6 } else { 1.0 }));
                }
                let ctrl_rect = Rect::from_min_max(pos2(r.right() - 230.0, r.top() + 4.0), pos2(r.right() - 8.0, r.bottom() - 4.0));
                let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(ctrl_rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
                if inert {
                    cui.disable();
                }
                if option_control(&mut cui, app, *o) {
                    changed = true;
                }
                // Changed options show a reset button left of their control.
                if !opt_default(&mut app.cfg, *o, false) {
                    let left = cui.min_rect().left();
                    let rr = Rect::from_center_size(pos2(left - 16.0, r.center().y), Vec2::splat(20.0));
                    let rresp = ui.interact(rr, Id::new(("optreset", *title)), Sense::click()).on_hover_text("Reset to default");
                    paint_glyph(ui.painter(), rr.shrink(3.0), Glyph::Refresh, if rresp.hovered() { pal.text } else { pal.text_dim });
                    if rresp.clicked() {
                        opt_default(&mut app.cfg, *o, true);
                        changed = true;
                    }
                }
            }
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                for (label, c) in [("Open config file", Cmd::OpenConfig), ("Reset to defaults", Cmd::ResetConfig), ("Install Maia", Cmd::Install)] {
                    if ui.button(label).clicked() {
                        cmd = Some(c);
                    }
                }
            });
        });
        hints(ui, &[("Up", ""), ("Down", "to navigate"), ("Enter", "to toggle"), ("Esc", "to dismiss")], &pal);
    });
    if toggle_compact {
        app.cfg.compact_lists = !app.cfg.compact_lists;
        changed = true;
    }
    if changed {
        if app.cfg.follow_system_theme {
            if let Some(t) = ctx.system_theme() {
                app.cfg.theme = match t {
                    egui::Theme::Light => if app.cfg.theme.is_light() { app.cfg.theme } else { ThemeName::Paper },
                    egui::Theme::Dark if app.cfg.theme.is_light() => ThemeName::Midnight,
                    _ => app.cfg.theme,
                };
            }
        }
        app.set_config_changed();
        ctx.all_styles_mut(|s| s.interaction.tooltip_delay = if app.cfg.tooltips { 0.5 } else { 1e9 });
    }
    if let Some(c) = cmd {
        app.actions.push(Action::Cmd(c));
        if c == Cmd::ResetConfig {
            return false;
        }
    }
    let combo_open = egui::Popup::is_any_open(ctx);
    !(close || (!combo_open && clicked_outside(ctx, rect)))
}

// ------------------------------------------------------------------ small popups

fn types(app: &mut App, ctx: &egui::Context, panel: u64, query: &mut String) -> bool {
    let pal = app.pal;
    let (show_hidden, show_system) = (app.cfg.show_hidden, app.cfg.show_system);
    let Some(p) = app.panels.get_mut(&panel) else { return false };
    let fallback = pos2(p.rect.right() - 320.0, p.rect.bottom() - 380.0);
    let t = p.tab_mut();
    // Opens under the Type column's filter button.
    let anchor = t.funnel_at.unwrap_or(fallback);
    let counts = t.type_samples(show_hidden, show_system);
    let q = fuzzy::Query::new(query);
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let mut close = false;
    let (_, rect) = anchored(ctx, "types", anchor, 300.0, &pal, |ui| {
        // Header: filter, check all, uncheck all | close.
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 22.0), Sense::hover());
            paint_glyph(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(12.0)), Glyph::Search, pal.text_dim);
            let te = ui.add(egui::TextEdit::singleline(query).hint_text(format!("Filter {} types…", counts.len())).frame(false).desired_width(ui.available_width() - 84.0));
            if !te.has_focus() && query.is_empty() {
                te.request_focus();
            }
            let icon_btn = |ui: &mut Ui, checked: bool, tip: &str| -> bool {
                let (r, resp) = ui.allocate_exact_size(vec2(24.0, 22.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(r, CornerRadius::same(4), pal.row_hover);
                }
                let b = Rect::from_center_size(r.center(), Vec2::splat(13.0));
                ui.painter().rect_stroke(b, CornerRadius::same(2), Stroke::new(1.2, pal.text_dim), StrokeKind::Middle);
                let c = b.center();
                let st = Stroke::new(1.3, pal.text_dim);
                if checked {
                    ui.painter().add(egui::Shape::line(vec![c + vec2(-3.0, 0.0), c + vec2(-0.8, 2.4), c + vec2(3.2, -2.4)], st));
                } else {
                    ui.painter().line_segment([c + vec2(-2.6, -2.6), c + vec2(2.6, 2.6)], st);
                    ui.painter().line_segment([c + vec2(-2.6, 2.6), c + vec2(2.6, -2.6)], st);
                }
                resp.on_hover_text(tip).clicked()
            };
            if icon_btn(ui, true, "Show only these types (check all)") {
                t.settings.only_types = counts.iter().map(|(k, ..)| k.clone()).collect();
                t.dirty = true;
            }
            if icon_btn(ui, false, "Show every type (uncheck all)") {
                t.settings.only_types.clear();
                t.dirty = true;
            }
            let (sr, _) = ui.allocate_exact_size(vec2(7.0, 20.0), Sense::hover());
            ui.painter().line_segment([sr.center_top(), sr.center_bottom()], Stroke::new(1.0, pal.border));
            if crate::ui::glyph_button(ui, Glyph::Close, "Close", true, &pal).clicked() {
                close = true;
            }
        });
        let yy = ui.cursor().top() + 1.0;
        let xr = ui.min_rect().x_range();
        ui.painter().line_segment([pos2(xr.min - 5.0, yy), pos2(xr.max + 5.0, yy)], Stroke::new(1.0, pal.border));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            for (k, n, sample) in &counts {
                if !q.is_empty() && fuzzy::score(&q, &k.to_lowercase()).is_none() {
                    continue;
                }
                let on = t.settings.only_types.contains(k);
                // Checked types are highlighted like a selection; the checkbox sits at the right.
                let resp = row(ui, 24.0, on, &pal, |ui, r| {
                    let ic = Rect::from_center_size(pos2(r.left() + 13.0, r.center().y), Vec2::splat(16.0));
                    if !crate::icons::entry(ui.painter(), ic, sample) {
                        theme::paint_icon(ui.painter(), ic, theme::categorize(sample.ext(), sample.is_dir, sample.is_executable()), "", false);
                    }
                    let tc = if on { Color32::WHITE } else { pal.text };
                    let job = highlight_job(k, &q, FontId::proportional(12.5), tc, if on { Color32::WHITE } else { pal.accent });
                    let g = ui.fonts_mut(|f| f.layout_job(job));
                    ui.painter().galley(pos2(r.left() + 30.0, r.center().y - g.size().y / 2.0), g, tc);
                    let cb = Rect::from_center_size(pos2(r.right() - 14.0, r.center().y), Vec2::splat(14.0));
                    if on {
                        ui.painter().rect_filled(cb, CornerRadius::same(3), Color32::WHITE.gamma_multiply(0.18));
                        ui.painter().rect_stroke(cb, CornerRadius::same(3), Stroke::new(1.2, Color32::WHITE), StrokeKind::Inside);
                        ui.painter().add(egui::Shape::line(vec![cb.left_center() + vec2(3.0, 0.0), cb.center_bottom() + vec2(-1.0, -3.5), cb.right_top() + vec2(-3.0, 3.5)], Stroke::new(1.6, Color32::WHITE)));
                    } else {
                        ui.painter().rect_stroke(cb, CornerRadius::same(3), Stroke::new(1.2, pal.text_dim.gamma_multiply(0.6)), StrokeKind::Inside);
                    }
                    ui.painter().text(pos2(cb.left() - 10.0, r.center().y), Align2::RIGHT_CENTER, crate::ui::fmt_count(*n), FontId::proportional(11.0), if on { Color32::from_white_alpha(200) } else { pal.text_dim.gamma_multiply(0.8) });
                });
                if resp.clicked() {
                    if on {
                        t.settings.only_types.retain(|x| x != k);
                    } else {
                        t.settings.only_types.push(k.clone());
                    }
                    t.dirty = true;
                }
            }
        });
    });
    !(close || clicked_outside(ctx, rect))
}

/// A checkbox drawn to match the theme.
pub fn checkbox_glyph(p: &egui::Painter, r: Rect, on: bool, pal: &Palette) {
    if on {
        p.rect_filled(r, CornerRadius::same(3), pal.accent);
        p.add(egui::Shape::line(vec![r.left_center() + vec2(3.0, 0.0), r.center_bottom() + vec2(-1.0, -3.5), r.right_top() + vec2(-3.0, 3.5)], Stroke::new(1.6, Color32::WHITE)));
    } else {
        p.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.2, pal.text_dim.gamma_multiply(0.8)), StrokeKind::Inside);
    }
}

fn tab_search(app: &mut App, ctx: &egui::Context, query: &mut String, sel: &mut usize) -> bool {
    let pal = app.pal;
    let k = take_keys(ctx);
    if k.esc {
        return false;
    }
    let q = fuzzy::Query::new(query);
    // (label, path, Some((panel, idx)) or None for closed)
    let mut rows: Vec<(String, PathBuf, Option<(u64, usize)>)> = vec![];
    let mut leaves = vec![];
    app.tree.leaves(&mut leaves);
    for pid in leaves {
        for (i, t) in app.panels[&pid].tabs.iter().enumerate() {
            rows.push((t.title(), t.path.clone(), Some((pid, i))));
        }
    }
    for c in app.closed_tabs.iter().rev().take(20) {
        let name = c.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| c.path.display().to_string());
        rows.push((name, c.path.clone(), None));
    }
    if !q.is_empty() {
        rows.retain(|(l, p, _)| fuzzy::score(&q, &format!("{l} {}", p.display()).to_lowercase()).is_some());
    }
    step(sel, rows.len(), &k, |_| false);
    let mut pick = if k.enter { rows.get(*sel).cloned() } else { None };
    let mut focus = true;
    let (_, rect) = centered(ctx, "tabsearch", 520.0, &pal, |ui| {
        search_field(ui, query, "Search open and recently closed tabs…", &mut focus, &pal, Glyph::Search);
        egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
            let mut closed_header = false;
            for (i, (label, path, loc)) in rows.iter().enumerate() {
                if loc.is_none() && !closed_header {
                    closed_header = true;
                    ui.label(egui::RichText::new("RECENTLY CLOSED").size(10.5).color(pal.text_dim));
                }
                let resp = row(ui, 30.0, i == *sel, &pal, |ui, r| {
                    let job = highlight_job(label, &q, FontId::proportional(13.5), pal.text, pal.accent);
                    let g = ui.fonts_mut(|f| f.layout_job(job));
                    let gw = g.size().x;
                    ui.painter().galley(pos2(r.left() + 10.0, r.center().y - g.size().y / 2.0), g, pal.text);
                    let s = crate::listing::elided(ui, &path.display().to_string(), FontId::proportional(11.5), r.width() - gw - 30.0, pal.text_dim);
                    ui.painter().galley(pos2(r.left() + 20.0 + gw, r.center().y - s.size().y / 2.0), s, pal.text_dim);
                });
                if resp.clicked() {
                    pick = Some((label.clone(), path.clone(), *loc));
                }
            }
        });
    });
    if let Some((_, path, loc)) = pick {
        match loc {
            Some((pid, i)) => app.actions.push(Action::ActivateTab { panel: pid, idx: i }),
            None => {
                app.closed_tabs.retain(|c| c.path != path);
                let id = app.active;
                app.actions.push(Action::NewTab { panel: id, path, select: None, focus: true });
            }
        }
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn saved_view(app: &mut App, ctx: &egui::Context, panel: u64) -> bool {
    let pal = app.pal;
    let Some(p) = app.panels.get(&panel) else { return false };
    let anchor = pos2(p.rect.left() + 100.0, p.rect.top() + 70.0);
    let path = p.path();
    let settings = p.tab().settings.clone();
    let current = app.ud.views.get(&path).map(|v| v.scope);
    let mut choice_v: Option<Option<SaveScope>> = None;
    let mut set_default = false;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let (_, rect) = anchored(ctx, "savedview", anchor, 300.0, &pal, |ui| {
        ui.label(egui::RichText::new("Saved view settings").strong());
        ui.label(egui::RichText::new("View size, columns, sort, grouping and type filters.").small().color(pal.text_dim));
        ui.add_space(4.0);
        for (v, label) in [(None, "None (inherit)"), (Some(SaveScope::ThisFolder), "This folder"), (Some(SaveScope::Subfolders), "This folder and subfolders")] {
            if ui.radio(current == v, label).clicked() {
                choice_v = Some(v);
            }
        }
        ui.separator();
        if ui.button("Set as default for all folders").clicked() {
            set_default = true;
        }
    });
    if let Some(v) = choice_v {
        match v {
            None => {
                app.ud.views.remove(&path);
            }
            Some(scope) => {
                app.ud.views.insert(path.clone(), SavedView { scope, view: settings.clone() });
            }
        }
        if let Some(p) = app.panels.get_mut(&panel) {
            p.tab_mut().saved_scope = v;
        }
        app.ud_dirty = true;
    }
    if set_default {
        app.ud.default_view = Some(settings);
        app.ud_dirty = true;
        app.toast("Saved as the default view");
    }
    !clicked_outside(ctx, rect)
}

fn zoom(app: &mut App, ctx: &egui::Context, panel: u64, anchor: Pos2) -> bool {
    let pal = app.pal;
    let Some(p) = app.panels.get_mut(&panel) else { return false };
    let t = p.tab_mut();
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    // Opens above the footer chip. The named views sit beside a vertical slider, each
    // at the height of its zoom value, so the dense views bunch up at the bottom.
    let (w, track) = (176.0, 168.0);
    let (_, rect) = anchored(ctx, "zoom", pos2(anchor.x - w + 30.0, anchor.y - track - 40.0), w, &pal, |ui| {
        let (r, resp) = ui.allocate_exact_size(vec2(w - 16.0, track + 20.0), Sense::click_and_drag());
        let rail_x = r.right() - 10.0;
        let (y0, y1) = (r.bottom() - 10.0, r.top() + 10.0);
        let y_of = |v: f32| y0 + (y1 - y0) * v;
        let z = t.settings.zoom;
        let mut set = None;
        let painter = ui.painter();
        for (name, v) in crate::listing::VIEW_STOPS {
            let y = y_of(v);
            let lr = Rect::from_min_max(pos2(r.left(), y - 10.0), pos2(rail_x - 14.0, y + 10.0));
            let on = crate::listing::view_name(z) == name || (name == "M Icons" && (z - 0.3).abs() < 0.001);
            let hov = ui.rect_contains_pointer(lr);
            if on {
                painter.rect_filled(lr, CornerRadius::same(4), pal.accent);
            } else if hov {
                painter.rect_filled(lr, CornerRadius::same(4), pal.hover);
            }
            let col = if on { Color32::WHITE } else { pal.text };
            crate::listing::paint_view_glyph(painter, Rect::from_center_size(pos2(lr.left() + 12.0, y), Vec2::splat(11.0)), crate::listing::mode_for(v), col);
            painter.text(pos2(lr.left() + 26.0, y), Align2::LEFT_CENTER, name, FontId::proportional(12.5), col);
            if !on && v > 0.25 {
                painter.line_segment([pos2(lr.left() + 26.0, y + 10.0), pos2(lr.right(), y + 10.0)], Stroke::new(1.0, pal.border.gamma_multiply(0.6)));
            }
            if hov && resp.clicked() {
                set = Some(v);
            }
        }
        // Slider rail and knob.
        painter.line_segment([pos2(rail_x, y0), pos2(rail_x, y1)], Stroke::new(4.0, pal.border));
        painter.line_segment([pos2(rail_x, y0), pos2(rail_x, y_of(z))], Stroke::new(4.0, pal.accent.gamma_multiply(0.6)));
        painter.circle_filled(pos2(rail_x, y_of(z)), 7.0, pal.accent);
        if let Some(pp) = resp.interact_pointer_pos() {
            if resp.dragged() || (resp.clicked() && pp.x > rail_x - 16.0) {
                let v = ((pp.y - y0) / (y1 - y0)).clamp(0.0, 1.0);
                // Snap the dense views to their stops; icons stay continuous.
                set = Some(if v < 0.3 { (v * 10.0).round() / 10.0 } else { v });
            }
        }
        if let Some(v) = set {
            t.settings.zoom = v;
            t.scroll_to_cursor = true;
        }
    });
    !clicked_outside(ctx, rect)
}

fn layouts(app: &mut App, ctx: &egui::Context, delete: bool) -> bool {
    let pal = app.pal;
    let names: Vec<String> = app.ud.layouts.keys().cloned().collect();
    let k = take_keys(ctx);
    if k.esc {
        return false;
    }
    let mut pick = None;
    let mut focus = false;
    let mut dummy = String::new();
    let (_, rect) = centered(ctx, "layouts", 420.0, &pal, |ui| {
        let _ = (&mut focus, &mut dummy);
        ui.label(egui::RichText::new(if delete { "Delete layout" } else { "Load layout" }).strong());
        ui.add_space(4.0);
        if names.is_empty() {
            ui.label(egui::RichText::new("No saved layouts yet. Use “Save Layout As…”.").color(pal.text_dim));
        }
        for n in &names {
            if menu_item(ui, n, None, &pal) {
                pick = Some(n.clone());
            }
        }
    });
    if let Some(n) = pick {
        if delete {
            app.ud.layouts.remove(&n);
            app.ud_dirty = true;
            app.toast(format!("Deleted layout “{n}”"));
        } else if let Some(l) = app.ud.layouts.get(&n).cloned() {
            app.apply_layout(&l);
        }
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn prompt(app: &mut App, ctx: &egui::Context, title: &str, text: &mut String, on_ok: &mut Option<Box<dyn FnOnce(&mut App, String)>>) -> bool {
    let pal = app.pal;
    let mut ok = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let mut cancel = false;
    centered(ctx, "prompt", 420.0, &pal, |ui| {
        ui.label(egui::RichText::new(title).strong());
        let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
        r.request_focus();
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() {
                ok = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if ok {
        if let Some(f) = on_ok.take() {
            f(app, text.clone());
        }
        return false;
    }
    !cancel
}

fn connect(app: &mut App, ctx: &egui::Context, st: &mut ConnectState) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let uri = crate::network::normalize(&st.uri);
    let busy = app.connecting(&uri);
    // The dialog stays until the place opens; a failure comes back here as `error`.
    if let Some(e) = app.take_connect_error(&uri) {
        st.error = Some(e);
        st.password.clear();
    }
    let valid = crate::network::is_uri(&st.uri);
    let mut go = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) && valid && !busy;
    let mut cancel = false;
    let mut pick: Option<String> = None;
    centered(ctx, "connect", 440.0, &pal, |ui| {
        ui.label(egui::RichText::new("Connect to server").strong());
        ui.add_space(6.0);
        let field = |ui: &mut Ui, label: &str, id: &'static str, text: &mut String, hint: &str, secret: bool, focus: bool| {
            ui.label(egui::RichText::new(label).small().color(pal.text_dim));
            let r = ui.add_enabled(!busy, egui::TextEdit::singleline(text).hint_text(egui::RichText::new(hint).color(pal.text_dim)).password(secret).desired_width(f32::INFINITY));
            crate::ui::note_control(ui, r.rect, id);
            if focus {
                r.request_focus();
            }
        };
        field(ui, "Address", "address field", &mut st.uri, "smb://server/share, sftp://user@host, ftp://host", false, st.focus);
        st.focus = false;
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            field(&mut cols[0], "User", "user field", &mut st.user, "Optional", false, false);
            field(&mut cols[1], "Password", "password field", &mut st.password, "Optional", true, false);
        });
        ui.add_space(2.0);
        ui.add_enabled(!busy, egui::Checkbox::new(&mut st.anonymous, "Connect as a guest"));
        if let Some(e) = &st.error {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(e).color(pal.danger));
        }
        if !app.ud.servers.is_empty() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Recent").small().color(pal.text_dim));
            for s in app.ud.servers.iter().take(5) {
                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(r, CornerRadius::same(5), pal.hover);
                }
                paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.left() + 12.0, r.center().y), Vec2::splat(13.0)), Glyph::Globe, pal.text_dim);
                let g = crate::listing::elided(ui, s, FontId::proportional(app.cfg.font_size - 1.0), r.width() - 30.0, pal.text);
                ui.painter().galley(pos2(r.left() + 26.0, r.center().y - g.size().y / 2.0), g, pal.text);
                if resp.clicked() && !busy {
                    pick = Some(s.clone());
                }
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(valid && !busy, egui::Button::new(egui::RichText::new("Connect").color(Color32::WHITE)).fill(pal.accent)).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if busy {
                    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                    crate::ui::spinner(ui.painter(), r.center(), 6.0, pal.accent, ui.input(|i| i.time));
                    ui.ctx().request_repaint();
                }
            });
        });
    });
    if let Some(s) = pick {
        st.uri = s;
        st.error = None;
    }
    if go {
        st.error = None;
        let login = crate::network::Login { user: st.user.trim().to_string(), password: st.password.clone(), anonymous: st.anonymous };
        let panel = app.active;
        app.connect_from_dialog(uri, login, panel);
    }
    !cancel && !app.connected(&crate::network::normalize(&st.uri))
}

fn confirm(app: &mut App, ctx: &egui::Context, msg: &str, ok_label: &str, on_ok: &mut Option<Box<dyn FnOnce(&mut App)>>) -> bool {
    let pal = app.pal;
    let mut ok = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
    let mut cancel = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    // Dim the background for destructive confirmations.
    let screen = ctx.content_rect();
    ctx.layer_painter(egui::LayerId::new(egui::Order::Middle, Id::new("dim"))).rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(90));
    centered(ctx, "confirm", 440.0, &pal, |ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(msg).size(14.0));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(egui::RichText::new(ok_label).color(Color32::WHITE)).fill(pal.danger)).clicked() {
                    ok = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    });
    if ok {
        if let Some(f) = on_ok.take() {
            f(app);
        }
        return false;
    }
    !cancel
}

fn conflict(app: &mut App, ctx: &egui::Context, st: &mut ConflictState) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let Some(item) = st.items.get(st.idx) else { return false };
    let both_dirs = item.src.is_dir() && item.dst.is_dir();
    let info = |p: &Path| -> String {
        let e = fsx::Entry::from_path(p);
        match e {
            Some(e) if e.is_dir => format!("Folder · modified {}", fsx::fmt_time(e.modified)),
            Some(e) => format!("{} · modified {}", fsx::fmt_size(e.size), fsx::fmt_time(e.modified)),
            None => String::new(),
        }
    };
    let mut decision: Option<Conflict> = None;
    let mut cancel = false;
    let remaining = st.items.len() - st.idx;
    centered(ctx, "conflict", 520.0, &pal, |ui| {
        let name = item.dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        ui.label(egui::RichText::new(format!("“{name}” already exists")).strong().size(15.0));
        ui.label(egui::RichText::new(format!("in {}", st.dest.display())).color(pal.text_dim));
        ui.add_space(8.0);
        egui::Grid::new("cf").num_columns(2).spacing(vec2(12.0, 4.0)).show(ui, |ui| {
            ui.label(egui::RichText::new(if st.mv { "Moving" } else { "Copying" }).color(pal.text_dim));
            ui.label(info(&item.src));
            ui.end_row();
            ui.label(egui::RichText::new("Existing").color(pal.text_dim));
            ui.label(info(&item.dst));
            ui.end_row();
        });
        ui.add_space(8.0);
        if remaining > 1 {
            ui.checkbox(&mut st.apply_all, format!("Apply to all {remaining} conflicts"));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Replace").clicked() {
                decision = Some(Conflict::Overwrite);
            }
            if both_dirs && ui.button("Merge").on_hover_text("Copy into the existing folder, replacing files").clicked() {
                decision = Some(Conflict::Merge);
            }
            if ui.button("Keep both").clicked() {
                decision = Some(Conflict::KeepBoth);
            }
            if ui.button("Skip").clicked() {
                decision = Some(Conflict::Skip);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    });
    if cancel {
        return false;
    }
    if let Some(d) = decision {
        let range = if st.apply_all { st.idx..st.items.len() } else { st.idx..st.idx + 1 };
        for i in range {
            let it = &st.items[i];
            let d = if d == Conflict::Merge && !(it.src.is_dir() && it.dst.is_dir()) { Conflict::Overwrite } else { d };
            st.decide.insert(it.src.clone(), d);
        }
        st.idx = if st.apply_all { st.items.len() } else { st.idx + 1 };
        if st.idx >= st.items.len() {
            let decide = std::mem::take(&mut st.decide);
            let kind = if st.mv { OpKind::Move { srcs: st.srcs.clone(), dest: st.dest.clone(), decide } } else { OpKind::Copy { srcs: st.srcs.clone(), dest: st.dest.clone(), decide } };
            app.actions.push(Action::StartJob(kind));
            return false;
        }
    }
    true
}

fn user_name(uid: u32, file: &str) -> String {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|s| s.lines().find_map(|l| {
            let mut it = l.split(':');
            let name = it.next()?;
            it.next();
            (it.next()?.parse::<u32>().ok()? == uid).then(|| name.to_string())
        }))
        .unwrap_or_else(|| uid.to_string())
}

fn properties(app: &mut App, ctx: &egui::Context, st: &mut PropState) -> bool {
    use std::os::unix::fs::MetadataExt;
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let mut close = false;
    st.refresh();
    let snap = st.snap.take();
    let (_, rect) = centered(ctx, "props", 480.0, &pal, |ui| {
        let Some(snap) = &snap else { return };
        if st.paths.len() == 1 {
            let p = st.paths[0].clone();
            let Some((m, e)) = snap.single.clone() else {
                ui.label("Item no longer exists");
                return;
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                if !crate::icons::entry(ui.painter(), r, &e) {
                    theme::paint_icon(ui.painter(), r, theme::categorize(&e.ext(), e.is_dir, e.is_executable()), &e.ext(), e.is_link);
                }
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&e.name).strong().size(15.0));
                    let db = crate::mime::mime_db();
                    let mime = db.for_name(&e.name, e.is_dir, e.is_executable());
                    ui.label(egui::RichText::new(format!("{} · {mime}", db.comment(&mime))).color(pal.text_dim));
                });
            });
            ui.separator();
            egui::Grid::new("props").num_columns(2).spacing(vec2(14.0, 5.0)).show(ui, |ui| {
                let mut kv = |k: &str, v: String| {
                    ui.label(egui::RichText::new(k).color(pal.text_dim));
                    ui.add(egui::Label::new(v).wrap());
                    ui.end_row();
                };
                kv("Location", p.parent().map(|x| x.display().to_string()).unwrap_or_default());
                if e.is_dir {
                    match app.stats.map.get(&p) {
                        Some(s) => kv("Size", format!("{} ({} bytes)\n{} files, {} folders", fsx::fmt_size(s.size), s.size, s.files, s.folders)),
                        None => {
                            app.stats.request(&p);
                            kv("Size", "Calculating…".into())
                        }
                    }
                } else {
                    kv("Size", format!("{} ({} bytes)", fsx::fmt_size(e.size), e.size));
                }
                kv("Created", fsx::fmt_time(e.created));
                kv("Modified", fsx::fmt_time(e.modified));
                kv("Accessed", fsx::fmt_time(m.accessed().ok()));
                kv("Owner", format!("{} : {}", user_name(m.uid(), "/etc/passwd"), user_name(m.gid(), "/etc/group")));
                if let Some(t) = &snap.link {
                    kv("Link target", t.clone());
                }
                if let Some((total, free)) = snap.vol {
                    kv("Volume", format!("{} free of {}", fsx::fmt_size(free), fsx::fmt_size(total)));
                }
            });
            ui.separator();
            // Permissions
            let mode = st.mode.get_or_insert(m.mode() & 0o7777);
            ui.label(egui::RichText::new("Permissions").color(pal.text_dim));
            egui::Grid::new("perm").num_columns(4).show(ui, |ui| {
                ui.label("");
                for h in ["Read", "Write", "Execute"] {
                    ui.label(egui::RichText::new(h).small());
                }
                ui.end_row();
                for (who, shift) in [("Owner", 6), ("Group", 3), ("Others", 0)] {
                    ui.label(who);
                    for bit in [4u32, 2, 1] {
                        let mask = bit << shift;
                        let mut on = *mode & mask != 0;
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                *mode |= mask;
                            } else {
                                *mode &= !mask;
                            }
                        }
                    }
                    ui.end_row();
                }
            });
            let orig = m.mode() & 0o7777;
            let mode = *mode;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{:04o}  {}", mode, fsx::fmt_mode(mode, e.is_dir))).monospace());
                if mode != orig && ui.button("Apply").clicked() {
                    use std::os::unix::fs::PermissionsExt;
                    match std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)) {
                        Ok(()) => app.toast("Permissions updated"),
                        Err(err) => app.toast(format!("Couldn't change permissions: {err}")),
                    }
                }
            });
        } else {
            ui.label(egui::RichText::new(format!("{} items", st.paths.len())).strong().size(15.0));
            let mut total = 0u64;
            let mut pending = false;
            let (mut files, mut dirs) = (0, 0);
            for (p, &(is_dir, len)) in st.paths.iter().zip(snap.multi.iter()) {
                if is_dir {
                    dirs += 1;
                    match app.stats.map.get(p) {
                        Some(s) => total += s.size,
                        None => {
                            pending = true;
                            app.stats.request(p);
                        }
                    }
                } else {
                    files += 1;
                    total += len;
                }
            }
            ui.label(format!("{files} files, {dirs} folders"));
            ui.label(format!("Total size: {}{}", fsx::fmt_size(total), if pending { " (calculating…)" } else { "" }));
            if let Some(parent) = st.paths[0].parent() {
                ui.label(egui::RichText::new(format!("in {}", parent.display())).color(pal.text_dim));
            }
        }
        ui.add_space(6.0);
        if ui.button("Close").clicked() {
            close = true;
        }
    });
    ctx.request_repaint_after(std::time::Duration::from_millis(300));
    st.snap = snap;
    !(close || clicked_outside(ctx, rect))
}

fn open_with(app: &mut App, ctx: &egui::Context, paths: &mut Vec<PathBuf>, query: &mut String, all: &mut bool) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let db = crate::mime::mime_db();
    let first = paths[0].clone();
    let mime = db.for_path(&first, false);
    let apps = crate::mime::apps();
    let list: Vec<&crate::mime::App> = if *all { apps.apps.iter().filter(|a| !a.hidden).collect() } else { apps.for_mime(&mime) };
    let q = fuzzy::Query::new(query);
    let mut launch: Option<crate::mime::App> = None;
    let mut set_def: Option<String> = None;
    let mut focus = true;
    let (_, rect) = centered(ctx, "openwith", 480.0, &pal, |ui| {
        ui.label(egui::RichText::new(format!("Open {} with…", if paths.len() == 1 { first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { format!("{} files", paths.len()) })).strong());
        ui.label(egui::RichText::new(format!("{} ({mime})", db.comment(&mime))).small().color(pal.text_dim));
        search_field(ui, query, "Filter applications…", &mut focus, &pal, Glyph::Search);
        egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
            for (i, a) in list.iter().enumerate() {
                if !q.is_empty() && fuzzy::score(&q, &a.name.to_lowercase()).is_none() {
                    continue;
                }
                ui.horizontal(|ui| {
                    let label = if i == 0 && !*all { format!("{}  (default)", a.name) } else { a.name.clone() };
                    if ui.add(egui::Button::new(label).frame(false)).clicked() {
                        launch = Some((*a).clone());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Set default").clicked() {
                            set_def = Some(a.id.clone());
                        }
                    });
                });
            }
            if list.is_empty() {
                ui.label(egui::RichText::new("No registered applications for this type.").color(pal.text_dim));
            }
        });
        ui.checkbox(all, "Show all applications");
    });
    if let Some(id) = set_def {
        match crate::mime::set_default(&id, &mime) {
            Ok(()) => app.toast(format!("Default for {mime} updated")),
            Err(e) => app.toast(format!("Couldn't set default: {e}")),
        }
    }
    if let Some(a) = launch {
        if let Err(e) = crate::mime::launch(&a, paths, first.parent()) {
            app.toast(format!("Couldn't launch {}: {e}", a.name));
        }
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn run_exec(app: &mut App, ctx: &egui::Context, path: &mut PathBuf) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let mut done = false;
    centered(ctx, "runexec", 440.0, &pal, |ui| {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        ui.label(egui::RichText::new(format!("“{name}” is executable")).strong());
        ui.label(egui::RichText::new("Do you want to run it?").color(pal.text_dim));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Run").clicked() {
                if let Err(e) = crate::mime::run_with(path, &[]) {
                    app.toast(format!("Couldn't run: {e}"));
                }
                done = true;
            }
            if ui.button("Run in terminal").clicked() {
                let a = crate::mime::App { id: "run".into(), name: name.clone(), exec: format!("\"{}\"", path.display()), mimes: vec![], terminal: true, hidden: false, icon: String::new() };
                if let Err(e) = crate::mime::launch(&a, &[], path.parent()) {
                    app.toast(format!("Couldn't run: {e}"));
                }
                done = true;
            }
            if ui.button("Open").on_hover_text("Open with the default application").clicked() {
                app.actions.push(Action::OpenFile(path.clone()));
                done = true;
            }
            if ui.button("Open with…").clicked() {
                app.actions.push(Action::Open(Popup::open_with(vec![path.clone()])));
                done = true;
            }
            if ui.button("Cancel").clicked() {
                done = true;
            }
        });
    });
    !done
}

fn main_menu(app: &mut App, ctx: &egui::Context) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let mut run = None;
    let (_, rect) = anchored(ctx, "mainmenu", pos2(8.0, crate::ui::TITLE_H), 280.0, &pal, |ui| {
        for c in [Cmd::NewWindow, Cmd::NewTab] {
            if cmd_item(ui, app, c) {
                run = Some(c);
            }
        }
        menu_sep(ui, &pal);
        for c in [Cmd::SaveLayout, Cmd::LoadLayout, Cmd::DeleteLayout] {
            if cmd_item(ui, app, c) {
                run = Some(c);
            }
        }
        menu_sep(ui, &pal);
        for c in [Cmd::Options, Cmd::Palette, Cmd::GoTo, Cmd::OpenConfig, Cmd::RevealConfig] {
            if cmd_item(ui, app, c) {
                run = Some(c);
            }
        }
        menu_sep(ui, &pal);
        for c in [Cmd::ConnectServer, Cmd::Install, Cmd::SetDefault, Cmd::About, Cmd::Quit] {
            if cmd_item(ui, app, c) {
                run = Some(c);
            }
        }
    });
    if let Some(c) = run {
        app.actions.push(Action::Cmd(c));
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn quick_menu(app: &mut App, ctx: &egui::Context, panel: Option<(u64, Pos2)>) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let anchor = match panel {
        Some((_, a)) => pos2(a.x - 250.0, a.y + 4.0),
        None => {
            // Opens at the mouse pointer.
            let r = app.panels.get(&app.active).map(|p| p.rect).unwrap_or(ctx.content_rect());
            *app.quick_anchor.get_or_insert_with(|| ctx.pointer_latest_pos().unwrap_or(pos2(r.right() - 290.0, r.top() + 70.0)))
        }
    };
    let mut run = None;
    let quick: Vec<Cmd> = app.ud.quick.iter().filter_map(|id| Cmd::from_id(id)).collect();
    let group = app.panels.get(&app.active).map(|p| p.tab().settings.group).unwrap_or_default();
    let (_, rect) = anchored(ctx, "quickmenu", anchor, 280.0, &pal, |ui| {
        ui.label(egui::RichText::new("QUICK").size(10.5).color(pal.text_dim));
        for c in &quick {
            if cmd_item(ui, app, *c) {
                run = Some(*c);
            }
        }
        if panel.is_some() {
            menu_sep(ui, &pal);
            ui.label(egui::RichText::new("SORT").size(10.5).color(pal.text_dim));
            for c in [Cmd::SortName, Cmd::SortSize, Cmd::SortModified, Cmd::SortType, Cmd::ToggleFoldersFirst] {
                if cmd_item(ui, app, c) {
                    run = Some(c);
                }
            }
            menu_sep(ui, &pal);
            ui.label(egui::RichText::new("GROUP").size(10.5).color(pal.text_dim));
            for c in crate::commands::GROUP_CMDS {
                if !applicable(app, c) {
                    continue;
                }
                let g = c.group_key().unwrap_or_default();
                let label = format!("{}{}", if g == group { "● " } else { "" }, c.info().title.trim_start_matches("Group By "));
                if menu_item(ui, &label, None, &pal) {
                    run = Some(c);
                }
            }
            menu_sep(ui, &pal);
            for c in [Cmd::SavedView, Cmd::ToggleInspector, Cmd::ClosePanel] {
                if cmd_item(ui, app, c) {
                    run = Some(c);
                }
            }
        }
    });
    if let Some(c) = run {
        app.actions.push(Action::Cmd(c));
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn path_menu(app: &mut App, ctx: &egui::Context, panel: u64, path: &Path, anchor: Pos2) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let mut done = false;
    let computer = tab::is_computer(path);
    let (_, rect) = anchored(ctx, "pathmenu", anchor, 250.0, &pal, |ui| {
        if !computer {
            if menu_item(ui, "Copy path", None, &pal) {
                ctx.copy_text(path.display().to_string());
                done = true;
            }
            if menu_item(ui, "Copy path (quoted)", None, &pal) {
                ctx.copy_text(format!("\"{}\"", path.display()));
                done = true;
            }
            if menu_item(ui, "Open in new tab", None, &pal) {
                app.actions.push(Action::NewTab { panel, path: path.to_path_buf(), select: None, focus: true });
                done = true;
            }
            if menu_item(ui, "Open terminal here", None, &pal) {
                let _ = crate::mime::open_terminal(path);
                done = true;
            }
            menu_sep(ui, &pal);
        }
        let mut c = app.cfg.compact_paths;
        if ui.checkbox(&mut c, "Show compact paths").changed() {
            app.cfg.compact_paths = c;
            app.cfg_dirty = true;
        }
        let mut b = app.cfg.breadcrumbs;
        if ui.checkbox(&mut b, "Path breadcrumbs").changed() {
            app.cfg.breadcrumbs = b;
            app.cfg_dirty = true;
        }
    });
    !(done || clicked_outside(ctx, rect))
}

fn path_dropdown(app: &mut App, ctx: &egui::Context, st: &mut PathDropState) -> bool {
    let pal = app.pal;
    let k = take_keys(ctx);
    if k.esc {
        return false;
    }
    if k.alt_left {
        if let Some(parent) = st.dir.parent() {
            let child = st.dir.clone();
            *st = match Popup::path_dropdown(st.panel, parent.to_path_buf(), Some(child)) {
                Popup::PathDropdown(mut s) => {
                    s.anchor = st.anchor.map(|a| a - vec2(60.0, 0.0));
                    s
                }
                _ => unreachable!(),
            };
        }
    }
    if k.alt_right {
        if let Some(next) = st.next.clone().or_else(|| st.entries.get(st.sel).cloned()) {
            let anchor = st.anchor;
            if let Popup::PathDropdown(mut s) = Popup::path_dropdown(st.panel, next, None) {
                s.anchor = anchor.map(|a| a + vec2(60.0, 0.0));
                *st = s;
            }
        }
    }
    let q = fuzzy::Query::new(&st.query);
    let shown: Vec<PathBuf> = st.entries.iter().filter(|p| q.is_empty() || fuzzy::score(&q, &p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()).is_some()).cloned().collect();
    step(&mut st.sel, shown.len(), &k, |_| false);
    let mut go: Option<(PathBuf, bool)> = None;
    if k.enter || k.ctrl_enter {
        if let Some(p) = shown.get(st.sel) {
            go = Some((p.clone(), k.ctrl_enter));
        }
    }
    let anchor = st.anchor.unwrap_or_else(|| app.panels.get(&st.panel).map(|p| p.rect.left_top() + vec2(120.0, 66.0)).unwrap_or(Pos2::ZERO));
    let mut close = false;
    let n_items = st.entries.len();
    let (_, rect) = anchored(ctx, "pathdrop", anchor, 250.0, &pal, |ui| {
        // Search row: magnifier, "Filter N items…", close.
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 22.0), Sense::hover());
            paint_glyph(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(12.0)), Glyph::Search, pal.text_dim);
            let hint = format!("Filter {} {}…", n_items, if n_items == 1 { "item" } else { "items" });
            let r = ui.add(egui::TextEdit::singleline(&mut st.query).hint_text(hint).frame(false).desired_width(ui.available_width() - 24.0));
            if st.focus {
                r.request_focus();
                st.focus = false;
            }
            if crate::ui::glyph_button(ui, Glyph::Close, "Close (Esc)", true, &pal).clicked() {
                close = true;
            }
        });
        let yy = ui.cursor().top() + 1.0;
        let xr = ui.min_rect().x_range();
        ui.painter().line_segment([pos2(xr.min - 5.0, yy), pos2(xr.max + 5.0, yy)], Stroke::new(1.0, pal.border));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for (i, p) in shown.iter().enumerate() {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let highlighted = Some(p) == st.next.as_ref();
                let sel = i == st.sel;
                let resp = row(ui, 24.0, sel, &pal, |ui, r| {
                    let ic = Rect::from_center_size(pos2(r.left() + 14.0, r.center().y), Vec2::splat(16.0));
                    if let Some(e) = st.info.get(p) {
                        if !crate::icons::entry(ui.painter(), ic, e) {
                            theme::paint_icon(ui.painter(), ic, theme::categorize(&e.ext(), e.is_dir, e.is_executable()), "", e.is_link);
                        }
                    }
                    // The folder the path continues into is marked like the current crumb.
                    let col = if sel { Color32::WHITE } else if highlighted { pal.accent } else { pal.text };
                    // Long names end in "…" inside the row instead of running past the popup.
                    let g = crate::listing::elided(ui, &name, FontId::proportional(12.5), (r.width() - 38.0).max(20.0), col);
                    ui.painter().galley(pos2(r.left() + 30.0, r.center().y - g.size().y / 2.0), g, col);
                });
                if i == st.sel && (k.up || k.down) {
                    resp.scroll_to_me(None);
                }
                if resp.clicked() {
                    go = Some((p.clone(), false));
                }
                if resp.middle_clicked() {
                    go = Some((p.clone(), true));
                }
            }
            if shown.is_empty() {
                ui.label(egui::RichText::new("Empty folder").color(pal.text_dim));
            }
        });
    });
    if close {
        return false;
    }
    if let Some((p, new_tab)) = go {
        if !p.is_dir() {
            app.actions.push(Action::OpenFile(p));
        } else if new_tab {
            app.actions.push(Action::NewTab { panel: st.panel, path: p, select: None, focus: false });
        } else {
            app.actions.push(Action::Navigate { panel: st.panel, path: p });
        }
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn history(app: &mut App, ctx: &egui::Context, panel: u64, anchor: Pos2) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let Some(p) = app.panels.get(&panel) else { return false };
    let t = p.tab();
    let hist: Vec<(usize, PathBuf)> = t.history.iter().enumerate().map(|(i, h)| (i, h.path.clone())).rev().collect();
    let pos = t.hist_pos;
    let recents: Vec<PathBuf> = app.ud.recents.iter().take(10).cloned().collect();
    let mut go = None;
    let (_, rect) = anchored(ctx, "history", anchor, 320.0, &pal, |ui| {
        ui.label(egui::RichText::new("HISTORY").size(10.5).color(pal.text_dim));
        for (i, h) in hist.iter().take(15) {
            let name = if tab::is_computer(h) { "Computer".into() } else { h.display().to_string() };
            let label = if *i == pos { format!("● {name}") } else { name };
            if menu_item(ui, &label, None, &pal) {
                go = Some(h.clone());
            }
        }
        if !recents.is_empty() {
            menu_sep(ui, &pal);
            ui.label(egui::RichText::new("RECENT").size(10.5).color(pal.text_dim));
            for r in &recents {
                if menu_item(ui, &r.display().to_string(), None, &pal) {
                    go = Some(r.clone());
                }
            }
        }
    });
    if let Some(p) = go {
        app.actions.push(Action::Navigate { panel, path: p });
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn rename_menu(app: &mut App, ctx: &egui::Context, panel: u64, pos: Pos2) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let Some(p) = app.panels.get_mut(&panel) else { return false };
    let Some(r) = p.tab_mut().rename.as_mut() else { return false };
    let mut done = false;
    let mut find = false;
    let mut clip: Option<String> = None;
    let (_, rect) = anchored(ctx, "renamemenu", pos, 270.0, &pal, |ui| {
        if menu_item(ui, "Cut", Some("Ctrl+X".into()), &pal) {
            clip = Some(r.copy_text());
            r.insert_all("");
            done = true;
        }
        if menu_item(ui, "Copy", Some("Ctrl+C".into()), &pal) {
            clip = Some(r.copy_text());
            done = true;
        }
        if menu_item(ui, "Paste", Some("Ctrl+V".into()), &pal) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            done = true;
        }
        if menu_item(ui, "Delete", Some("Backspace".into()), &pal) {
            r.insert_all("");
            done = true;
        }
        if menu_item(ui, "Select all", Some("Ctrl+A".into()), &pal) {
            r.select_all();
            done = true;
        }
        menu_sep(ui, &pal);
        if menu_item(ui, "Insert incremental ID", None, &pal) {
            r.insert_incremental_id();
            done = true;
        }
        if menu_item(ui, "Insert current date", None, &pal) {
            r.insert_date(DateKind::Today);
            done = true;
        }
        if menu_item(ui, "Insert file creation date", None, &pal) {
            r.insert_date(DateKind::Created);
            done = true;
        }
        if menu_item(ui, "Insert file last-modified date", None, &pal) {
            r.insert_date(DateKind::Modified);
            done = true;
        }
        if menu_item(ui, "Insert parent folder name", None, &pal) {
            r.insert_parent_name();
            done = true;
        }
        if menu_item(ui, "Insert unique ID", None, &pal) {
            r.insert_unique_id();
            done = true;
        }
        if menu_item(ui, "Insert original name", None, &pal) {
            r.insert_original();
            done = true;
        }
        menu_sep(ui, &pal);
        if menu_item(ui, "Find & replace…", None, &pal) {
            find = true;
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Case:").small().color(pal.text_dim));
            for (l, c) in [("lower", Case::Lower), ("UPPER", Case::Upper), ("Title", Case::Title), ("Sentence", Case::Sentence)] {
                if ui.small_button(l).clicked() {
                    r.change_case(c);
                    done = true;
                }
            }
        });
    });
    if let Some(c) = clip {
        ctx.copy_text(c);
    }
    if find {
        app.actions.push(Action::Open(Popup::FindReplace { panel }));
        return false;
    }
    !(done || clicked_outside(ctx, rect))
}

fn find_replace(app: &mut App, ctx: &egui::Context, panel: u64) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let Some(p) = app.panels.get_mut(&panel) else { return false };
    let Some(r) = p.tab_mut().rename.as_mut() else { return false };
    let mut done = false;
    centered(ctx, "findreplace", 400.0, &pal, |ui| {
        ui.label(egui::RichText::new(format!("Find & replace in {} name{}", r.fields.len(), if r.fields.len() == 1 { "" } else { "s" })).strong());
        egui::Grid::new("fr").num_columns(2).show(ui, |ui| {
            ui.label("Find");
            let f = ui.add(egui::TextEdit::singleline(&mut r.find).desired_width(260.0));
            if r.find.is_empty() && !f.has_focus() {
                f.request_focus();
            }
            ui.end_row();
            ui.label("Replace");
            ui.add(egui::TextEdit::singleline(&mut r.replace).desired_width(260.0));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Replace all").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                r.find_replace();
                done = true;
            }
            if ui.button("Cancel").clicked() {
                done = true;
            }
        });
    });
    !done
}

fn about(app: &mut App, ctx: &egui::Context) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let (_, rect) = centered(ctx, "about", 380.0, &pal, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Maia").size(24.0).strong());
            ui.label(egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(pal.text_dim));
            ui.add_space(6.0);
            ui.label("A fast, keyboard-first file manager for Linux.");
            ui.label(egui::RichText::new("Single binary · Rust + egui").small().color(pal.text_dim));
            ui.add_space(6.0);
        });
    });
    !clicked_outside(ctx, rect)
}

fn tab_menu(app: &mut App, ctx: &egui::Context, panel: u64, idx: usize, pos: Pos2) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    // Commands act on the right-clicked tab.
    if let Some(p) = app.panels.get_mut(&panel) {
        if idx < p.tabs.len() {
            p.active = idx;
        }
    }
    app.focus_panel(panel);
    let mut run = None;
    let (_, rect) = anchored(ctx, "tabmenu", pos, 300.0, &pal, |ui| {
        for c in [Cmd::MoveTabLeft, Cmd::MoveTabRight] {
            if cmd_row(ui, app, c) {
                run = Some(c);
            }
        }
        menu_sep(ui, &pal);
        if cmd_row(ui, app, Cmd::OpenTabInWindow) {
            run = Some(Cmd::OpenTabInWindow);
        }
        menu_sep(ui, &pal);
        for c in [Cmd::CloseTab, Cmd::CloseOtherTabs, Cmd::CloseRightTabs, Cmd::CloseLeftTabs, Cmd::CloseAllTabs] {
            if cmd_row(ui, app, c) {
                run = Some(c);
            }
        }
    });
    if let Some(c) = run {
        app.actions.push(Action::Cmd(c));
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn tabs_options(app: &mut App, ctx: &egui::Context, panel: u64, pos: Pos2) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    app.focus_panel(panel);
    let mut run = None;
    let (_, rect) = anchored(ctx, "tabsopts", pos, 280.0, &pal, |ui| {
        for c in [Cmd::NewTab, Cmd::ReopenTab, Cmd::TabSearch, Cmd::CloseAllTabs] {
            if cmd_row(ui, app, c) {
                run = Some(c);
            }
        }
    });
    if let Some(c) = run {
        app.actions.push(Action::Cmd(c));
        return false;
    }
    !clicked_outside(ctx, rect)
}

fn group_menu(app: &mut App, ctx: &egui::Context, panel: u64, label: &str, pos: Pos2) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let Some(p) = app.panels.get_mut(&panel) else { return false };
    let t = p.tab_mut();
    let labels: Vec<String> = t.header_labels();
    let collapsed = t.collapsed_groups.contains(label);
    let any_collapsed = !t.collapsed_groups.is_empty();
    let all_collapsed = labels.iter().all(|l| t.collapsed_groups.contains(l));
    let mut done = false;
    let item = |ui: &mut Ui, text: &str, enabled: bool| -> bool {
        let r = row(ui, 26.0, false, &pal, |ui, r| {
            ui.painter().text(pos2(r.left() + 10.0, r.center().y), Align2::LEFT_CENTER, text, FontId::proportional(13.0), if enabled { pal.text } else { pal.text_dim.gamma_multiply(0.6) });
        });
        enabled && r.clicked()
    };
    let (_, rect) = anchored(ctx, "groupmenu", pos, 230.0, &pal, |ui| {
        if item(ui, if collapsed { "Expand Group" } else { "Collapse Group" }, true) {
            if collapsed {
                t.collapsed_groups.remove(label);
            } else {
                t.collapsed_groups.insert(label.to_string());
            }
            done = true;
        }
        if item(ui, "Expand All Groups", any_collapsed) {
            t.collapsed_groups.clear();
            done = true;
        }
        if item(ui, "Collapse All Groups", !all_collapsed) {
            t.collapsed_groups = labels.iter().cloned().collect();
            done = true;
        }
    });
    if done {
        t.dirty = true;
        return false;
    }
    !clicked_outside(ctx, rect)
}

/// Shown after dragging files with the right mouse button.
fn drop_menu(app: &mut App, ctx: &egui::Context, paths: &[PathBuf], dest: &Path, pos: Pos2) -> bool {
    let pal = app.pal;
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return false;
    }
    let mut pick: Option<u8> = None;
    let (_, rect) = anchored(ctx, "dropmenu", pos, 200.0, &pal, |ui| {
        ui.label(egui::RichText::new(format!("{} item{} → {}", paths.len(), if paths.len() == 1 { "" } else { "s" }, dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dest.display().to_string()))).small().color(pal.text_dim));
        for (k, label) in [(0u8, "Move here"), (1, "Copy here"), (2, "Link here")] {
            if menu_item(ui, label, None, &pal) {
                pick = Some(k);
            }
        }
        menu_sep(ui, &pal);
        if menu_item(ui, "Cancel", None, &pal) {
            pick = Some(9);
        }
    });
    match pick {
        Some(0) => app.begin_transfer(paths.to_vec(), dest.to_path_buf(), true),
        Some(1) => app.begin_transfer(paths.to_vec(), dest.to_path_buf(), false),
        Some(2) => app.actions.push(Action::Drop { paths: paths.to_vec(), dest: dest.to_path_buf(), copy: false, link: true }),
        Some(_) => {}
        None => return !clicked_outside(ctx, rect),
    }
    false
}

/// Header button that condenses a list to titles only (and back).
pub fn compact_button(ui: &mut Ui, on: bool, pal: &Palette) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
    if resp.hovered() || on {
        ui.painter().rect_filled(r, CornerRadius::same(5), if on { pal.accent.gamma_multiply(0.3) } else { pal.hover });
    }
    let c = r.center();
    let col = if on { pal.accent } else { pal.text_dim };
    let st = Stroke::new(1.2, col);
    ui.painter().rect_stroke(Rect::from_center_size(c, vec2(12.0, 13.0)), CornerRadius::same(2), st, StrokeKind::Middle);
    let rows: &[f32] = if on { &[-3.0, 0.0, 3.0] } else { &[-3.0, 1.5] };
    for dy in rows {
        ui.painter().line_segment([c + vec2(-3.5, *dy), c + vec2(3.5, *dy)], st);
    }
    resp.on_hover_text(if on { "Show descriptions" } else { "Titles only" })
}

/// Enter on an option row: flip switches; other controls are left to the mouse.
fn opt_flip(c: &mut crate::config::Config, o: Opt) -> bool {
    let b: &mut bool = match o {
        Opt::RectButtons => &mut c.rect_buttons,
        Opt::FollowSystem => &mut c.follow_system_theme,
        Opt::Round => &mut c.round_corners,
        Opt::Animations => &mut c.animations,
        Opt::Tooltips => &mut c.tooltips,
        Opt::AltRows => &mut c.alt_rows,
        Opt::Breadcrumbs => &mut c.breadcrumbs,
        Opt::CompactPaths => &mut c.compact_paths,
        Opt::ShowExt => &mut c.show_ext,
        Opt::ShowHidden => &mut c.show_hidden,
        Opt::ShowSystem => &mut c.show_system,
        Opt::FoldersFirst => &mut c.folders_first,
        Opt::HighlightRecents => &mut c.highlight_recents,
        Opt::RecentsOnTop => &mut c.recents_on_top,
        Opt::CalcSizes => &mut c.folder_sizes,
        Opt::SizesSsd => &mut c.sizes_ssd,
        Opt::SizesHdd => &mut c.sizes_hdd,
        Opt::SizesNet => &mut c.sizes_network,
        Opt::SelectionBoxes => &mut c.selection_boxes,
        Opt::SystemIcons => &mut c.system_icons,
        Opt::ThumbFill => &mut c.thumb_fill,
        Opt::ScrollBeyond => &mut c.scroll_beyond,
        Opt::ActOnPress => &mut c.act_on_press,
        Opt::SingleClick => &mut c.single_click,
        Opt::SingleClickFolders => &mut c.single_click_folders_only,
        Opt::OpenFoldersNewTab => &mut c.open_folders_new_tab,
        Opt::DblClickParent => &mut c.dblclick_bg_parent,
        Opt::ReverseScroll => &mut c.reverse_scroll,
        Opt::SingleInstance => &mut c.single_instance,
        _ => return false,
    };
    *b = !*b;
    true
}

/// Icon for a palette category.
fn paint_cat_glyph(p: &egui::Painter, r: Rect, cat: Cat, col: Color32) {
    match cat {
        Cat::Quick => crate::ui::star(p, r.center(), r.width() * 0.48, col, false),
        Cat::Folder => paint_glyph(p, r, Glyph::Folder, col),
        Cat::Files => paint_glyph(p, r, Glyph::Docs, col),
        Cat::Selection => checkbox_glyph(p, r.shrink(1.5), true, &crate::theme::Palette { accent: Color32::TRANSPARENT, text: col, text_dim: col, ..crate::theme::palette(ThemeName::Midnight) }),
        Cat::Tabs => {
            let st = Stroke::new(1.2, col);
            let b = r.shrink(1.5);
            p.rect_stroke(Rect::from_min_max(pos2(b.left(), b.top() + 3.0), b.max), CornerRadius::same(2), st, StrokeKind::Middle);
            p.line_segment([pos2(b.left() + 1.0, b.top() + 3.0), pos2(b.left() + 1.0, b.top()), ], st);
            p.line_segment([pos2(b.left() + 1.0, b.top()), pos2(b.center().x, b.top())], st);
            p.line_segment([pos2(b.center().x, b.top()), pos2(b.center().x, b.top() + 3.0)], st);
        }
        Cat::Panels => paint_glyph(p, r, Glyph::Split, col),
        Cat::View => paint_glyph(p, r, Glyph::Pics, col),
        Cat::Sort => {
            let st = Stroke::new(1.2, col);
            let (l, t, b) = (r.left() + 2.0, r.top() + 2.0, r.bottom() - 2.0);
            p.line_segment([pos2(l + 2.5, t), pos2(l + 2.5, b)], st);
            p.add(egui::Shape::line(vec![pos2(l, b - 2.5), pos2(l + 2.5, b), pos2(l + 5.0, b - 2.5)], st));
            for (k, w) in [(0.0, 6.0), (3.5, 4.5), (7.0, 3.0)] {
                p.line_segment([pos2(l + 7.0, t + 1.0 + k), pos2(l + 7.0 + w, t + 1.0 + k)], st);
            }
        }
        Cat::Group => {
            let st = Stroke::new(1.2, col);
            let b = r.shrink(1.5);
            for (y0, y1) in [(b.top(), b.center().y - 1.0), (b.center().y + 1.0, b.bottom())] {
                p.rect_stroke(Rect::from_min_max(pos2(b.left(), y0), pos2(b.right(), y1)), CornerRadius::same(1), st, StrokeKind::Middle);
            }
        }
        Cat::Inspector => paint_glyph(p, r, Glyph::Eye, col),
        Cat::Layouts => paint_glyph(p, r, Glyph::Monitor, col),
        Cat::Misc => paint_glyph(p, r, Glyph::Menu, col),
    }
}
