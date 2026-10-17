//! Recent Projects: the git repositories under the home folder, the ones last visited
//! first. Finding them walks the home folder on a thread; what was found is kept in the
//! cache folder, so the list opens at once next time and is brought up to date while it
//! shows. Branches come from `.git/HEAD` as written; whether a work tree has changes is
//! asked of git only for the rows on screen, a few at a time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime};

use egui::{CornerRadius, FontId, Rect, Sense, Vec2, pos2, vec2};

use crate::app::{Action, App};
use crate::popups::{self as pp, Popup};

/// How far below the home folder a repository is looked for (~/src/host/user/repo is 4).
const DEPTH: usize = 5;
/// Folders looked into at most: a home full of data must not keep a thread busy for long.
const VISITS: usize = 60_000;
/// Work trees asked about at once.
const ASKS: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    /// The branch checked out, or the commit's first seven digits when detached.
    pub branch: String,
    /// When git last wrote HEAD or the index: a commit, a checkout, a staged change.
    pub active: Option<SystemTime>,
}

/// The repositories under `home`: folders holding a `.git` (a folder, or a file in a work
/// tree or submodule). Not looked in: hidden folders, build output and installed
/// packages, links, other file systems, and a repository's own folders.
pub fn find(home: &Path, stop: &AtomicBool) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut out = vec![];
    let Ok(dev) = std::fs::metadata(home).map(|m| m.dev()) else { return out };
    let mut stack = vec![(home.to_path_buf(), 0usize)];
    let mut visits = 0;
    while let Some((d, depth)) = stack.pop() {
        visits += 1;
        if stop.load(Relaxed) || visits > VISITS {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let kids: Vec<(String, std::fs::FileType, PathBuf)> = rd.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.file_type().ok()?, e.path()))).collect();
        let has = |n: &str| kids.iter().any(|k| k.0 == n);
        if has(".git") {
            out.push(d.clone());
            // Dotfiles kept in git make the home folder one; what is under it still counts.
            if d != home {
                continue;
            }
        }
        if depth >= DEPTH {
            continue;
        }
        for (name, ft, path) in &kids {
            if name.starts_with('.') || !ft.is_dir() || crate::buildclean::kind_by_name(name, &has).is_some() {
                continue;
            }
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.dev() == dev) {
                stack.push((path.clone(), depth + 1));
            }
        }
    }
    out
}

/// Where a work tree's git data is: `.git` itself, or the folder a `.git` file names.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    let m = std::fs::metadata(&dot).ok()?;
    if m.is_dir() {
        return Some(dot);
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let to = PathBuf::from(text.lines().next()?.strip_prefix("gitdir:")?.trim());
    Some(if to.is_absolute() { to } else { root.join(to) })
}

/// The branch a HEAD file names: `ref: refs/heads/main` is main; a commit is shown short.
pub fn branch_of(head: &str) -> String {
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(r) => {
            let r = r.trim();
            r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
        }
        None => head.chars().take(7).collect(),
    }
}

/// A project as its files say now: its branch and when git last wrote to it. None when it
/// is no longer a repository.
pub fn read(root: &Path) -> Option<Project> {
    let gd = git_dir(root)?;
    let head = std::fs::read_to_string(gd.join("HEAD")).ok()?;
    let active = ["HEAD", "index"].iter().filter_map(|f| crate::config::mtime(&gd.join(f))).max();
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
    Some(Project { root: root.to_path_buf(), name, branch: branch_of(&head), active })
}

/// The projects in their order: the ones visited last first (the first recent inside
/// each, which a project's own subfolders count for), then the rest by their own latest
/// activity. A project inside another counts only for the innermost.
pub fn rank(v: Vec<Project>, recents: &[PathBuf]) -> Vec<Project> {
    let mut roots: Vec<&Path> = v.iter().map(|p| p.root.as_path()).collect();
    roots.sort_by_key(|r| std::cmp::Reverse(r.as_os_str().len()));
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    for (i, r) in recents.iter().enumerate() {
        if let Some(p) = roots.iter().find(|p| r.starts_with(p)) {
            seen.entry(p.to_path_buf()).or_insert(i);
        }
    }
    let key = |p: &Project| (seen.get(&p.root).copied().unwrap_or(usize::MAX), std::cmp::Reverse(p.active), p.name.to_lowercase());
    let keys: Vec<_> = v.iter().map(key).collect();
    let mut order: Vec<u32> = (0..v.len() as u32).collect();
    crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| keys[*a as usize].cmp(&keys[*b as usize]));
    let mut slots: Vec<Option<Project>> = v.into_iter().map(Some).collect();
    order.iter().filter_map(|&i| slots[i as usize].take()).collect()
}

// ------------------------------------------------------------------ the cache

fn cache_path() -> PathBuf {
    // Tests run outside the sandbox too: never the real cache.
    let base = if cfg!(test) { None } else { dirs::cache_dir() };
    base.unwrap_or_else(std::env::temp_dir).join("maia/projects.txt")
}

/// The roots found last time, one per line.
pub fn load_cache(path: &Path) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    std::fs::read(path).map(|b| b.split(|&c| c == b'\n').filter(|l| !l.is_empty()).map(|l| PathBuf::from(std::ffi::OsStr::from_bytes(l))).collect()).unwrap_or_default()
}

pub fn save_cache(path: &Path, roots: &[PathBuf]) -> std::io::Result<()> {
    let mut data = vec![];
    for r in roots.iter().filter(|r| !r.as_os_str().as_encoded_bytes().contains(&b'\n')) {
        data.extend_from_slice(r.as_os_str().as_encoded_bytes());
        data.push(b'\n');
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

thread_local! {
    /// The list as last shown in this process: the next opening starts from it.
    static LAST: std::cell::RefCell<Vec<Project>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub enum Msg {
    /// The projects, ranked; `done` once the walk is over.
    List(Vec<Project>, bool),
}

/// Whether a work tree has changes that aren't committed, untracked files included; None
/// when git can't say.
pub fn has_changes(root: &Path) -> Option<bool> {
    if !crate::media::have("git") {
        return None;
    }
    let mut c = crate::media::tool("git");
    c.arg("--no-optional-locks").args(crate::gitstatus::guard(root)).arg("-C").arg(root).args(["status", "--porcelain=v1", "-z", "--ignored=no"]);
    crate::media::output(c, Duration::from_secs(15)).map(|o| !o.is_empty())
}

// ------------------------------------------------------------------ the dialog

pub struct State {
    pub query: String,
    focus: bool,
    pub sel: usize,
    pub items: Vec<Project>,
    rx: Option<mpsc::Receiver<Msg>>,
    stop: Arc<AtomicBool>,
    pub done: bool,
    /// What git said about each work tree (None while it is being asked).
    pub changes: HashMap<PathBuf, Option<bool>>,
    answers: Arc<Mutex<Vec<(PathBuf, bool)>>>,
    asking: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

pub fn open(app: &App, ctx: &egui::Context) -> Popup {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let recents = app.ud.recents.clone();
    let home = crate::fsx::home();
    let (st, c) = (stop.clone(), ctx.clone());
    let _ = std::thread::Builder::new().name("projects".into()).spawn(move || {
        let cache = cache_path();
        let known: Vec<Project> = load_cache(&cache).iter().filter_map(|r| read(r)).collect();
        if !known.is_empty() {
            let _ = tx.send(Msg::List(rank(known, &recents), false));
            c.request_repaint();
        }
        let roots = find(&home, &st);
        if st.load(Relaxed) {
            return;
        }
        let found: Vec<Project> = roots.iter().filter_map(|r| read(r)).collect();
        let ranked = rank(found, &recents);
        let _ = save_cache(&cache, &ranked.iter().map(|p| p.root.clone()).collect::<Vec<_>>());
        let _ = tx.send(Msg::List(ranked, true));
        c.request_repaint();
    });
    let items = LAST.with(|l| rank(l.borrow().clone(), &app.ud.recents));
    Popup::Projects(State { query: String::new(), focus: true, sel: 0, items, rx: Some(rx), stop, done: false, changes: HashMap::new(), answers: Default::default(), asking: Default::default() })
}

impl State {
    /// Ask git about these work trees, a few at a time, unless asked already.
    fn ask(&mut self, roots: &[PathBuf], ctx: &egui::Context) {
        for r in roots {
            if self.asking.load(Relaxed) >= ASKS {
                return;
            }
            if self.changes.contains_key(r) {
                continue;
            }
            self.changes.insert(r.clone(), None);
            self.asking.fetch_add(1, Relaxed);
            let (root, answers, asking, stop, ctx) = (r.clone(), self.answers.clone(), self.asking.clone(), self.stop.clone(), ctx.clone());
            let left = asking.clone();
            let spawned = std::thread::Builder::new().name("project status".into()).spawn(move || {
                let v = if stop.load(Relaxed) { None } else { has_changes(&root) };
                if let (Some(v), Ok(mut a)) = (v, answers.lock()) {
                    a.push((root, v));
                }
                asking.fetch_sub(1, Relaxed);
                ctx.request_repaint();
            });
            if spawned.is_err() {
                left.fetch_sub(1, Relaxed);
                return;
            }
        }
    }

    /// The rows the query keeps, best first while typing.
    pub fn shown(&self) -> Vec<usize> {
        let q = crate::fuzzy::Query::new(&self.query);
        if q.is_empty() {
            return (0..self.items.len()).collect();
        }
        let mut hits: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let name = crate::fuzzy::score(&q, &p.name.to_lowercase()).map(|s| s + 1000);
                let path = crate::fuzzy::score(&q, &pp::home_short(&p.root).to_lowercase());
                name.max(path).map(|s| (s, i))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|h| h.1).collect()
    }
}

/// Recent Projects: type to narrow, Enter opens here, Ctrl+Enter in a new tab.
pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    let k = pp::take_keys(ctx);
    if k.esc {
        return false;
    }
    if let Some(rx) = &st.rx {
        while let Ok(Msg::List(v, done)) = rx.try_recv() {
            let at = st.shown().get(st.sel).and_then(|&i| st.items.get(i)).map(|p| p.root.clone());
            st.items = v;
            if let Some(at) = at {
                let rows = st.shown();
                if let Some(j) = rows.iter().position(|&i| st.items[i].root == at) {
                    st.sel = j;
                }
            }
            if done {
                st.done = true;
                LAST.with(|l| *l.borrow_mut() = st.items.clone());
            }
        }
        if st.done {
            st.rx = None;
        }
    }
    if let Ok(mut a) = st.answers.lock() {
        for (r, v) in a.drain(..) {
            st.changes.insert(r, Some(v));
        }
    }
    let rows = st.shown();
    pp::step(&mut st.sel, rows.len(), &k, |_| false);
    let fs = app.cfg.font_size;
    let small = fs - 1.5;
    let mut open: Option<(PathBuf, bool, bool)> = match rows.get(st.sel) {
        Some(&i) if k.enter || k.ctrl_enter => Some((st.items[i].root.clone(), k.ctrl_enter, true)),
        _ => None,
    };
    let mods = ctx.input(|i| i.modifiers);
    let list_max = pp::fit_list(ctx, 460.0, 110.0);
    let mut reset_sel = false;
    let mut visible: Vec<PathBuf> = vec![];
    pp::centered(ctx, "projects", 680.0, &pal, |ui| {
        let r = pp::search_field(ui, &mut st.query, "Open a project…", &mut st.focus, &pal, crate::theme::Glyph::Search);
        crate::ui::note_control(ui, r.rect, "projects field");
        reset_sel = r.changed();
        let row_h = (fs * 1.35 + 12.0).round();
        let n = rows.len();
        if n > 0 {
            let mut area = egui::ScrollArea::vertical().id_salt("projects list").max_height(list_max).min_scrolled_height((n as f32 * row_h).min(list_max)).auto_shrink([false, true]);
            if k.up || k.down || k.page_up || k.page_down {
                let most = (n as f32 * row_h - list_max).max(0.0);
                area = area.vertical_scroll_offset((st.sel as f32 * row_h - list_max / 2.0 + row_h / 2.0).clamp(0.0, most));
            }
            area.show_rows(ui, row_h, n, |ui, range| {
                for j in range {
                    let p = &st.items[rows[j]];
                    visible.push(p.root.clone());
                    let on = j == st.sel;
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                    if on {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                    }
                    let (tc, dc) = if on { (pal.on_sel(), pal.on_sel_dim()) } else { (pal.text, pal.text_dim) };
                    // Right to left: the branch, then a dot for uncommitted changes.
                    let mut x = r.right() - 10.0;
                    let bw = (r.width() * 0.28).min(200.0);
                    let bg = crate::listing::elided(ui, &p.branch, FontId::proportional(small), bw, dc);
                    x -= bg.size().x;
                    ui.painter().galley(pos2(x, r.center().y - bg.size().y / 2.0), bg, dc);
                    let ic = Rect::from_center_size(pos2(x - 10.0, r.center().y), Vec2::splat(small));
                    crate::theme::paint_lucide(ui.painter(), ic, "git-branch", dc);
                    x = ic.left() - 8.0;
                    if st.changes.get(&p.root).copied().flatten() == Some(true) {
                        let c = pos2(x - 4.0, r.center().y);
                        ui.painter().circle_filled(c, 3.5, crate::gitstatus::AMBER);
                        let tip = ui.interact(Rect::from_center_size(c, Vec2::splat(12.0)), egui::Id::new(("project changes", j)), Sense::hover());
                        tip.on_hover_text("Uncommitted changes");
                        x -= 16.0;
                    }
                    let left = r.left() + 12.0;
                    let room = x - 12.0 - left;
                    let name = crate::listing::elided(ui, &p.name, FontId::proportional(fs), room.min(room * 0.6 + 40.0), tc);
                    let nw = name.size().x;
                    ui.painter().galley(pos2(left, r.center().y - name.size().y / 2.0), name, tc);
                    let path = pp::home_short(&p.root);
                    let pw = room - nw - 12.0;
                    if pw > 30.0 {
                        let pg = crate::listing::elided_tail(ui, &path, FontId::proportional(small), pw, dc);
                        ui.painter().galley(pos2(left + nw + 12.0, r.center().y - pg.size().y / 2.0), pg, dc);
                    }
                    crate::ui::note_control(ui, r.shrink(1.0), "project row");
                    if resp.clicked() {
                        open = Some((p.root.clone(), mods.command, true));
                    } else if resp.middle_clicked() {
                        open = Some((p.root.clone(), true, false));
                    }
                }
            });
        } else {
            let msg = if !st.done && st.items.is_empty() {
                "Looking for git repositories in your home folder…"
            } else if st.items.is_empty() {
                "No git repositories in your home folder"
            } else {
                "No project by that name"
            };
            ui.add_space(4.0);
            ui.add(egui::Label::new(egui::RichText::new(msg).color(pal.text_dim)).wrap());
            ui.add_space(4.0);
        }
        pp::hints(ui, &[("Enter", "open"), ("Ctrl+Enter", "new tab"), ("Esc", "close")], &pal);
    });
    if reset_sel {
        st.sel = 0;
    }
    st.ask(&visible, ctx);
    if let Some((path, new_tab, focus)) = open {
        let panel = app.active;
        app.actions.push(if new_tab { Action::NewTab { panel, path, select: None, focus } } else { Action::Navigate { panel, path } });
        return false;
    }
    true
}
