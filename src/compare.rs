//! Compare Panels: entries one panel's folder has and the other's lacks, entries newer
//! than their counterpart, and same-named files of another size. Folders count by name
//! only. The marks follow reloads of either folder and clear when either panel moves.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Painter, Rect, pos2};

use crate::app::App;
use crate::commands::Cmd;
use crate::fsx::Entry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diff {
    Only,
    Newer,
    Size,
}

/// (panel, tab, folder, when its listing landed) for both sides, and the marks of both.
struct Pair {
    sides: [(u64, u64, PathBuf, Option<Instant>); 2],
    marks: HashMap<PathBuf, Diff>,
}

thread_local! {
    static CMP: RefCell<Option<Pair>> = const { RefCell::new(None) };
}

/// Marks for `mine` against `theirs`, keyed by the paths in `mine`.
pub fn side(mine: &[&Entry], theirs: &[&Entry], out: &mut HashMap<PathBuf, Diff>) {
    let by: HashMap<&str, &Entry> = theirs.iter().map(|e| (e.name.as_str(), *e)).collect();
    for e in mine {
        let d = match by.get(e.name.as_str()) {
            Some(o) if o.is_dir == e.is_dir => {
                if e.is_dir {
                    continue;
                }
                // A second of slack: some filesystems keep times only to the second (or two).
                if e.modified > o.modified.and_then(|t| t.checked_add(Duration::from_secs(1))) {
                    Diff::Newer
                } else if e.size != o.size {
                    Diff::Size
                } else {
                    continue;
                }
            }
            _ => Diff::Only,
        };
        out.insert(e.path.clone(), d);
    }
}

/// The panel F5 would copy to: the one focused before, else any other.
pub fn other(app: &App) -> Option<u64> {
    if app.prev_active != app.active && app.panels.contains_key(&app.prev_active) {
        return Some(app.prev_active);
    }
    let mut leaves = vec![];
    app.tree.leaves(&mut leaves);
    leaves.into_iter().find(|&l| l != app.active)
}

fn build(app: &App, a: u64, b: u64) -> Option<Pair> {
    let (ta, tb) = (app.panels.get(&a)?.tab(), app.panels.get(&b)?.tab());
    if ta.flatten || tb.flatten || crate::tab::is_computer(&ta.path) || crate::tab::is_computer(&tb.path) {
        return None;
    }
    fn shown(t: &crate::tab::Tab) -> Vec<&Entry> {
        t.view.iter().map(|&i| &t.entries[i as usize]).filter(|e| e.depth == 0).collect()
    }
    let (ea, eb) = (shown(ta), shown(tb));
    let mut marks = HashMap::new();
    side(&ea, &eb, &mut marks);
    side(&eb, &ea, &mut marks);
    Some(Pair { sides: [(a, ta.id, ta.path.clone(), ta.loaded_at), (b, tb.id, tb.path.clone(), tb.loaded_at)], marks })
}

pub fn run(app: &mut App, cmd: Cmd) {
    // Selecting and syncing compare afresh first, so they act on what is on screen now.
    let Some(o) = other(app) else {
        app.toast("Compare needs two panels");
        return;
    };
    let Some(pair) = build(app, app.active, o) else {
        app.toast("Compare works on two folder listings");
        return;
    };
    let (path, dest) = (pair.sides[0].2.clone(), pair.sides[1].2.clone());
    let mine: Vec<PathBuf> = pair.marks.iter().filter(|(p, d)| p.parent() == Some(&path) && (cmd != Cmd::SyncPanels || **d != Diff::Size)).map(|(p, _)| p.clone()).collect();
    let n = pair.marks.len();
    CMP.with(|c| *c.borrow_mut() = Some(pair));
    match cmd {
        Cmd::SelectDifferences => {
            let t = app.tab();
            t.selected = t.view.iter().copied().filter(|&i| mark(&t.entries[i as usize].path).is_some()).collect();
        }
        Cmd::SyncPanels if mine.is_empty() => app.toast("Nothing here is missing or newer there"),
        Cmd::SyncPanels => app.begin_transfer(mine, dest, false),
        _ if n == 0 => app.toast("No differences"),
        _ => app.toast(format!("{} different here, {} there", mine.len(), n - mine.len())),
    }
}

/// Once a frame: drop the marks when either panel has moved, compare again when either
/// folder's listing was reloaded (a sync or undo landing, a change on disk).
pub fn pump(app: &App) {
    CMP.with(|c| {
        let mut c = c.borrow_mut();
        let Some(p) = c.as_ref() else { return };
        let mut reloaded = false;
        for (pid, tid, path, at) in &p.sides {
            match app.panels.get(pid).map(|p| p.tab()) {
                Some(t) if t.id == *tid && t.path == *path && !t.flatten => {
                    if t.is_loading() || t.dirty {
                        return;
                    }
                    reloaded |= t.loaded_at != *at;
                }
                _ => {
                    *c = None;
                    return;
                }
            }
        }
        if reloaded {
            *c = build(app, p.sides[0].0, p.sides[1].0);
        }
    });
}

pub fn mark(path: &Path) -> Option<Diff> {
    CMP.with(|c| c.borrow().as_ref()?.marks.get(path).copied())
}

/// A short bar along the left edge of `r`: accent where only this side has the entry,
/// green where it is newer, amber where only the size differs.
pub fn paint(p: &Painter, r: Rect, path: &Path, pal: &crate::theme::Palette) {
    let Some(d) = mark(path) else { return };
    let col = match d {
        Diff::Only => pal.accent,
        Diff::Newer => crate::gitstatus::GREEN,
        Diff::Size => crate::gitstatus::AMBER,
    };
    p.rect_filled(Rect::from_min_max(pos2(r.left() + 1.0, r.top() + 3.0), pos2(r.left() + 3.5, r.bottom() - 3.0)), 1.0, col);
}
