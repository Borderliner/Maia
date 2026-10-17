//! Find Duplicates: files under a folder with the same contents, whatever their names.
//! Sizes rule out most files without reading them; files of one size are compared by
//! their first 64 KB, then whole. All of it on a thread; the sets stream into the
//! dialog as they are found. In each set the oldest copy is the one kept.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use egui::{CornerRadius, FontId, Key, Modifiers, Rect, Sense, Vec2, pos2, vec2};

use crate::app::{Action, App};
use crate::popups::{self as pp, Popup};

/// What is read of every file of a shared size before reading whole ones.
const HEAD: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Copy {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

/// Files with the same contents; the first is the oldest, the one to keep.
#[derive(Clone, Debug, PartialEq)]
pub struct Set {
    pub size: u64,
    pub files: Vec<Copy>,
}

impl Set {
    /// What trashing every copy but one gives back.
    pub fn spare(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }
}

pub enum Msg {
    /// Files looked at so far.
    Counted(usize),
    /// Files of a shared size compared so far, of how many.
    Compared(usize, usize),
    Set(Set),
    Done,
}

/// A search in progress; dropping it stops it.
pub struct Scan {
    pub rx: mpsc::Receiver<Msg>,
    stop: Arc<AtomicBool>,
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

impl Scan {
    pub fn start(root: PathBuf, ctx: &egui::Context) -> Scan {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, ctx) = (stop.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("dupes".into()).spawn(move || {
            let mut shown = std::time::Instant::now();
            let send = |m: Msg, force: bool, shown: &mut std::time::Instant| {
                if force || shown.elapsed().as_millis() >= 100 {
                    *shown = std::time::Instant::now();
                    let _ = tx.send(m);
                    ctx.request_repaint();
                }
            };
            find(&root, &st, &mut |m| {
                let force = matches!(m, Msg::Set(_));
                send(m, force, &mut shown);
            });
            let _ = tx.send(Msg::Done);
            ctx.request_repaint();
        });
        Scan { rx, stop }
    }
}

struct File {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

/// Every set of duplicates under `root`, the biggest files first, with progress on the
/// way. Not looked in: hidden files and folders, build folders and installed packages,
/// links, other file systems. Empty files are all alike and are left out; hard links to
/// one file are one file.
pub fn find(root: &Path, stop: &AtomicBool, out: &mut dyn FnMut(Msg)) {
    let files = gather(root, stop, out);
    if stop.load(Relaxed) {
        return;
    }
    let mut by_size: HashMap<u64, Vec<u32>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        by_size.entry(f.size).or_default().push(i as u32);
    }
    let mut sizes: Vec<u64> = by_size.iter().filter(|(_, v)| v.len() > 1).map(|(s, _)| *s).collect();
    crate::tab::merge_sort(&mut sizes, &|a: &u64, b: &u64| b.cmp(a));
    let total: usize = sizes.iter().map(|s| by_size[s].len()).sum();
    let mut done = 0;
    // Two independent keys: what a whole file hashes to is 128 bits, so two files that
    // differ never pass for the same.
    let keys = (std::collections::hash_map::RandomState::new(), std::collections::hash_map::RandomState::new());
    let mut buf = vec![0u8; 1 << 20];
    for size in sizes {
        let ids = &by_size[&size];
        let mut by_head: HashMap<(u64, u64), Vec<u32>> = HashMap::new();
        for &i in ids {
            if stop.load(Relaxed) {
                return;
            }
            if let Some(h) = hash(&files[i as usize].path, Some(HEAD), &keys, &mut buf, stop) {
                by_head.entry(h).or_default().push(i);
            }
            done += 1;
            out(Msg::Compared(done, total));
        }
        for (_, same) in by_head.into_iter().filter(|(_, v)| v.len() > 1) {
            let groups: Vec<Vec<u32>> = if size as usize <= HEAD {
                vec![same]
            } else {
                let mut whole: HashMap<(u64, u64), Vec<u32>> = HashMap::new();
                for i in same {
                    if let Some(h) = hash(&files[i as usize].path, None, &keys, &mut buf, stop) {
                        whole.entry(h).or_default().push(i);
                    }
                }
                whole.into_values().filter(|v| v.len() > 1).collect()
            };
            for g in groups {
                let mut copies: Vec<Copy> = g.iter().map(|&i| Copy { path: files[i as usize].path.clone(), modified: files[i as usize].modified }).collect();
                // The oldest first: most likely the original. Ties go to the shorter path.
                let mut order: Vec<u32> = (0..copies.len() as u32).collect();
                crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| {
                    let (x, y) = (&copies[*a as usize], &copies[*b as usize]);
                    x.modified.cmp(&y.modified).then_with(|| x.path.as_os_str().len().cmp(&y.path.as_os_str().len())).then_with(|| x.path.cmp(&y.path))
                });
                let sorted = order.iter().map(|&k| copies[k as usize].clone()).collect();
                copies = sorted;
                out(Msg::Set(Set { size, files: copies }));
            }
        }
    }
}

fn gather(root: &Path, stop: &AtomicBool, out: &mut dyn FnMut(Msg)) -> Vec<File> {
    use std::os::unix::fs::MetadataExt;
    let mut files = vec![];
    let Ok(dev) = std::fs::metadata(root).map(|m| m.dev()) else { return files };
    if crate::fsx::is_virtual_fs(root) {
        return files;
    }
    let mut inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if stop.load(Relaxed) {
            return files;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let kids: Vec<(String, std::fs::FileType, PathBuf)> = rd.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.file_type().ok()?, e.path()))).collect();
        let has = |n: &str| kids.iter().any(|k| k.0 == n);
        for (name, ft, path) in &kids {
            if name.starts_with('.') || ft.is_symlink() {
                continue;
            }
            let Ok(m) = std::fs::symlink_metadata(path) else { continue };
            if m.dev() != dev {
                continue;
            }
            if ft.is_dir() {
                if crate::buildclean::kind_by_name(name, &has).is_none() {
                    stack.push(path.clone());
                }
            } else if ft.is_file() && m.len() > 0 && inodes.insert((m.dev(), m.ino())) {
                files.push(File { path: path.clone(), size: m.len(), modified: m.modified().ok() });
                if files.len() % 512 == 0 {
                    out(Msg::Counted(files.len()));
                }
            }
        }
    }
    out(Msg::Counted(files.len()));
    files
}

/// The first `limit` bytes of a file (all of it with none) under two keys.
fn hash(p: &Path, limit: Option<usize>, keys: &(std::collections::hash_map::RandomState, std::collections::hash_map::RandomState), buf: &mut [u8], stop: &AtomicBool) -> Option<(u64, u64)> {
    let mut f = std::fs::File::open(p).ok()?;
    let (mut a, mut b) = (keys.0.build_hasher(), keys.1.build_hasher());
    let mut left = limit.unwrap_or(usize::MAX);
    let cap = buf.len();
    while left > 0 {
        if stop.load(Relaxed) {
            return None;
        }
        let n = f.read(&mut buf[..cap.min(left)]).ok()?;
        if n == 0 {
            break;
        }
        a.write(&buf[..n]);
        b.write(&buf[..n]);
        left -= n;
    }
    Some((a.finish(), b.finish()))
}

// ------------------------------------------------------------------ the dialog

pub struct State {
    pub root: PathBuf,
    scan: Option<Scan>,
    pub sets: Vec<Set>,
    pub checked: HashSet<PathBuf>,
    /// Rows: a set's heading (file u32::MAX) or one of its files.
    rows: Vec<(u32, u32)>,
    /// The selected row, always a file's.
    pub sel: usize,
    pub done: bool,
    counted: usize,
    compared: (usize, usize),
    /// Said under the list when Move to Trash can't go ahead.
    refusal: Option<String>,
}

pub fn open(ctx: &egui::Context, root: PathBuf) -> Popup {
    let scan = Scan::start(root.clone(), ctx);
    Popup::Duplicates(State { root, scan: Some(scan), sets: vec![], checked: HashSet::new(), rows: vec![], sel: 0, done: false, counted: 0, compared: (0, 0), refusal: None })
}

impl State {
    fn rebuild(&mut self) {
        let at = self.rows.get(self.sel).copied();
        self.rows.clear();
        for (g, s) in self.sets.iter().enumerate() {
            self.rows.push((g as u32, u32::MAX));
            self.rows.extend((0..s.files.len() as u32).map(|f| (g as u32, f)));
        }
        self.sel = at.and_then(|a| self.rows.iter().position(|r| *r == a)).unwrap_or(1.min(self.rows.len().saturating_sub(1)));
    }

    fn file(&self, row: usize) -> Option<&Copy> {
        let (g, f) = *self.rows.get(row)?;
        self.sets.get(g as usize)?.files.get(f as usize)
    }

    /// Every copy but the one kept in each set: checked, or all unchecked again.
    pub fn toggle_extras(&mut self) {
        let extras: Vec<PathBuf> = self.sets.iter().flat_map(|s| s.files.iter().skip(1).map(|c| c.path.clone())).collect();
        if !extras.is_empty() && extras.iter().all(|p| self.checked.contains(p)) {
            self.checked.clear();
        } else {
            self.checked = extras.into_iter().collect();
        }
    }

    /// Sets with every copy checked: trashing them would lose the file.
    pub fn whole_sets(&self) -> usize {
        self.sets.iter().filter(|s| s.files.iter().all(|c| self.checked.contains(&c.path))).count()
    }

    fn checked_size(&self) -> u64 {
        self.sets.iter().map(|s| s.size * s.files.iter().filter(|c| self.checked.contains(&c.path)).count() as u64).sum()
    }
}

/// Find Duplicates: the sets as they are found, the biggest files first, each with its
/// copies; the checked ones go to the trash once confirmed, never every copy of one.
pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    let k = pp::take_keys(ctx);
    if k.esc {
        return false;
    }
    let mut grew = false;
    if let Some(s) = &st.scan {
        while let Ok(m) = s.rx.try_recv() {
            match m {
                Msg::Counted(n) => st.counted = n,
                Msg::Compared(a, b) => st.compared = (a, b),
                Msg::Set(set) => {
                    st.sets.push(set);
                    grew = true;
                }
                Msg::Done => st.done = true,
            }
        }
    }
    if st.done && st.scan.is_some() {
        st.scan = None;
        // The most space back first, now that every set is in.
        let mut order: Vec<u32> = (0..st.sets.len() as u32).collect();
        let sets = &st.sets;
        crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| sets[*b as usize].spare().cmp(&sets[*a as usize].spare()));
        st.sets = order.iter().map(|&i| st.sets[i as usize].clone()).collect();
        grew = true;
    }
    if grew {
        st.rebuild();
    }
    let rows = &st.rows;
    pp::step(&mut st.sel, rows.len(), &k, |i| rows.get(i).is_some_and(|r| r.1 == u32::MAX));
    let flip = |st: &mut State, row: usize| {
        if let Some(p) = st.file(row).map(|c| c.path.clone()) {
            if !st.checked.remove(&p) {
                st.checked.insert(p);
            }
            st.refusal = None;
        }
    };
    if crate::devtools::take(ctx, Modifiers::NONE, Key::Space) {
        let at = st.sel;
        flip(st, at);
    }
    if crate::devtools::take(ctx, Modifiers::COMMAND, Key::A) {
        st.toggle_extras();
        st.refusal = None;
    }
    let fs = app.cfg.font_size;
    let small = fs - 1.5;
    let mut trash = k.enter && !st.checked.is_empty();
    let (mut cancel, mut extras) = (false, false);
    let mut toggle: Option<usize> = None;
    let list_max = pp::fit_list(ctx, 460.0, 200.0);
    let chosen = st.checked.len();
    let chosen_size = st.checked_size();
    let spare: u64 = st.sets.iter().map(Set::spare).sum();
    pp::centered(ctx, "duplicates", 720.0, &pal, |ui| {
        let sub = format!("In {}. The oldest copy in each set is the one to keep; checked copies go to the trash.", pp::home_short(&st.root));
        pp::dialog_title(ui, "Find duplicates", Some(&sub), &pal);
        ui.add_space(8.0);
        let row_h = (fs * 1.35 + 12.0).round();
        if !st.rows.is_empty() {
            let n = st.rows.len();
            let mut area = egui::ScrollArea::vertical().id_salt("dupes list").max_height(list_max).min_scrolled_height((n as f32 * row_h).min(list_max)).auto_shrink([false, true]);
            if k.up || k.down || k.page_up || k.page_down {
                // Keep the selected row in view, with its set's heading when it is the first.
                // egui takes a negative offset as it is, so it is kept within the list.
                let top = (st.sel as f32 - 1.0).max(0.0) * row_h;
                let most = (n as f32 * row_h - list_max).max(0.0);
                area = area.vertical_scroll_offset((top - list_max / 2.0 + row_h).clamp(0.0, most));
            }
            area.show_rows(ui, row_h, n, |ui, range| {
                for i in range {
                    let (g, f) = st.rows[i];
                    let set = &st.sets[g as usize];
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), if f == u32::MAX { Sense::hover() } else { Sense::click() });
                    if f == u32::MAX {
                        let name = set.files[0].path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let what = format!("{name} · {} copies · {} each", set.files.len(), crate::fsx::fmt_size(set.size));
                        let g = crate::listing::elided(ui, &what, FontId::proportional(small), r.width() - 16.0, pal.text_dim);
                        ui.painter().galley(pos2(r.left() + 8.0, r.bottom() - 4.0 - g.size().y), g, pal.text_dim);
                        continue;
                    }
                    let on = i == st.sel;
                    if on {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                    }
                    if resp.clicked() {
                        toggle = Some(i);
                    }
                    let c = &set.files[f as usize];
                    let (tc, dc) = if on { (pal.on_sel(), pal.on_sel_dim()) } else { (pal.text, pal.text_dim) };
                    let cb = Rect::from_center_size(pos2(r.left() + 16.0, r.center().y), Vec2::splat(14.0));
                    crate::popups::checkbox_glyph(ui.painter(), cb, st.checked.contains(&c.path), &pal);
                    crate::ui::note_control(ui, cb, "duplicate check");
                    let when = c.modified.map(|m| crate::timefmt::format(m, &app.cfg.date_format)).unwrap_or_default();
                    let wg = ui.painter().layout_no_wrap(when, FontId::proportional(small), dc);
                    let ww = wg.size().x;
                    ui.painter().galley(pos2(r.right() - 12.0 - ww, r.center().y - wg.size().y / 2.0), wg, dc);
                    let x = cb.right() + 12.0;
                    let rel = crate::project::relative(&c.path, &st.root);
                    let g = crate::listing::elided_tail(ui, &rel, FontId::proportional(fs), r.right() - 24.0 - ww - x, tc);
                    ui.painter().galley(pos2(x, r.center().y - g.size().y / 2.0), g, tc);
                }
            });
        }
        ui.add_space(6.0);
        let status = if let Some(why) = &st.refusal {
            why.clone()
        } else if !st.done && st.compared.1 == 0 {
            format!("Looking… {} files", crate::table::fmt_count(st.counted))
        } else if !st.done {
            format!("Comparing… {} of {} files", crate::table::fmt_count(st.compared.0), crate::table::fmt_count(st.compared.1))
        } else if st.sets.is_empty() {
            "No duplicates here".to_string()
        } else {
            format!("{} {} of duplicates; {} could be freed", st.sets.len(), if st.sets.len() == 1 { "set" } else { "sets" }, crate::fsx::fmt_size(spare))
        };
        let col = if st.refusal.is_some() { pal.danger } else { pal.text_dim };
        ui.label(egui::RichText::new(status).size(small).color(col));
        pp::hints(ui, &[("Space", "check"), ("Ctrl+A", "check the extras"), ("Enter", "move to trash"), ("Esc", "close")], &pal);
        pp::dialog_buttons(ui, |ui| {
            let label = if chosen == 0 { "Move to Trash".to_string() } else { format!("Move {} to Trash", crate::fsx::fmt_size(chosen_size)) };
            let b = pp::primary_button(ui, &label, chosen > 0, &pal);
            crate::ui::note_control(ui, b.rect, "duplicates trash");
            trash |= b.clicked();
            let c = pp::plain_button(ui, "Cancel", &pal);
            crate::ui::note_control(ui, c.rect, "duplicates cancel");
            cancel = c.clicked();
            if !st.sets.is_empty() {
                let e = pp::plain_button(ui, "Check the extras", &pal);
                crate::ui::note_control(ui, e.rect, "duplicates extras");
                extras = e.clicked();
            }
        });
    });
    if let Some(i) = toggle {
        st.sel = i;
        flip(st, i);
    }
    if extras {
        st.toggle_extras();
        st.refusal = None;
    }
    if cancel {
        return false;
    }
    if trash && !st.checked.is_empty() {
        let whole = st.whole_sets();
        if whole > 0 {
            st.refusal = Some(format!("Every copy is checked in {whole} {}: leave one of each unchecked", if whole == 1 { "set" } else { "sets" }));
            return true;
        }
        let paths: Vec<PathBuf> = st.sets.iter().flat_map(|s| s.files.iter()).filter(|c| st.checked.contains(&c.path)).map(|c| c.path.clone()).collect();
        let what = if paths.len() == 1 { format!("“{}”", crate::project::relative(&paths[0], &st.root)) } else { format!("{} files", paths.len()) };
        app.popup = Popup::confirm(format!("Move {what} ({}) to the trash?", crate::fsx::fmt_size(st.checked_size())), "Move to Trash", Box::new(move |app: &mut App| {
            app.actions.push(Action::StartJob(crate::ops::OpKind::Trash { paths }));
        }));
        return false;
    }
    true
}
