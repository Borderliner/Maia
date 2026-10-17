//! Show Space Used: a folder's contents biggest first, each with a bar for its share.
//! The folder is read on a thread; folder sizes come from the folder-size workers
//! (stats.rs) and land as they are counted. The list is sorted again only when sizes
//! arrive, at most a few times a second, never on every frame.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui::{CornerRadius, FontId, Key, Modifiers, Rect, Sense, Vec2, pos2, vec2};

use crate::app::{Action, App};
use crate::fsx::Entry;
use crate::popups::{self as pp, Popup};

pub struct Item {
    pub entry: Entry,
    /// Files have theirs at once; a folder's is None until it is counted.
    pub size: Option<u64>,
}

pub struct State {
    /// Where it was opened: Backspace goes no higher.
    pub root: PathBuf,
    pub dir: PathBuf,
    listing: Option<mpsc::Receiver<Vec<Entry>>>,
    pub items: Vec<Item>,
    /// Items in the order shown.
    pub order: Vec<u32>,
    /// Folders whose size hasn't come yet.
    waiting: Vec<u32>,
    /// A size came since the last sort.
    unsorted: bool,
    sorted_at: Instant,
    /// The row with the cursor (a position in `order`).
    pub sel: usize,
    pub checked: HashSet<PathBuf>,
    /// The folder to put the cursor on once a listing lands (the one just left).
    select: Option<PathBuf>,
    /// Said under the list in place of the totals, until the next key.
    pub note: Option<String>,
    /// Asking whether to move the checked items to the trash.
    pub confirm: bool,
    reveal: bool,
    /// The cursor was put somewhere: it follows its item as sizes reorder the list;
    /// until then it stays on the first row, the biggest.
    moved: bool,
}

/// Show Space Used for the folder the command targets.
pub fn open(app: &mut App) {
    match crate::devtools::target_dir(app) {
        Some(d) => {
            let ctx = app.ctx.clone();
            app.popup = Popup::SpaceUsed(Box::new(State::new(d, &ctx)));
        }
        None => app.toast("Show Space Used needs a folder"),
    }
}

impl State {
    pub fn new(root: PathBuf, ctx: &egui::Context) -> State {
        let mut st = State { root: root.clone(), dir: root.clone(), listing: None, items: vec![], order: vec![], waiting: vec![], unsorted: false, sorted_at: Instant::now(), sel: 0, checked: HashSet::new(), select: None, note: None, confirm: false, reveal: false, moved: false };
        st.go(root, None, ctx);
        st
    }

    /// Show `dir`, the cursor going to `select` once it is read.
    fn go(&mut self, dir: PathBuf, select: Option<PathBuf>, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let (d, c) = (dir.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("space used".into()).spawn(move || {
            let mut v = vec![];
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    if let Some(en) = Entry::from_path(&e.path()) {
                        v.push(en);
                    }
                }
            }
            let _ = tx.send(v);
            c.request_repaint();
        });
        self.dir = dir;
        self.listing = Some(rx);
        self.items.clear();
        self.order.clear();
        self.waiting.clear();
        self.checked.clear();
        self.sel = 0;
        self.select = select;
        self.moved = self.select.is_some();
        self.confirm = false;
    }

    /// Everything counted so far.
    pub fn total(&self) -> u64 {
        self.items.iter().filter_map(|i| i.size).sum()
    }

    pub fn counting(&self) -> bool {
        self.listing.is_some() || !self.waiting.is_empty()
    }

    fn current(&self) -> Option<&Item> {
        self.items.get(*self.order.get(self.sel)? as usize)
    }

    /// Biggest first; folders not counted yet after them, by name.
    fn sort(&mut self) {
        let items = &self.items;
        let at = self.current().filter(|_| self.moved).map(|i| i.entry.path.clone());
        let mut order: Vec<u32> = (0..items.len() as u32).collect();
        crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| {
            let (x, y) = (&items[*a as usize], &items[*b as usize]);
            match (x.size, y.size) {
                (Some(p), Some(q)) => q.cmp(&p).then_with(|| crate::tab::natural_cmp(&x.entry.name.to_lowercase(), &y.entry.name.to_lowercase())),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => crate::tab::natural_cmp(&x.entry.name.to_lowercase(), &y.entry.name.to_lowercase()),
            }
        });
        self.order = order;
        // The cursor stays on its item while others move past it.
        if let Some(p) = at {
            if let Some(i) = self.order.iter().position(|&k| self.items[k as usize].entry.path == p) {
                self.sel = i;
            }
        }
        self.unsorted = false;
        self.sorted_at = Instant::now();
    }

    /// Take in a listing that landed and sizes that were counted.
    pub fn pump(&mut self, stats: &mut crate::stats::Stats, ctx: &egui::Context) {
        if let Some(rx) = &self.listing {
            if let Ok(v) = rx.try_recv() {
                self.listing = None;
                self.items = v.into_iter().map(|e| Item { size: (!e.is_dir || e.is_link).then_some(e.size), entry: e }).collect();
                self.waiting = (0..self.items.len() as u32).filter(|&i| self.items[i as usize].size.is_none()).collect();
                for &i in &self.waiting {
                    stats.request(&self.items[i as usize].entry.path);
                }
                self.sort();
                if let Some(p) = self.select.take() {
                    self.sel = self.order.iter().position(|&k| self.items[k as usize].entry.path == p).unwrap_or(0);
                    self.reveal = true;
                }
            }
        }
        if !self.waiting.is_empty() {
            let items = &mut self.items;
            let before = self.waiting.len();
            self.waiting.retain(|&i| {
                let it = &mut items[i as usize];
                match stats.map.get(&it.entry.path) {
                    Some(s) => {
                        it.size = Some(s.size);
                        false
                    }
                    None => {
                        // Forgotten since it was asked for (its contents changed): again.
                        stats.request(&it.entry.path);
                        true
                    }
                }
            });
            self.unsorted |= self.waiting.len() != before;
        }
        if self.unsorted {
            let wait = Duration::from_millis(250).saturating_sub(self.sorted_at.elapsed());
            if wait.is_zero() || self.waiting.is_empty() {
                self.sort();
            } else {
                ctx.request_repaint_after(wait);
            }
        }
    }

    fn flip(&mut self, row: usize) {
        if let Some(p) = self.order.get(row).map(|&k| self.items[k as usize].entry.path.clone()) {
            if !self.checked.remove(&p) {
                self.checked.insert(p);
            }
        }
    }

    fn checked_size(&self) -> u64 {
        self.items.iter().filter(|i| self.checked.contains(&i.entry.path)).filter_map(|i| i.size).sum()
    }
}

/// The dialog. Returns false when it closes.
pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    st.pump(&mut app.stats, ctx);
    let k = pp::take_keys(ctx);
    let (space, delete) = (crate::devtools::take(ctx, Modifiers::NONE, Key::Space), crate::devtools::take(ctx, Modifiers::NONE, Key::Delete));
    let backspace = k.backspace && !st.confirm;
    if k.esc {
        if st.confirm {
            st.confirm = false;
        } else {
            return false;
        }
    }
    if k.up || k.down || k.page_up || k.page_down || space || backspace || k.enter {
        st.note = None;
    }
    let n = st.order.len();
    if !st.confirm {
        let before = st.sel;
        pp::step(&mut st.sel, n, &k, |_| false);
        if crate::devtools::take(ctx, Modifiers::NONE, Key::Home) {
            st.sel = 0;
        }
        if crate::devtools::take(ctx, Modifiers::NONE, Key::End) {
            st.sel = n.saturating_sub(1);
        }
        st.reveal |= st.sel != before;
        st.moved |= st.sel != before;
    }
    if space && !st.confirm {
        let at = st.sel;
        st.flip(at);
    }
    let fs = app.cfg.font_size;
    let small = fs - 1.5;
    let row_h = (fs * 1.35 + 12.0).round();
    let list_max = pp::fit_list(ctx, 460.0, 220.0);
    let total = st.total();
    let (mut enter, mut show, mut trash, mut cancel, mut close) = (None, false, (delete || k.enter && st.confirm) && !st.checked.is_empty(), false, false);
    if k.enter && !st.confirm {
        enter = Some(st.sel);
    }
    let mut toggle = None;
    let counting = st.counting();
    let now = ctx.input(|i| i.time);
    pp::centered(ctx, "space used", 720.0, &pal, |ui| {
        let what = if counting { format!("{} · {} so far", pp::home_short(&st.dir), crate::fsx::fmt_size(total)) } else { format!("{} · {}", pp::home_short(&st.dir), crate::fsx::fmt_size(total)) };
        pp::dialog_title(ui, "Space used", Some(&what), &pal);
        ui.add_space(8.0);
        if n > 0 {
            let mut area = egui::ScrollArea::vertical().id_salt("space list").max_height(list_max).min_scrolled_height((n as f32 * row_h).min(list_max)).auto_shrink([false, true]);
            if std::mem::take(&mut st.reveal) {
                let top = st.sel as f32 * row_h;
                let most = (n as f32 * row_h - list_max).max(0.0);
                area = area.vertical_scroll_offset((top - list_max / 2.0 + row_h).clamp(0.0, most));
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, row_h, n, |ui, range| {
                for row in range {
                    let it = &st.items[st.order[row] as usize];
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                    crate::ui::note_control(ui, r, "space row");
                    let on = row == st.sel;
                    if on {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                    }
                    let (tc, dc) = if on { (pal.on_sel(), pal.on_sel_dim()) } else { (pal.text, pal.text_dim) };
                    let cb = Rect::from_center_size(pos2(r.left() + 16.0, r.center().y), Vec2::splat(14.0));
                    pp::checkbox_glyph(ui.painter(), cb, st.checked.contains(&it.entry.path), &pal);
                    if resp.clicked() {
                        let at_box = resp.interact_pointer_pos().is_some_and(|p| p.x < cb.right() + 6.0);
                        if at_box {
                            toggle = Some(row);
                        } else if it.entry.is_dir && !it.entry.is_link {
                            enter = Some(row);
                        } else {
                            st.sel = row;
                            st.moved = true;
                        }
                    }
                    let ic = Rect::from_center_size(pos2(cb.right() + 18.0, r.center().y), Vec2::splat(16.0));
                    if !crate::icons::entry(ui.painter(), ic, &it.entry) {
                        crate::theme::paint_icon(ui.painter(), ic, crate::theme::categorize(it.entry.ext(), it.entry.is_dir, it.entry.is_executable()), it.entry.ext(), it.entry.is_link);
                    }
                    // Size at the edge, the bar before it, the name in what is left.
                    let size_w = 72.0;
                    let bar_w = (r.width() * 0.22).clamp(40.0, 160.0);
                    let text = match it.size {
                        Some(s) => crate::fsx::fmt_size(s),
                        None => "…".into(),
                    };
                    let sg = ui.painter().layout_no_wrap(text, FontId::proportional(small), dc);
                    ui.painter().galley(pos2(r.right() - 12.0 - sg.size().x, r.center().y - sg.size().y / 2.0), sg, dc);
                    let bar = Rect::from_min_size(pos2(r.right() - 12.0 - size_w - bar_w, r.center().y - 2.5), vec2(bar_w, 5.0));
                    ui.painter().rect_filled(bar, CornerRadius::same(3), if on { pal.on_sel_dim().gamma_multiply(0.35) } else { pal.raised });
                    match it.size {
                        Some(s) if total > 0 && s > 0 => {
                            let w = (bar_w * s as f32 / total as f32).max(2.0);
                            ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(w, bar.height())), CornerRadius::same(3), if on { pal.on_sel() } else { pal.accent });
                        }
                        None => crate::ui::spinner(ui.painter(), pos2(bar.left() - 12.0, r.center().y), 4.5, dc, now),
                        _ => {}
                    }
                    let x = ic.right() + 10.0;
                    let room = bar.left() - 24.0 - x;
                    let g = crate::listing::elided(ui, &it.entry.name, FontId::proportional(fs), room, tc);
                    ui.painter().galley(pos2(x, r.center().y - g.size().y / 2.0), g, tc);
                }
            });
            if counting {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        ui.add_space(6.0);
        let chosen = st.checked.len();
        let (status, col) = if st.confirm {
            let what = if chosen == 1 { "1 item".to_string() } else { format!("{chosen} items") };
            (format!("Move {what} ({}) to the trash?", crate::fsx::fmt_size(st.checked_size())), pal.danger)
        } else if let Some(t) = &st.note {
            (t.clone(), pal.text_dim)
        } else if st.listing.is_some() {
            ("Reading the folder…".to_string(), pal.text_dim)
        } else if n == 0 {
            ("This folder is empty".to_string(), pal.text_dim)
        } else if counting {
            (format!("Counting {} folders…", crate::table::fmt_count(st.waiting.len())), pal.text_dim)
        } else {
            let items = if n == 1 { "1 item".to_string() } else { format!("{} items", crate::table::fmt_count(n)) };
            (if chosen > 0 { format!("{items}; {chosen} checked, {}", crate::fsx::fmt_size(st.checked_size())) } else { items }, pal.text_dim)
        };
        ui.label(egui::RichText::new(status).size(small).color(col));
        pp::hints(ui, &[("Enter", "open folder"), ("Backspace", "up"), ("Space", "check"), ("Delete", "trash"), ("Esc", "close")], &pal);
        pp::dialog_buttons(ui, |ui| {
            if st.confirm {
                let b = pp::primary_button(ui, "Move to Trash", true, &pal);
                crate::ui::note_control(ui, b.rect, "space trash confirm");
                trash |= b.clicked();
                let c = pp::plain_button(ui, "Cancel", &pal);
                crate::ui::note_control(ui, c.rect, "space trash cancel");
                cancel = c.clicked();
                return;
            }
            let label = if chosen == 0 { "Move to Trash".to_string() } else { format!("Move {} to Trash", crate::fsx::fmt_size(st.checked_size())) };
            let b = pp::primary_button(ui, &label, chosen > 0, &pal);
            crate::ui::note_control(ui, b.rect, "space trash");
            trash |= b.clicked();
            let c = pp::plain_button(ui, "Close", &pal);
            crate::ui::note_control(ui, c.rect, "space close");
            close = c.clicked();
            if n > 0 {
                let s = pp::plain_button(ui, "Show in Maia", &pal);
                crate::ui::note_control(ui, s.rect, "space show");
                show = s.clicked();
            }
        });
    });
    if close {
        return false;
    }
    if cancel {
        st.confirm = false;
    }
    if let Some(row) = toggle {
        st.sel = row;
        st.moved = true;
        st.flip(row);
    }
    if let Some(row) = enter {
        let dir = st.order.get(row).map(|&k| &st.items[k as usize]).filter(|i| i.entry.is_dir && !i.entry.is_link).map(|i| i.entry.path.clone());
        if let Some(d) = dir {
            st.go(d, None, ctx);
            return true;
        }
    }
    if backspace {
        if st.dir == st.root || !st.dir.starts_with(&st.root) {
            st.note = Some(format!("Space used was opened in {}: it goes no higher", st.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| st.root.display().to_string())));
        } else if let Some(up) = st.dir.parent().map(Path::to_path_buf) {
            let from = st.dir.clone();
            st.go(up, Some(from), ctx);
        }
        return true;
    }
    if show {
        if let Some(p) = st.current().map(|i| i.entry.path.clone()) {
            let panel = app.active;
            app.actions.push(Action::Navigate { panel, path: p });
            return false;
        }
    }
    if trash && !st.checked.is_empty() {
        if !st.confirm {
            st.confirm = true;
            return true;
        }
        let paths: Vec<PathBuf> = st.items.iter().filter(|i| st.checked.contains(&i.entry.path)).map(|i| i.entry.path.clone()).collect();
        app.actions.push(Action::StartJob(crate::ops::OpKind::Trash { paths }));
        // Gone from the list now; the job takes them away.
        let checked = std::mem::take(&mut st.checked);
        let at = st.current().map(|i| i.entry.path.clone());
        st.items.retain(|i| !checked.contains(&i.entry.path));
        st.waiting = (0..st.items.len() as u32).filter(|&i| st.items[i as usize].size.is_none()).collect();
        st.sort();
        st.sel = at.and_then(|p| st.order.iter().position(|&k| st.items[k as usize].entry.path == p)).unwrap_or(st.sel).min(st.order.len().saturating_sub(1));
        st.confirm = false;
    }
    true
}
