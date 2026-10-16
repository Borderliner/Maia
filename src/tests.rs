//! Unit tests for the core (non-UI) logic.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::commands::{self, Cmd, Keymap};
use crate::config::{SortKey, ViewSettings};
use crate::fuzzy;
use crate::ops::{self, Conflict, OpKind};
use crate::panel::{Node, Side};
use crate::rename::{Outcome, Rename};
use crate::tab::{self, Tab, ViewCtx};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("maia-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn touch(p: &Path, content: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

#[test]
fn fuzzy_ranks_prefix_and_boundaries() {
    let q = fuzzy::Query::new("dl");
    assert!(fuzzy::score(&q, "downloads").is_some());
    assert!(fuzzy::score(&q, "xyz").is_none());
    let pre = fuzzy::score(&fuzzy::Query::new("doc"), "documents").unwrap();
    let mid = fuzzy::score(&fuzzy::Query::new("doc"), "my_old_doc_backup").unwrap();
    assert!(pre > mid, "prefix should outrank middle: {pre} vs {mid}");
    let (_, pos) = fuzzy::positions(&fuzzy::Query::new("rmd"), "README.md").unwrap();
    assert_eq!(pos.len(), 3);
    assert_eq!(pos[0], 0);
}

#[test]
fn natural_sort_orders_numbers() {
    let mut v = vec!["file10", "file2", "file1", "File3", "file02"];
    v.sort_by(|a, b| tab::natural_cmp(&a.to_lowercase(), &b.to_lowercase()));
    assert_eq!(v, vec!["file1", "file2", "file02", "File3", "file10"]);
}

#[test]
fn rename_selects_stem_and_mirrors_edits() {
    let items = vec![(0, PathBuf::from("/x/01 alpha.txt"), "01 alpha.txt".to_string()), (1, PathBuf::from("/x/02 beta.txt"), "02 beta.txt".to_string())];
    let mut r = Rename::new(items, false);
    assert_eq!(r.fields[0].selection(), (0, 8)); // stem only
    // Home, then shift+right twice selects "01"/"02"; delete removes them everywhere.
    use egui::{Event, Key, Modifiers};
    let key = |k: Key, m: Modifiers| Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: m };
    let mut clip = None;
    let ev = vec![key(Key::Home, Modifiers::NONE), key(Key::ArrowRight, Modifiers::SHIFT), key(Key::ArrowRight, Modifiers::SHIFT), key(Key::Delete, Modifiers::NONE)];
    assert_eq!(r.handle(&ev, &mut clip), Outcome::Continue);
    assert_eq!(r.fields[0].text_string(), " alpha.txt");
    assert_eq!(r.fields[1].text_string(), " beta.txt");
    r.insert_incremental_id();
    assert_eq!(r.fields[0].text_string(), "1 alpha.txt");
    assert_eq!(r.fields[1].text_string(), "2 beta.txt");
    // Multi-line paste distributes lines.
    r.select_all();
    r.paste("a.txt\nb.txt");
    assert_eq!(r.fields[0].text_string(), "a.txt");
    assert_eq!(r.fields[1].text_string(), "b.txt");
    let ev = vec![key(Key::Enter, Modifiers::NONE)];
    assert_eq!(r.handle(&ev, &mut clip), Outcome::Commit);
}

#[test]
fn rename_detects_conflicts_and_swaps_safely() {
    let d = tmpdir("rename");
    touch(&d.join("a.txt"), "A");
    touch(&d.join("b.txt"), "B");
    touch(&d.join("c.txt"), "C");
    // Swap a <-> b (allowed: both are being renamed).
    let items = vec![(0, d.join("a.txt"), "a.txt".into()), (1, d.join("b.txt"), "b.txt".into())];
    let mut r = Rename::new(items, false);
    r.select_all();
    r.paste("b.txt\na.txt");
    assert!(r.conflicts().is_empty());
    let errs = crate::rename::apply(&r.plan());
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "B");
    assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "A");
    // Renaming onto an existing, unrelated file is a conflict.
    let mut r = Rename::new(vec![(0, d.join("a.txt"), "a.txt".into())], false);
    r.select_all();
    r.insert_all("c.txt");
    assert!(!r.conflicts().is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hotkeys_parse_and_lookup() {
    let b = commands::parse_binding("Ctrl+Shift+P").unwrap();
    assert_eq!(commands::binding_to_string(&b), "Ctrl+Shift+P");
    let seq = commands::parse_binding("G, H").unwrap();
    assert_eq!(seq.len(), 2);
    let km = Keymap::build(&Default::default());
    let (hit, _) = km.lookup(&commands::parse_binding("Ctrl+Shift+P").unwrap());
    assert_eq!(hit, Some(Cmd::Palette));
    let (hit, _) = km.lookup(&commands::parse_binding("Ctrl+E").unwrap());
    assert_eq!(hit, Some(Cmd::ToggleFlatten));
    // Every default binding parses, and ids are unique.
    let mut ids = HashSet::new();
    for &c in commands::ALL {
        assert!(ids.insert(c.info().id), "duplicate id {}", c.info().id);
        for k in c.info().keys {
            assert!(commands::parse_binding(k).is_some(), "bad default key {k} for {}", c.info().id);
        }
    }
    // Overrides with a sequence.
    let mut ov = std::collections::BTreeMap::new();
    ov.insert("home".to_string(), vec!["G, H".to_string()]);
    let km = Keymap::build(&ov);
    let g = commands::parse_binding("G").unwrap();
    let (exact, longer) = km.lookup(&g);
    assert!(exact.is_none() && longer);
}

#[test]
fn default_hotkeys_have_no_conflicts() {
    let km = Keymap::build(&Default::default());
    let mut seen: HashMap<String, Cmd> = HashMap::new();
    for &c in commands::ALL {
        for b in km.bindings(c) {
            let s = commands::binding_to_string(b);
            if let Some(o) = seen.insert(s.clone(), c) {
                panic!("{s} bound to both {:?} and {:?}", o, c);
            }
        }
    }
}

#[test]
fn split_tree_operations() {
    let mut t = Node::Leaf(1);
    assert!(t.split(1, 2, Side::Right));
    assert!(t.split(2, 3, Side::Bottom));
    let mut leaves = vec![];
    t.leaves(&mut leaves);
    assert_eq!(leaves, vec![1, 2, 3]);
    assert!(t.remove(2));
    leaves.clear();
    t.leaves(&mut leaves);
    assert_eq!(leaves, vec![1, 3]);
    assert!(t.remove(1));
    assert!(matches!(t, Node::Leaf(3)));
}

#[test]
fn uri_roundtrip() {
    let p = PathBuf::from("/home/u/My Files/naïve #1.txt");
    let u = crate::clipboard::path_to_uri(&p);
    assert!(u.starts_with("file:///home/u/My%20Files/"));
    assert_eq!(crate::clipboard::uri_to_path(&u).unwrap(), p);
    assert_eq!(crate::clipboard::quote_if_needed("/a/b"), "/a/b");
    assert_eq!(crate::clipboard::quote_if_needed("/a b/it's"), "'/a b/it'\\''s'");
}

#[test]
fn unique_names() {
    let d = tmpdir("unique");
    touch(&d.join("x.txt"), "");
    assert_eq!(crate::fsx::unique_dest(&d, "x.txt"), d.join("x (1).txt"));
    assert_eq!(crate::fsx::unique_dest(&d, "y.txt"), d.join("y.txt"));
    std::fs::create_dir(d.join("New folder")).unwrap();
    assert_eq!(crate::fsx::unique_dest(&d, "New folder"), d.join("New folder (1)"));
    let _ = std::fs::remove_dir_all(&d);
}

fn wait_job(j: &ops::Job) {
    let start = Instant::now();
    while !j.progress.lock().unwrap().finished {
        assert!(start.elapsed() < Duration::from_secs(10), "job timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn copy_move_merge_and_conflicts() {
    let ctx = egui::Context::default();
    let d = tmpdir("ops");
    let src = d.join("src");
    let dst = d.join("dst");
    touch(&src.join("a.txt"), "new a");
    touch(&src.join("sub/b.txt"), "b");
    touch(&dst.join("a.txt"), "old a");
    touch(&dst.join("sub/keep.txt"), "keep");
    // Keep both
    let j = ops::spawn(1, OpKind::Copy { srcs: vec![src.join("a.txt")], dest: dst.clone(), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert_eq!(std::fs::read_to_string(dst.join("a (1).txt")).unwrap(), "new a");
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "old a");
    // Overwrite + merge folders
    let mut decide = HashMap::new();
    decide.insert(src.join("a.txt"), Conflict::Overwrite);
    decide.insert(src.join("sub"), Conflict::Merge);
    let j = ops::spawn(2, OpKind::Copy { srcs: vec![src.join("a.txt"), src.join("sub")], dest: dst.clone(), decide }, ctx.clone());
    wait_job(&j);
    assert!(j.progress.lock().unwrap().errors.is_empty(), "{:?}", j.progress.lock().unwrap().errors);
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "new a");
    assert!(dst.join("sub/keep.txt").exists() && dst.join("sub/b.txt").exists());
    // Skip
    let mut decide = HashMap::new();
    decide.insert(src.join("a.txt"), Conflict::Skip);
    touch(&src.join("a.txt"), "newer a");
    let j = ops::spawn(3, OpKind::Copy { srcs: vec![src.join("a.txt")], dest: dst.clone(), decide }, ctx.clone());
    wait_job(&j);
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "new a");
    // Move into a fresh folder
    let other = d.join("other");
    std::fs::create_dir(&other).unwrap();
    let j = ops::spawn(4, OpKind::Move { srcs: vec![src.join("sub")], dest: other.clone(), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(other.join("sub/b.txt").exists() && !src.join("sub").exists());
    assert_eq!(j.progress.lock().unwrap().produced.len(), 1);
    // Copying a folder into itself is refused.
    let j = ops::spawn(5, OpKind::Copy { srcs: vec![other.clone()], dest: other.join("sub"), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(!j.progress.lock().unwrap().errors.is_empty());
    // Permanent delete
    let j = ops::spawn(6, OpKind::Delete { paths: vec![other.clone()] }, ctx);
    wait_job(&j);
    assert!(!other.exists());
    let _ = std::fs::remove_dir_all(&d);
}

fn pump(t: &mut Tab, stats: &HashMap<PathBuf, crate::stats::DirStats>) {
    let recents = HashSet::new();
    let ctx = ViewCtx { show_hidden: false, show_system: false, folders_first: true, recents_on_top: false, recents: &recents, stats };
    let start = Instant::now();
    while t.is_loading() {
        t.poll();
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    t.poll();
    t.rebuild(&ctx);
}

#[test]
fn tab_listing_sort_filter_flatten() {
    let d = tmpdir("tab");
    touch(&d.join("b10.txt"), "1234567890");
    touch(&d.join("b2.txt"), "1");
    touch(&d.join(".hidden"), "");
    touch(&d.join("zdir/deep/needle.rs"), "");
    touch(&d.join("adir/x.png"), "");
    let ctx = egui::Context::default();
    let stats = HashMap::new();
    let mut t = Tab::new(1, d.clone(), ViewSettings::default(), ctx);
    pump(&mut t, &stats);
    let names: Vec<&str> = t.view.iter().map(|&i| t.entries[i as usize].name.as_str()).collect();
    assert_eq!(names, vec!["adir", "zdir", "b2.txt", "b10.txt"], "folders first, natural order, hidden excluded");
    assert_eq!(t.hidden_count, 1);
    assert_eq!(t.cursor, Some(t.view[0]), "cursor rests on the first item");
    assert!(t.selected.is_empty(), "no selection on entry");
    assert_eq!(t.counts, (2, 2, 2, 3), "totals include the hidden file");
    // Size sort defaults to descending.
    t.set_sort(SortKey::Size);
    assert!(t.settings.desc);
    pump(&mut t, &stats);
    let files: Vec<&str> = t.view.iter().map(|&i| t.entries[i as usize].name.as_str()).filter(|n| n.ends_with(".txt")).collect();
    assert_eq!(files, vec!["b10.txt", "b2.txt"]);
    // Filter
    t.filter = "b1".into();
    t.dirty = true;
    pump(&mut t, &stats);
    assert_eq!(t.view.len(), 1);
    // Flatten finds nested files, filter matches relative path.
    t.filter = "needle".into();
    t.set_flatten(true);
    pump(&mut t, &stats);
    let found: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].rel().map(|r| r.into_owned()).unwrap_or_default()).collect();
    assert_eq!(found, vec!["zdir/deep/needle.rs".to_string()]);
    // Flattened lists are files only.
    t.filter.clear();
    t.dirty = true;
    pump(&mut t, &stats);
    assert!(t.view.iter().all(|&i| !t.entries[i as usize].is_dir), "flatten shows files only");
    // Type filter: only the checked types are shown.
    t.set_flatten(false);
    let txt = crate::mime::mime_db().label("a.txt", "txt", false, false);
    t.settings.only_types = vec![txt.clone()];
    t.dirty = true;
    pump(&mut t, &stats);
    assert!(!t.view.is_empty());
    assert!(t.view.iter().all(|&i| t.entries[i as usize].ext() == "txt"));
    t.settings.only_types.clear();
    // Footer toggles hide folders / files.
    t.settings.show_folders = false;
    t.dirty = true;
    pump(&mut t, &stats);
    assert!(t.view.iter().all(|&i| !t.entries[i as usize].is_dir));
    t.settings.show_folders = true;
    // Details view has no tree; Tree view (10% zoom) unfolds folders inline, indented.
    assert!(!t.tree_active());
    t.settings.zoom = 0.1;
    assert!(t.tree_active());
    t.dirty = true;
    pump(&mut t, &stats);
    t.toggle_expand(&d.join("zdir"));
    pump(&mut t, &stats);
    let names: Vec<(String, u8)> = t.view.iter().map(|&i| (t.entries[i as usize].name.clone(), t.entries[i as usize].depth)).collect();
    let zpos = names.iter().position(|(n, _)| n == "zdir").unwrap();
    assert_eq!(names[zpos + 1], ("deep".to_string(), 1), "child right below its parent: {names:?}");
    t.toggle_expand(&d.join("zdir/deep"));
    pump(&mut t, &stats);
    assert!(t.view.iter().any(|&i| t.entries[i as usize].name == "needle.rs" && t.entries[i as usize].depth == 2));
    // Filtering in Tree view keeps the hierarchy: the match stays under its folders,
    // which are kept as dimmed context because they don't match themselves.
    t.filter = "needle".into();
    t.dirty = true;
    pump(&mut t, &stats);
    let names: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
    assert_eq!(names, vec!["zdir", "deep", "needle.rs"], "{names:?}");
    let dim: Vec<String> = t.dimmed.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
    assert!(dim.contains(&"zdir".to_string()) && dim.contains(&"deep".to_string()) && !dim.contains(&"needle.rs".to_string()));
    t.filter.clear();
    t.dirty = true;
    pump(&mut t, &stats);
    t.toggle_expand(&d.join("zdir"));
    pump(&mut t, &stats);
    assert!(!t.view.iter().any(|&i| t.entries[i as usize].depth > 0), "collapsing hides descendants");
    // Ctrl+E in Tree view unfolds every folder recursively (and the title gets the […] mark).
    t.toggle_expand_all();
    for _ in 0..50 {
        pump(&mut t, &stats);
        if !t.is_loading() {
            break;
        }
    }
    assert!(t.view.iter().any(|&i| t.entries[i as usize].name == "needle.rs"), "expand all reaches deep files");
    assert!(t.title().ends_with("[…]"));
    t.toggle_expand_all();
    pump(&mut t, &stats);
    assert!(!t.view.iter().any(|&i| t.entries[i as usize].depth > 0));
    t.settings.zoom = 0.0;
    // Navigation + history + up selects the child.
    t.navigate(d.join("zdir"), None);
    pump(&mut t, &stats);
    assert!(t.can_back());
    t.up(|_| None);
    pump(&mut t, &stats);
    assert_eq!(t.cursor_path(), Some(d.join("zdir")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn group_by_name_ranges() {
    let d = tmpdir("group");
    for n in ["apple", "kiwi", "zebra", "_x"] {
        touch(&d.join(n), "");
    }
    let mut v = ViewSettings::default();
    v.group = crate::config::GroupBy::Name;
    let mut t = Tab::new(1, d.clone(), v, egui::Context::default());
    pump(&mut t, &HashMap::new());
    let headers: Vec<String> = t.header_labels();
    assert_eq!(headers, vec!["A – H", "I – P", "Q – Z", "Other"]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn view_settings_inheritance() {
    use crate::config::{SaveScope, SavedView, UserData};
    let mut ud = UserData::default();
    let mut v = ViewSettings::default();
    v.zoom = 0.6;
    ud.views.insert(PathBuf::from("/a"), SavedView { scope: SaveScope::Subfolders, view: v.clone() });
    assert_eq!(ud.view_for(Path::new("/a/b/c")).0.zoom, 0.6);
    ud.views.insert(PathBuf::from("/x"), SavedView { scope: SaveScope::ThisFolder, view: v });
    assert_eq!(ud.view_for(Path::new("/x/y")).0.zoom, 0.0);
}

#[test]
fn mime_lookup() {
    let db = crate::mime::mime_db();
    assert_eq!(db.for_name("x.png", false, false), "image/png");
    assert_eq!(db.for_name("dir", true, false), "inode/directory");
    assert_eq!(db.for_name("a.tar.gz", false, false), "application/x-compressed-tar");
}

#[test]
fn image_decoders_and_downscale() {
    let d = tmpdir("img");
    // PNG via the png encoder.
    let png_path = d.join("a.png");
    {
        let f = std::fs::File::create(&png_path).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(f), 40, 20);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        let mut w = e.write_header().unwrap();
        let data: Vec<u8> = (0..40 * 20).flat_map(|i| [(i % 40 * 6) as u8, 100, 200]).collect();
        w.write_image_data(&data).unwrap();
    }
    let (img, dims) = crate::imgdec::load(&png_path, 10).unwrap();
    assert_eq!(dims, [40, 20]);
    assert_eq!((img.w, img.h), (10, 5));
    assert_eq!(img.px.len(), 10 * 5 * 4);
    assert_eq!(img.px[3], 255);
    // 24-bit bottom-up BMP, 2x2: red, green / blue, white (rows stored bottom first).
    let mut bmp = vec![];
    let row = |a: [u8; 3], b: [u8; 3]| [a[2], a[1], a[0], b[2], b[1], b[0], 0, 0];
    let pixels = [row([0, 0, 255], [255, 255, 255]), row([255, 0, 0], [0, 255, 0])].concat();
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&2i32.to_le_bytes());
    bmp.extend_from_slice(&2i32.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    bmp.extend_from_slice(&pixels);
    let bmp_path = d.join("b.bmp");
    std::fs::write(&bmp_path, &bmp).unwrap();
    let (img, _) = crate::imgdec::load(&bmp_path, 64).unwrap();
    assert_eq!(&img.px[0..4], &[255, 0, 0, 255], "top-left red");
    assert_eq!(&img.px[12..16], &[255, 255, 255, 255], "bottom-right white");
    assert!(crate::imgdec::load(&d.join("missing.png"), 64).is_none());
    std::fs::write(d.join("junk.png"), b"not an image").unwrap();
    assert!(crate::imgdec::load(&d.join("junk.png"), 64).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn trash_roundtrip() {
    let d = tmpdir("trash");
    let f = d.join("doomed file.txt");
    touch(&f, "bye");
    crate::trashcan::delete(&f).unwrap();
    assert!(!f.exists());
    let items: Vec<_> = crate::trashcan::list().into_iter().filter(|i| i.original == f).collect();
    assert_eq!(items.len(), 1, "listed with its original path");
    assert!(items[0].file.exists() && items[0].info.exists());
    assert!(!items[0].deleted.is_empty());
    crate::trashcan::restore(&items[0]).unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "bye");
    // Trash again and purge.
    crate::trashcan::delete(&f).unwrap();
    let it = crate::trashcan::list().into_iter().find(|i| i.original == f).unwrap();
    crate::trashcan::purge(&it).unwrap();
    assert!(!it.file.exists() && !it.info.exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn inotify_reports_changes() {
    let d = tmpdir("watch");
    let mut w = crate::watch::Watch::new(egui::Context::default());
    w.sync(&[d.clone()].into_iter().collect());
    touch(&d.join("new.txt"), "x");
    let start = Instant::now();
    let mut got = vec![];
    while got.is_empty() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(30));
        got = w.poll();
    }
    assert_eq!(got, vec![d.clone()]);
    assert!(w.changed_files.contains(&d.join("new.txt")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn time_formatting() {
    let t = std::time::UNIX_EPOCH + Duration::from_secs(86_400 * 400);
    assert_eq!(crate::timefmt::format(t, "%Y"), "1971");
    let now = std::time::SystemTime::now();
    assert_eq!(crate::timefmt::local_day(now) - crate::timefmt::local_day(now - Duration::from_secs(86_400)), 1);
}

#[test]
fn desktop_exec_expansion() {
    let p = vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")];
    assert_eq!(crate::mime::expand_exec("gedit %U", &p, "Gedit"), vec!["gedit", "/tmp/a b.txt", "/tmp/c.txt"]);
    assert_eq!(crate::mime::expand_exec("vlc --started-from-file %f", &p[..1], "VLC"), vec!["vlc", "--started-from-file", "/tmp/a b.txt"]);
    assert_eq!(crate::mime::expand_exec("\"/opt/My App/run\" --x", &p[..1], "X"), vec!["/opt/My App/run", "--x", "/tmp/a b.txt"]);
    assert_eq!(crate::mime::expand_exec("app --name=%c %%", &[], "Zed"), vec!["app", "--name=Zed", "%"]);
}

#[test]
fn trash_metadata_columns_and_groups() {
    // A fake per-volume trash: files/ plus info/*.trashinfo.
    let d = tmpdir("trashmeta");
    let files = d.join(".Trash-1000/files");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::create_dir_all(d.join(".Trash-1000/info")).unwrap();
    touch(&files.join("old.txt"), "x");
    touch(&files.join("new.txt"), "y");
    let now = crate::timefmt::format(std::time::SystemTime::now(), "%Y-%m-%dT%H:%M:%S");
    touch(&d.join(".Trash-1000/info/old.txt.trashinfo"), "[Trash Info]\nPath=/srv/a/old.txt\nDeletionDate=2001-02-03T04:05:06\n");
    touch(&d.join(".Trash-1000/info/new.txt.trashinfo"), &format!("[Trash Info]\nPath=/srv/b/new.txt\nDeletionDate={now}\n"));
    assert!(crate::trashcan::is_trash_files(&files));
    assert!(!crate::trashcan::is_trash_files(&d));
    let m = crate::trashcan::info_for(&files);
    assert_eq!(m.get(&files.join("old.txt")).map(|x| x.0.clone()), Some(PathBuf::from("/srv/a/old.txt")));
    let t0 = crate::timefmt::parse_local("2001-02-03T04:05:06").unwrap();
    assert_eq!(crate::timefmt::format(t0, "%Y-%m-%dT%H:%M:%S"), "2001-02-03T04:05:06");
    // A tab on the trash groups by deletion date and by original folder.
    let stats = HashMap::new();
    let mut t = Tab::new(7, files.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    assert_eq!(t.trash_meta.len(), 2);
    t.settings.group = crate::config::GroupBy::Deleted;
    t.dirty = true;
    pump(&mut t, &stats);
    let heads: Vec<String> = t.header_labels();
    assert_eq!(heads, vec!["Today", "Older"]);
    t.settings.group = crate::config::GroupBy::Original;
    t.dirty = true;
    pump(&mut t, &stats);
    let heads: Vec<String> = t.header_labels();
    assert_eq!(heads, vec!["/srv/a", "/srv/b"]);
    let _ = std::fs::remove_dir_all(&d);
}

/// Renders the SVGs listed in $COMET_SVG_SAMPLES (one path per line) into $COMET_SVG_OUT
/// as PNGs, for eyeballing the renderer. Run with --ignored.
#[test]
#[ignore]
fn svg_render_samples() {
    let (Ok(list), Ok(out)) = (std::env::var("COMET_SVG_SAMPLES"), std::env::var("COMET_SVG_OUT")) else { return };
    std::fs::create_dir_all(&out).unwrap();
    for (i, p) in std::fs::read_to_string(list).unwrap().lines().enumerate() {
        let data = std::fs::read(p).unwrap();
        for px in [24u32, 64, 128] {
            let t = Instant::now();
            let Some((w, h, mut rgba)) = crate::svg::render(&data, px) else { eprintln!("FAILED {p}"); continue };
            // Unpremultiply for the PNG file.
            for c in rgba.chunks_exact_mut(4) {
                if c[3] > 0 && c[3] < 255 {
                    for k in 0..3 {
                        c[k] = ((c[k] as u32 * 255 + c[3] as u32 / 2) / c[3] as u32).min(255) as u8;
                    }
                }
            }
            eprintln!("{i:02} {px} {:?} {p}", t.elapsed());
            let f = std::fs::File::create(format!("{out}/{i:02}-{px}.png")).unwrap();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
    }
}

#[test]
fn zoom_steps_through_views_then_icon_sizes() {
    use crate::listing::zoom_step;
    // Up: one named view per step, then 5.5% icon steps up to 100%.
    let mut z = 0.0;
    let mut seen = vec![];
    for _ in 0..20 {
        z = zoom_step(z, 1);
        seen.push((z * 1000.0).round() as i32);
    }
    assert_eq!(&seen[..6], &[100, 200, 300, 355, 410, 465]);
    assert_eq!(*seen.last().unwrap(), 1000);
    // Down retraces through the dense views without skipping any.
    let mut z = 0.355;
    let mut down = vec![];
    for _ in 0..5 {
        z = zoom_step(z, -1);
        down.push((z * 1000.0).round() as i32);
    }
    assert_eq!(down, vec![300, 200, 100, 0, 0]);
    // From a slider position between stops, a step lands on the neighbouring stop.
    assert_eq!((zoom_step(0.15, 1) * 1000.0).round() as i32, 200);
    assert_eq!((zoom_step(0.15, -1) * 1000.0).round() as i32, 100);
}

#[test]
fn decoders_reject_huge_or_corrupt_headers_without_allocating() {
    let t = Instant::now();
    // GIF claiming a 65535x65535 screen (4.3 gigapixels) with one 1x1 frame.
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x80, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff]);
    gif.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0x02, 0x02, 0x44, 0x01, 0, 0x3b]);
    assert!(crate::imgdec::decode_bytes(&gif).is_none());
    // BMP claiming 32000x32000 pixels.
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&[0; 8]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&32000i32.to_le_bytes());
    bmp.extend_from_slice(&32000i32.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    assert!(crate::imgdec::decode_bytes(&bmp).is_none());
    // 8-bit BMP with a palette count of 4 billion must not try to build that palette.
    let mut pal = b"BM".to_vec();
    pal.extend_from_slice(&[0; 8]);
    pal.extend_from_slice(&58u32.to_le_bytes());
    pal.extend_from_slice(&40u32.to_le_bytes());
    pal.extend_from_slice(&1i32.to_le_bytes());
    pal.extend_from_slice(&1i32.to_le_bytes());
    pal.extend_from_slice(&1u16.to_le_bytes());
    pal.extend_from_slice(&8u16.to_le_bytes());
    pal.extend_from_slice(&0u32.to_le_bytes());
    pal.extend_from_slice(&[0; 12]);
    pal.extend_from_slice(&u32::MAX.to_le_bytes());
    pal.extend_from_slice(&[0; 4 + 4 + 4]);
    let _ = crate::imgdec::decode_bytes(&pal);
    assert!(t.elapsed() < Duration::from_secs(1), "rejected quickly");
}

#[test]
fn copy_skips_special_files_and_refuses_self_nesting_through_symlinks() {
    let ctx = egui::Context::default();
    let d = tmpdir("ops_edge");
    let src = d.join("src");
    touch(&src.join("ok.txt"), "ok");
    // A FIFO inside a copied folder: must be skipped, not opened (that would block forever).
    let fifo = std::ffi::CString::new(src.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let out = d.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let t = Instant::now();
    let j = ops::spawn(10, OpKind::Copy { srcs: vec![src.clone()], dest: out.clone(), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(t.elapsed() < Duration::from_secs(5), "copy finished despite the FIFO");
    assert_eq!(std::fs::read_to_string(out.join("src/ok.txt")).unwrap(), "ok");
    assert!(!out.join("src/pipe").exists());
    assert!(j.progress.lock().unwrap().errors.iter().any(|e| e.contains("special file")));
    // Copying `src` into a symlink that points inside `src` is copying it into itself.
    std::os::unix::fs::symlink(&src, d.join("alias")).unwrap();
    std::fs::create_dir_all(src.join("inner")).unwrap();
    let j = ops::spawn(11, OpKind::Copy { srcs: vec![src.clone()], dest: d.join("alias/inner"), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(j.progress.lock().unwrap().errors.iter().any(|e| e.contains("into itself")), "{:?}", j.progress.lock().unwrap().errors);
    assert!(!src.join("inner/src").exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn deleted_folder_is_flagged_gone_but_unreadable_is_not() {
    let stats = HashMap::new();
    let d = tmpdir("gone");
    let sub = d.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let mut t = Tab::new(21, sub.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    assert!(!t.gone);
    std::fs::remove_dir(&sub).unwrap();
    t.reload();
    pump(&mut t, &stats);
    assert!(t.gone, "missing folder reported as gone: {:?}", t.error);
    // A folder we can't read (permission denied) stays put with an error instead.
    let locked = d.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut t = Tab::new(22, locked.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let readable = std::fs::read_dir(&locked).is_ok(); // true when running as root
    if !readable {
        assert!(!t.gone && t.error.is_some());
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn special_files_are_labelled_and_never_executable() {
    let d = tmpdir("special");
    let f = d.join("p");
    let c = std::ffi::CString::new(f.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o755) }, 0);
    let e = crate::fsx::Entry::from_path(&f).unwrap();
    assert!(e.special && !e.is_executable());
    assert_eq!(e.special_kind(), Some("Pipe"));
    let n = crate::fsx::Entry::from_path(&d).unwrap();
    assert_eq!(n.special_kind(), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn entry_derives_lowercase_name_extension_and_relative_path() {
    let d = tmpdir("derive");
    std::fs::create_dir_all(d.join("Sub.D/inner")).unwrap();
    for n in ["Photo.JPG", ".bashrc", "archive.tar.gz", "file.", "plain", "..odd", "Sub.D/inner/x.Rs"] {
        std::fs::write(d.join(n), b"x").unwrap();
    }
    let e = |n: &str| crate::fsx::Entry::from_path(&d.join(n)).unwrap();
    let (p, b, a, f, pl, o, dir) = (e("Photo.JPG"), e(".bashrc"), e("archive.tar.gz"), e("file."), e("plain"), e("..odd"), e("Sub.D"));
    assert_eq!((p.name.as_str(), p.lname(), p.ext()), ("Photo.JPG", "photo.jpg", "jpg"));
    assert_eq!((b.ext(), b.hidden), ("", true));
    assert_eq!((a.lname(), a.ext()), ("archive.tar.gz", "gz"));
    assert_eq!((f.ext(), pl.ext(), o.ext(), dir.ext()), ("", "", "odd", ""));
    assert_eq!(dir.lname(), "sub.d");
    assert!(p.rel().is_none());
    // Renaming a volume entry keeps the derived fields in sync.
    let mut v = e("plain");
    v.set_name("My Disk".into());
    assert_eq!((v.lname(), v.ext()), ("my disk", ""));
    // Results of a recursive walk carry their path relative to the root.
    let w = crate::fsx::walk_async(d.clone(), true, egui::Context::default());
    while !w.done.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut rels: Vec<String> = w.rx.try_iter().flatten().filter_map(|e| e.rel().map(|r| r.into_owned())).collect();
    rels.sort();
    assert!(rels.contains(&"Sub.D/inner/x.Rs".to_string()) && rels.contains(&"plain".to_string()), "{rels:?}");
    assert!(std::mem::size_of::<crate::fsx::Entry>() <= 144, "Entry grew to {}", std::mem::size_of::<crate::fsx::Entry>());
    let _ = std::fs::remove_dir_all(&d);
}

/// The previous (char-based, allocating) natural order, kept as an oracle.
fn natural_cmp_reference(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| na.len().cmp(&nb.len()));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

#[test]
fn natural_cmp_matches_reference_on_random_names() {
    let alphabet: Vec<char> = "ab0123456789Z._- éß日😀".chars().collect();
    let mut x: u64 = 0x2545F4914F6CDD1D;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut names = vec![];
    for _ in 0..3000 {
        let n = (next() % 9) as usize;
        names.push((0..n).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect::<String>());
    }
    names.extend(["file2", "file10", "file02", "file002", "a1b2", "a1b10", "", "0", "00", "007", "7"].map(String::from));
    for a in &names {
        for b in names.iter().step_by(7) {
            assert_eq!(crate::tab::natural_cmp(a, b), natural_cmp_reference(a, b), "{a:?} vs {b:?}");
        }
    }
}

#[test]
fn fast_name_sort_orders_exactly_like_natural_order() {
    let d = tmpdir("namesort");
    let parts = ["a", "A", "b", "file", "File", "file1", "file10", "file2", "img_0001", "img_10", "z", "é", "Éa", "日本", "abcdefgh", "abcdefgi", "abcdefgh1", "abcdefgh10", "x.y", "00", "0", "007"];
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut made = std::collections::HashSet::new();
    for k in 0..400 {
        let n = 1 + (next() % 3) as usize;
        let name: String = (0..n).map(|_| parts[(next() % parts.len() as u64) as usize]).collect::<Vec<_>>().join("");
        if !made.insert(name.to_lowercase()) {
            continue;
        }
        if k % 5 == 0 {
            std::fs::create_dir_all(d.join(&name)).unwrap();
        } else {
            std::fs::write(d.join(&name), b"x").unwrap();
        }
    }
    let stats = HashMap::new();
    let recents = HashSet::new();
    for (folders_first, desc, filter) in [(true, false, ""), (true, true, ""), (false, false, ""), (false, true, ""), (true, false, "file")] {
        let mut t = Tab::new(31, d.clone(), ViewSettings::default(), egui::Context::default());
        pump(&mut t, &stats);
        t.settings.desc = desc;
        t.filter = filter.to_string();
        let ctx = ViewCtx { show_hidden: true, show_system: true, folders_first, recents_on_top: false, recents: &recents, stats: &stats };
        t.rebuild(&ctx);
        assert!(t.view.len() > if filter.is_empty() { 120 } else { 10 }, "{} items", t.view.len());
        let q = crate::fuzzy::Query::new(filter);
        let tier = |e: &crate::fsx::Entry| crate::fuzzy::score(&q, e.lname()).map(|s| (s + 40) / 25).unwrap_or(0);
        for w in t.view.windows(2) {
            let (a, b) = (&t.entries[w[0] as usize], &t.entries[w[1] as usize]);
            if !filter.is_empty() && tier(a) != tier(b) {
                assert!(tier(a) > tier(b), "better matches first");
                continue;
            }
            if a.is_dir != b.is_dir {
                assert_eq!(a.is_dir, folders_first, "{:?} before {:?}", a.name, b.name);
                continue;
            }
            let o = crate::tab::natural_cmp(a.lname(), b.lname());
            let o = if desc { o.reverse() } else { o };
            assert_ne!(o, std::cmp::Ordering::Greater, "{:?} before {:?} (desc {desc}, folders first {folders_first})", a.name, b.name);
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn parallel_sort_is_stable_and_matches_sort_by() {
    let mut x: u64 = 88172645463325252;
    for n in [0usize, 1, 39_999, 40_000, 40_001, 123_457, 200_000] {
        let v: Vec<(u32, u32)> = (0..n as u32)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x % 1000) as u32, i) // many equal keys; .1 records the original order
            })
            .collect();
        let mut a = v.clone();
        let mut b = v.clone();
        a.sort_by(|p, q| p.0.cmp(&q.0));
        crate::tab::par_sort_by(&mut b, &|p: &(u32, u32), q: &(u32, u32)| p.0.cmp(&q.0));
        assert_eq!(a, b, "n = {n}");
    }
}

#[test]
fn parallel_filter_path_keeps_every_match_in_order() {
    let d = tmpdir("parfilter");
    std::fs::write(d.join("seed.txt"), b"x").unwrap();
    std::fs::create_dir_all(d.join(".hid")).unwrap();
    let base = crate::fsx::Entry::from_path(&d.join("seed.txt")).unwrap();
    let words = ["alpha", "beta", "gamma", "delta", "lib", "Liberty", "x11", "file10", "file2", ".dot"];
    let mut entries = vec![];
    for i in 0..45_000u32 {
        let mut e = base.clone();
        let dir = if i % 97 == 0 { ".hid/" } else { "sub/" };
        let name = format!("{}{}.txt", words[(i as usize * 7) % words.len()], i % 1000);
        e.path = d.join(format!("{dir}{name}"));
        e.set_name(name);
        e.rel_at = (d.as_os_str().len() + 1) as u16;
        entries.push(e);
    }
    let stats = HashMap::new();
    let recents = HashSet::new();
    let mut t = Tab::new(41, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    t.flatten = true;
    t.set_entries(entries.clone());
    t.filter = "lib".into();
    let ctx = ViewCtx { show_hidden: false, show_system: false, folders_first: true, recents_on_top: false, recents: &recents, stats: &stats };
    t.rebuild(&ctx);
    let q = crate::fuzzy::Query::new("lib");
    let expect: usize = entries.iter().filter(|e| !e.hidden && !e.rel_hidden() && (crate::fuzzy::score(&q, e.lname()).is_some() || e.rel().is_some_and(|r| crate::fuzzy::score_ci(&q, &r).is_some()))).count();
    assert_eq!(t.view.len(), expect, "every visible match listed once");
    assert!(expect > 1000);
    let tier = |e: &crate::fsx::Entry| {
        let n = crate::fuzzy::score(&q, e.lname()).map(|s| s + 40);
        let p = e.rel().and_then(|r| crate::fuzzy::score_ci(&q, &r));
        n.max(p).unwrap() / 25
    };
    for w in t.view.windows(2) {
        let (a, b) = (&t.entries[w[0] as usize], &t.entries[w[1] as usize]);
        assert!(tier(a) > tier(b) || (tier(a) == tier(b) && crate::tab::natural_cmp(a.lname(), b.lname()) != std::cmp::Ordering::Greater), "{} before {}", a.name, b.name);
    }
    let hidden = entries.iter().filter(|e| e.rel_hidden()).count();
    assert_eq!(t.hidden_count, hidden);
    assert_eq!(t.counts.3, 45_000, "total files counted");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn compact_key_sorts_match_full_comparison_for_size_and_dates() {
    use crate::config::SortKey;
    let d = tmpdir("keysort");
    let base = std::time::SystemTime::UNIX_EPOCH;
    for i in 0..160u64 {
        let name = format!("{}{}", ["f", "F", "img", "a1b"][(i % 4) as usize], i % 37);
        let p = d.join(&name);
        if p.exists() {
            continue;
        }
        std::fs::write(&p, vec![b'x'; (i % 9) as usize * 100]).unwrap();
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        // Some timestamps before 1970, many equal ones.
        let t = if i % 11 == 0 { base - std::time::Duration::from_secs(1000 + i) } else { base + std::time::Duration::from_secs(1_700_000_000 + (i % 7) * 3600) };
        f.set_modified(t).unwrap();
    }
    let stats = HashMap::new();
    let recents = HashSet::new();
    for sort in [SortKey::Size, SortKey::Modified] {
        for desc in [false, true] {
            let mut t = Tab::new(51, d.clone(), ViewSettings::default(), egui::Context::default());
            pump(&mut t, &stats);
            t.settings.sort = sort;
            t.settings.desc = desc;
            let ctx = ViewCtx { show_hidden: true, show_system: true, folders_first: true, recents_on_top: false, recents: &recents, stats: &stats };
            t.rebuild(&ctx);
            assert!(t.view.len() > 100);
            for w in t.view.windows(2) {
                let (a, b) = (&t.entries[w[0] as usize], &t.entries[w[1] as usize]);
                let o = match sort {
                    SortKey::Size => a.size.cmp(&b.size),
                    _ => a.modified.unwrap().cmp(&b.modified.unwrap()),
                }
                .then_with(|| crate::tab::natural_cmp(a.lname(), b.lname()));
                let o = if desc { o.reverse() } else { o };
                assert_ne!(o, std::cmp::Ordering::Greater, "{sort:?} desc {desc}: {:?} before {:?}", a.name, b.name);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn recents_on_top_sorts_recents_first_then_folders_then_names() {
    let d = tmpdir("recentsort");
    for n in ["b.txt", "a.txt", "c10.txt", "c2.txt", "z.txt"] {
        std::fs::write(d.join(n), b"x").unwrap();
    }
    for n in ["dir1", "dir2"] {
        std::fs::create_dir_all(d.join(n)).unwrap();
    }
    let stats = HashMap::new();
    let recents: HashSet<PathBuf> = [d.join("z.txt"), d.join("c10.txt"), d.join("dir2")].into_iter().collect();
    let mut t = Tab::new(61, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let ctx = ViewCtx { show_hidden: true, show_system: true, folders_first: true, recents_on_top: true, recents: &recents, stats: &stats };
    t.rebuild(&ctx);
    let names: Vec<&str> = t.view.iter().map(|&i| t.entries[i as usize].name.as_str()).collect();
    assert_eq!(names, ["dir2", "c10.txt", "z.txt", "dir1", "a.txt", "b.txt", "c2.txt"]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn md5_matches_rfc1321_and_the_thumbnail_spec_example() {
    use crate::thumbcache::md5_hex;
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(md5_hex(b"The quick brown fox jumps over the lazy dog"), "9e107d9d372bb6826bd81d3542a419d6");
    assert_eq!(md5_hex(&[b'a'; 1000]), "cabe45dcc9ae5b66ba86600cca6b8ba8");
    // freedesktop.org Thumbnail Managing Standard, "Thumbnail Saving" example.
    assert_eq!(md5_hex(b"file:///home/jens/photos/me.png"), "c6ee772d9e49320e97ec29a7eb5b1697");
}

#[test]
fn thumbnail_uris_encode_like_glib_and_round_trip() {
    use crate::thumbcache::{path_from_uri, uri_for};
    assert_eq!(uri_for(Path::new("/home/jens/photos/me.png")), "file:///home/jens/photos/me.png");
    assert_eq!(uri_for(Path::new("/tmp/a b/é#%.png")), "file:///tmp/a%20b/%C3%A9%23%25.png");
    for p in ["/tmp/a b/é#%.png", "/home/x/日本/file (1).txt", "/plain/path"] {
        assert_eq!(path_from_uri(&uri_for(Path::new(p))).as_deref(), Some(Path::new(p)));
    }
    assert_eq!(path_from_uri("file://localhost/etc/hosts").as_deref(), Some(Path::new("/etc/hosts")));
    assert_eq!(path_from_uri("https://example.com/x"), None);
}

#[test]
fn thumbnail_cache_round_trip_and_staleness() {
    use crate::thumbcache::{flavor, lookup_in, store_in};
    use std::os::unix::fs::PermissionsExt;
    let d = tmpdir("thumbcache");
    let root = d.join("thumbnails");
    let file = d.join("photo one.png");
    std::fs::write(&file, b"x").unwrap();
    let mtime = std::fs::metadata(&file).unwrap().modified().ok();
    let img = crate::imgdec::Rgba { w: 3, h: 2, px: (0..24).map(|i| i as u8 * 10).collect() };
    store_in(&root, &file, mtime, 128, &img, 1, Some([4000, 3000]));
    let (got, dims) = lookup_in(&root, &file, mtime, 100).expect("cached");
    assert_eq!((got.w, got.h, got.px), (3, 2, img.px.clone()));
    assert_eq!(dims, Some([4000, 3000]));
    // Stored where the spec says, private.
    let name = format!("{}.png", crate::thumbcache::md5_hex(crate::thumbcache::uri_for(&file).as_bytes()));
    let meta = std::fs::metadata(root.join("normal").join(&name)).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // Other flavours miss; a changed file is stale.
    assert!(lookup_in(&root, &file, mtime, 512).is_none());
    let later = mtime.map(|t| t + std::time::Duration::from_secs(5));
    assert!(lookup_in(&root, &file, later, 100).is_none(), "stale after the file changed");
    assert!(lookup_in(&root, &d.join("other.png"), mtime, 100).is_none());
    assert_eq!(flavor(64), ("normal", 128));
    assert_eq!(flavor(200), ("large", 256));
    assert_eq!(flavor(512), ("x-large", 512));
    assert_eq!(flavor(900), ("xx-large", 1024));
    let _ = std::fs::remove_dir_all(&d);
}

/// Read-only check against a real thumbnail cache made by other apps (run by hand:
/// `cargo test real_cache -- --ignored`): Maia's URI encoding and hashing must find
/// exactly the files they wrote.
#[test]
#[ignore]
fn real_thumbnail_cache_entries_match_our_encoding() {
    let root = crate::thumbcache::cache_root();
    let (mut checked, mut bad) = (0, vec![]);
    for flavor in ["normal", "large", "x-large", "xx-large"] {
        let Ok(rd) = std::fs::read_dir(root.join(flavor)) else { continue };
        for e in rd.flatten().take(400) {
            let Ok(bytes) = std::fs::read(e.path()) else { continue };
            let Some((_, text)) = crate::imgdec::png_with_text(&bytes) else { continue };
            let Some(uri) = text.iter().find(|(k, _)| k == "Thumb::URI").map(|(_, v)| v.clone()) else { continue };
            let Some(p) = crate::thumbcache::path_from_uri(&uri) else { continue };
            checked += 1;
            let ours = crate::thumbcache::uri_for(&p);
            let stem = e.path().file_stem().unwrap().to_string_lossy().into_owned();
            if ours != uri || crate::thumbcache::md5_hex(ours.as_bytes()) != stem {
                bad.push(format!("{uri} -> {ours}"));
            }
        }
    }
    eprintln!("real cache: {checked} thumbnails checked, {} mismatches", bad.len());
    assert!(bad.is_empty(), "{:?}", &bad[..bad.len().min(5)]);
}

#[test]
fn syntax_highlighting_tokens() {
    use crate::syntax::{highlight, lang_for, Kind};
    let spans = |name: &str, src: &[&str]| {
        let lines: Vec<String> = src.iter().map(|s| s.to_string()).collect();
        let hl = highlight(lang_for(name).expect(name), &lines, 1000);
        hl.into_iter().enumerate().flat_map(|(n, v)| v.into_iter().map(move |(a, b, k)| (n, src_owned(src, n, a, b), k))).collect::<Vec<_>>()
    };
    fn src_owned(src: &[&str], n: usize, a: usize, b: usize) -> String {
        src[n][a..b].to_string()
    }
    let rs = spans("main.rs", &["fn main() { let s = \"a \\\" b\"; // done", "/* multi", " line */ let x: u32 = 42;"]);
    assert!(rs.contains(&(0, "fn".into(), Kind::Keyword)));
    assert!(rs.contains(&(0, "\"a \\\" b\"".into(), Kind::Str)), "escaped quote stays inside the string: {rs:?}");
    assert!(rs.contains(&(0, "// done".into(), Kind::Comment)));
    assert!(rs.contains(&(1, "/* multi".into(), Kind::Comment)));
    assert!(rs.contains(&(2, " line */".into(), Kind::Comment)), "block comment carried to the next line");
    assert!(rs.contains(&(2, "u32".into(), Kind::Type)));
    assert!(rs.contains(&(2, "42".into(), Kind::Number)));
    let py = spans("x.py", &["def f():", "    s = \"\"\"doc", "    more\"\"\" # tail", "a#b = 1"]);
    assert!(py.contains(&(0, "def".into(), Kind::Keyword)));
    assert!(py.contains(&(1, "\"\"\"doc".into(), Kind::Str)));
    assert!(py.contains(&(2, "    more\"\"\"".into(), Kind::Str)), "triple-quoted string spans lines: {py:?}");
    assert!(py.contains(&(2, "# tail".into(), Kind::Comment)));
    assert!(!py.iter().any(|(n, _, k)| *n == 3 && *k == Kind::Comment), "a#b is not a comment");
    let js = spans("app.ts", &["const x = `tpl", "end` // c"]);
    assert!(js.contains(&(1, "end`".into(), Kind::Str)), "template literal spans lines");
    let html = spans("index.html", &["<div class=\"a\"><!-- note --></div>"]);
    assert!(html.contains(&(0, "<div".into(), Kind::Keyword)));
    assert!(html.contains(&(0, "\"a\"".into(), Kind::Str)));
    assert!(html.contains(&(0, "<!-- note -->".into(), Kind::Comment)));
    let md = spans("README.md", &["# Title", "text `code` more"]);
    assert!(md.contains(&(0, "# Title".into(), Kind::Keyword)));
    assert!(md.contains(&(1, "`code`".into(), Kind::Str)));
    assert!(lang_for("Makefile").is_some() && lang_for("photo.png").is_none() && lang_for("notes.txt").is_none());
}

#[test]
fn syntax_highlighting_never_panics_and_spans_stay_on_char_boundaries() {
    use crate::syntax::{highlight, lang_for};
    let pieces = ["fn", " ", "\"", "'", "`", "\\", "/*", "*/", "//", "#", "<!--", "-->", "\"\"\"", "日本", "é", "😀", "<a", "42", "x", "\t", "--", "--[[", "]]"];
    let mut x: u64 = 0xdead_beef;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let lines: Vec<String> = (0..3000).map(|_| (0..(next() % 14)).map(|_| pieces[(next() % pieces.len() as u64) as usize]).collect()).collect();
    for name in ["a.rs", "a.c", "a.ts", "a.py", "a.sh", "a.go", "a.java", "a.lua", "a.rb", "a.php", "a.sql", "a.zig", "a.json", "a.toml", "a.yaml", "a.css", "a.html", "a.md", "Makefile"] {
        let hl = highlight(lang_for(name).unwrap(), &lines, 10_000);
        for (line, spans) in lines.iter().zip(&hl) {
            for &(a, b, _) in spans {
                assert!(a < b && b <= line.len() && line.is_char_boundary(a) && line.is_char_boundary(b), "{name}: bad span {a}..{b} in {line:?}");
            }
        }
    }
}

#[test]
fn ffprobe_output_is_read_like_a_player_needs() {
    let flat = b"streams.stream.0.codec_name=\"mp3\"\nstreams.stream.0.codec_type=\"audio\"\nstreams.stream.0.disposition.attached_pic=0\nstreams.stream.1.codec_name=\"png\"\nstreams.stream.1.codec_type=\"video\"\nstreams.stream.1.width=200\nstreams.stream.1.height=200\nstreams.stream.1.disposition.attached_pic=1\nformat.duration=\"2.035000\"\nformat.tags.title=\"Test \\\"Tone\\\"\"\nformat.tags.ARTIST=\"Maia\"\n";
    let p = crate::media::parse_probe(flat).unwrap();
    assert!(p.video.is_none(), "cover art is not a video stream");
    assert!(p.audio && (p.duration - 2.035).abs() < 1e-6);
    assert_eq!(p.title.as_deref(), Some("Test \"Tone\""), "escaped quotes unescaped");
    assert_eq!(p.artist.as_deref(), Some("Maia"), "tag keys in any case");
    let v = b"streams.stream.0.codec_name=\"h264\"\nstreams.stream.0.codec_type=\"video\"\nstreams.stream.0.width=1920\nstreams.stream.0.height=1080\nstreams.stream.0.disposition.attached_pic=0\nformat.duration=\"61.5\"\n";
    let p = crate::media::parse_probe(v).unwrap();
    assert_eq!(p.video, Some((1920, 1080)));
    assert!(!p.audio);
    assert!(crate::media::parse_probe(b"format.duration=\"1\"\n").is_none());
    assert_eq!(crate::media::fmt_time(61.5), "1:01");
    assert_eq!(crate::media::fmt_time(3725.0), "1:02:05");
}

#[test]
fn lsblk_pairs_parse_into_drives_with_safe_removability() {
    use crate::drives::parse_lsblk;
    // A real machine: system disk (reports HOTPLUG=1) and a Windows NVMe, plus a USB stick.
    let text = r#"PATH="/dev/sda" PKNAME="" LABEL="" FSTYPE="" SIZE="500107862016" RM="0" HOTPLUG="1" TYPE="disk" MOUNTPOINTS=""
PATH="/dev/sda1" PKNAME="sda" LABEL="" FSTYPE="vfat" SIZE="4294967296" RM="0" HOTPLUG="1" TYPE="part" MOUNTPOINTS="/boot"
PATH="/dev/sda2" PKNAME="sda" LABEL="" FSTYPE="btrfs" SIZE="495810760704" RM="0" HOTPLUG="1" TYPE="part" MOUNTPOINTS="/var/tmp\x0a/home\x0a/"
PATH="/dev/zram0" PKNAME="" LABEL="zram0" FSTYPE="swap" SIZE="33488371712" RM="0" HOTPLUG="0" TYPE="disk" MOUNTPOINTS="[SWAP]"
PATH="/dev/nvme0n1p1" PKNAME="nvme0n1" LABEL="" FSTYPE="vfat" SIZE="104857600" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/nvme0n1p2" PKNAME="nvme0n1" LABEL="" FSTYPE="" SIZE="16777216" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/nvme0n1p3" PKNAME="nvme0n1" LABEL="" FSTYPE="ntfs" SIZE="680705130496" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/nvme0n1p5" PKNAME="nvme0n1" LABEL="New Volume" FSTYPE="ntfs" SIZE="336961994752" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/sdb" PKNAME="" LABEL="" FSTYPE="" SIZE="31004295168" RM="1" HOTPLUG="1" TYPE="disk" MOUNTPOINTS=""
PATH="/dev/sdb1" PKNAME="sdb" LABEL="My \x22USB\x22 日本" FSTYPE="exfat" SIZE="31003246592" RM="1" HOTPLUG="1" TYPE="part" MOUNTPOINTS="/run/media/me/My \x22USB\x22 日本""#;
    let d = parse_lsblk(text);
    let get = |dev: &str| d.iter().find(|x| x.dev == dev).unwrap_or_else(|| panic!("{dev}"));
    let root = get("/dev/sda2");
    assert_eq!(root.mounts, vec![PathBuf::from("/var/tmp"), PathBuf::from("/home"), PathBuf::from("/")]);
    assert!(!root.removable && !get("/dev/sda1").removable, "the system disk is never ejectable, HOTPLUG or not");
    let usb = get("/dev/sdb1");
    assert!(usb.removable && usb.mounted());
    assert_eq!(usb.label, "My \"USB\" 日本");
    assert_eq!(usb.disk, "/dev/sdb");
    assert_eq!(usb.mounts, vec![PathBuf::from("/run/media/me/My \"USB\" 日本")]);
    // What gets offered for mounting: the big Windows partitions, not EFI/MSR/swap.
    let offered: Vec<&str> = d.iter().filter(|x| x.offer_mount()).map(|x| x.dev.as_str()).collect();
    assert_eq!(offered, ["/dev/nvme0n1p3", "/dev/nvme0n1p5"]);
    assert_eq!(get("/dev/nvme0n1p5").title(), "New Volume");
    assert!(get("/dev/nvme0n1p3").title().ends_with(" Volume"));
    // Garbage and partial lines don't panic.
    assert!(parse_lsblk("PATH=\"/dev/x\\x\" LABEL=\"\\xZZ é \\x\"\nnonsense\nPATH=").len() <= 1);
}

#[test]
fn stick_mounted_inside_home_stays_ejectable() {
    use crate::drives::parse_lsblk;
    let text = "PATH=\"/dev/sdc\" PKNAME=\"\" LABEL=\"\" FSTYPE=\"\" SIZE=\"8000000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"disk\" MOUNTPOINTS=\"\"\n\
PATH=\"/dev/sdc1\" PKNAME=\"sdc\" LABEL=\"S\" FSTYPE=\"vfat\" SIZE=\"7999000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"part\" MOUNTPOINTS=\"/home/me/usb\"\n\
PATH=\"/dev/sdd\" PKNAME=\"\" LABEL=\"\" FSTYPE=\"\" SIZE=\"8000000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"disk\" MOUNTPOINTS=\"\"\n\
PATH=\"/dev/sdd1\" PKNAME=\"sdd\" LABEL=\"\" FSTYPE=\"ext4\" SIZE=\"7999000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"part\" MOUNTPOINTS=\"/home\"\n";
    let d = parse_lsblk(text);
    assert!(d.iter().find(|x| x.dev == "/dev/sdc1").unwrap().removable, "a folder inside /home is just a mount point");
    assert!(!d.iter().find(|x| x.dev == "/dev/sdd1").unwrap().removable, "but a disk holding /home itself is the system's");
    assert!(crate::drives::can_unmount(d.iter().find(|x| x.dev == "/dev/sdc1").unwrap()));
    assert!(!crate::drives::can_unmount(d.iter().find(|x| x.dev == "/dev/sdd1").unwrap()));
}

#[test]
fn uri_lists_round_trip_any_file_name() {
    use crate::clipboard::{parse_uri_list, path_to_uri, uri_list};
    let paths: Vec<PathBuf> = ["/home/me/a b.txt", "/tmp/résumé – v3 (final).pdf", "/x/100% #1?.png", "/日本/ファイル", "/q/'quote\"s'", "/p/a+b=c&d"].iter().map(PathBuf::from).collect();
    let body = uri_list(&paths);
    assert!(body.lines().all(|l| l.starts_with("file:///") && !l.contains(' ')), "{body}");
    assert!(body.ends_with("\r\n"));
    assert_eq!(parse_uri_list(&body), paths);
    assert_eq!(path_to_uri(std::path::Path::new("/a b")), "file:///a%20b");
    // What other apps send: comments, LF or CRLF, localhost, other schemes.
    let foreign = "# copied by an app\nfile:///home/me/x.txt\r\nfile://localhost/home/me/y%20z.png\nhttps://example.com/pic.jpg\n\n  file:///tmp/t  \n";
    assert_eq!(parse_uri_list(foreign), ["/home/me/x.txt", "/home/me/y z.png", "/tmp/t"].iter().map(PathBuf::from).collect::<Vec<_>>());
    // Not valid UTF-8 on disk: still the same bytes back.
    use std::os::unix::ffi::OsStrExt;
    let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/odd/\xff\xfe.bin"));
    assert_eq!(parse_uri_list(&uri_list(std::slice::from_ref(&odd))), vec![odd]);
    assert!(parse_uri_list("file:///bad%zz%").len() == 1, "malformed escapes are kept, not panicked on");
}

#[test]
fn drag_picture_is_a_clean_premultiplied_card() {
    let ctx = egui::Context::default();
    let cfg = crate::config::Config::default();
    crate::theme::install_fonts(&ctx, &cfg.font_family, &cfg.inspector_font);
    let mut raw = egui::RawInput::default();
    raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(1.6);
    let _ = ctx.run(raw, |_| {});
    let pal = crate::theme::palette(crate::theme::ThemeName::default());
    let dir = std::env::temp_dir();
    let long = PathBuf::from("/x/a very long file name that keeps going and going well past what fits.txt");
    let cases: Vec<(&str, Vec<PathBuf>)> = vec![("one", vec![PathBuf::from("/x/photo from chat.png")]), ("many", vec![PathBuf::from("/x/a"), PathBuf::from("/x/b"), PathBuf::from("/x/c")]), ("folder", vec![dir.clone()]), ("long", vec![long])];
    for (name, paths) in cases {
        let pic = crate::ui::drag_picture(&ctx, &paths, &pal);
        let (w, h, px) = (pic.w, pic.h, &pic.px);
        assert_eq!(pic.scale, 2, "1.6 rounds up to a 2x picture");
        assert_eq!(h, 60);
        assert_eq!(px.len(), w * h * 4);
        assert!(px.chunks(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]), "premultiplied");
        let at = |x: usize, y: usize| &px[(y * w + x) * 4..(y * w + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "rounded corner is transparent");
        assert!(at(w / 2, h - 4)[3] > 230, "card is nearly opaque");
        let text = (70..w).flat_map(|x| (0..h).map(move |y| (x, y))).filter(|&(x, y)| at(x, y)[1] > 150).count();
        assert!(text > 60, "{name}: the label is drawn ({text} bright pixels)");
        assert!(w <= ((10.0 + 16.0 + 8.0 + 241.0 + 12.0) * 2.0) as usize + 2, "{name}: long names are cut ({w})");
        if let Some(d) = std::env::var_os("COMET_ICON_DUMP") {
            let mut rgba = px.clone();
            for p in rgba.chunks_mut(4) {
                p.swap(0, 2);
            }
            let f = std::fs::File::create(std::path::Path::new(&d).join(format!("drag-{name}.png"))).unwrap();
            let mut e = png::Encoder::new(f, w as u32, h as u32);
            e.set_color(png::ColorType::Rgba);
            e.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
    }
}

#[test]
fn window_icon_is_the_launcher_icon() {
    let (px, w, h) = crate::install::window_icon();
    assert_eq!((w, h), (64, 64));
    let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize];
    assert_eq!(at(0, 0)[3], 0, "rounded corner");
    assert!(at(32, 60)[3] == 255 && at(32, 60)[0] < 40, "dark tile");
    // The star's centre (82, 46 of 128) is white, and its glow is blue.
    let c = at(41, 23);
    assert!(c[0] > 240 && c[1] > 240 && c[2] > 240, "star {c:?}");
    let g = at(41, 31);
    assert!(g[2] > g[0], "blue glow under the star {g:?}");
    if let Some(d) = std::env::var_os("COMET_ICON_DUMP") {
        let f = std::fs::File::create(std::path::Path::new(&d).join("window-icon.png")).unwrap();
        let mut e = png::Encoder::new(f, w, h);
        e.set_color(png::ColorType::Rgba);
        e.write_header().unwrap().write_image_data(&px).unwrap();
    }
}

#[test]
fn finished_jobs_read_in_the_past_tense() {
    use crate::ui::finished_label;
    assert_eq!(finished_label("Copying 1 item"), "Copied 1 item");
    assert_eq!(finished_label("Moving 3 items"), "Moved 3 items");
    assert_eq!(finished_label("Moving to trash 2 items"), "Moved to trash 2 items");
    assert_eq!(finished_label("Deleting 1 item"), "Deleted 1 item");
    assert_eq!(crate::ui::failed_label("Copying 1 item"), "Couldn't copy 1 item");
    assert_eq!(crate::ui::failed_label("Moving to trash 2 items"), "Couldn't move to trash 2 items");
    assert_eq!(crate::ui::failed_label("Deleting 3 items as administrator"), "Couldn't delete 3 items as administrator");
}

/// Run a tab's contents search to the end and rebuild its view.
fn pump_contents(t: &mut Tab, hidden: bool) -> Vec<String> {
    let stats = HashMap::new();
    let recents = HashSet::new();
    let ctx = ViewCtx { show_hidden: hidden, show_system: false, folders_first: true, recents_on_top: false, recents: &recents, stats: &stats };
    let start = Instant::now();
    loop {
        crate::contentsearch::pump(t, hidden);
        t.poll();
        if !t.is_loading() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "contents search finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    crate::contentsearch::pump(t, hidden);
    t.rebuild(&ctx);
    let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
    v.sort();
    v
}

#[test]
fn contents_search_finds_text_and_skips_binary_big_and_hidden() {
    let d = tmpdir("contents");
    touch(&d.join("a.txt"), "Hello World\n\tsecond   line has Needle here\nthird needle\n");
    touch(&d.join("b.md"), "nothing to see");
    touch(&d.join("needle-in-name.txt"), "the name matches, the text does not");
    std::fs::write(d.join("c.bin"), b"\x7fELF\x00\x00 needle").unwrap();
    touch(&d.join(".hidden.txt"), "a hidden needle");
    touch(&d.join("sub/deep.txt"), "NEEDLE at depth");
    touch(&d.join("long.txt"), &format!("{}needle{}\n", "x".repeat(500), "y".repeat(500)));
    touch(&d.join("grüße.txt"), "Viel ÄRGER heute\n");
    // A match across the 64 KB read boundary is still found.
    let mut seam = vec![b'.'; (64 << 10) - 3];
    seam.extend_from_slice(b"needle\n");
    std::fs::write(d.join("seam.txt"), seam).unwrap();
    // Over the size cap (sparse, so it costs no disk): skipped although it matches.
    let big = std::fs::File::create(d.join("big.log")).unwrap();
    std::io::Write::write_all(&mut &big, b"needle\n").unwrap();
    big.set_len(crate::contentsearch::MAX_BYTES + 1).unwrap();
    drop(big);

    let mut t = Tab::new(40, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &HashMap::new());
    t.content.on = true;
    t.filter = "needle".into();
    assert_eq!(pump_contents(&mut t, false), vec!["a.txt", "long.txt", "seam.txt"]);
    assert_eq!(t.content.snippet(&d.join("a.txt")), Some("second line has Needle here"));
    let long = t.content.snippet(&d.join("long.txt")).unwrap_or_default().to_string();
    assert!(long.starts_with('…') && long.contains("needle") && long.chars().count() < 140, "{long}");
    // Hidden files once they are shown; the subfolder only when flattened.
    assert_eq!(pump_contents(&mut t, true), vec![".hidden.txt", "a.txt", "long.txt", "seam.txt"]);
    t.set_flatten(true);
    assert_eq!(pump_contents(&mut t, false), vec!["a.txt", "deep.txt", "long.txt", "seam.txt"]);
    t.set_flatten(false);
    pump(&mut t, &HashMap::new());
    // Case-insensitive outside ASCII too.
    t.filter = "ärger".into();
    assert_eq!(pump_contents(&mut t, false), vec!["grüße.txt"]);
    assert_eq!(t.content.snippet(&d.join("grüße.txt")), Some("Viel ÄRGER heute"));
    // Back to names: the same query matches the file name, not the text.
    t.filter = "needle".into();
    t.content.on = false;
    assert_eq!(pump_contents(&mut t, false), vec!["needle-in-name.txt"]);
    assert!(t.content.take().is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn contents_search_never_opens_a_fifo_and_stops_when_cancelled() {
    let d = tmpdir("contents_fifo");
    let fifo = std::ffi::CString::new(d.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    touch(&d.join("x.txt"), "needle");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut buf = vec![0u8; 64 << 10];
    let t = Instant::now();
    assert_eq!(crate::contentsearch::find(&d.join("pipe"), "needle", &mut buf, &stop), None);
    assert!(t.elapsed() < Duration::from_secs(1), "the FIFO was not waited on");
    assert_eq!(crate::contentsearch::find(&d.join("x.txt"), "needle", &mut buf, &stop).as_deref(), Some("needle"));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(crate::contentsearch::find(&d.join("x.txt"), "needle", &mut buf, &stop), None, "a cancelled search reads nothing");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn second_launch_hands_its_folders_to_the_first() {
    use crate::single::{Request, claim_at, forward_to, parse, serve};
    let d = tmpdir("single");
    let sock = d.join("maia.sock");
    assert!(!forward_to(&sock, &[PathBuf::from("/x")]), "nobody listening: start normally");
    let l = claim_at(&sock).expect("first launch listens");
    assert!(claim_at(&sock).is_none(), "a second one does not take over");
    let (tx, rx) = std::sync::mpsc::channel();
    serve(l, tx, || {});
    let odd = PathBuf::from("/tmp/two\nlines – é/日本 #1");
    assert!(forward_to(&sock, &[PathBuf::from("/home"), odd.clone()]), "handed over");
    assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), Request::Open(vec![PathBuf::from("/home"), odd]));
    assert!(forward_to(&sock, &[]));
    assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), Request::Focus);
    // A socket left by a crash (nobody answering) is replaced.
    let stale = d.join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert!(stale.exists() && claim_at(&stale).is_some(), "stale socket replaced");
    // Garbage never panics.
    assert_eq!(parse(b"open"), None);
    assert_eq!(parse(b"\0\0\0"), None);
    assert_eq!(parse(b"what\0ever\0"), None);
}

#[test]
fn default_file_manager_is_read_from_mimeapps() {
    use crate::install::default_in;
    let t = "[Added Associations]\ninode/directory=other.desktop;\n\n[Default Applications]\ntext/plain=gedit.desktop\ninode/directory=io.github.yappologistic.Maia.desktop;org.gnome.Nautilus.desktop;\n";
    assert_eq!(default_in(t).as_deref(), Some("io.github.yappologistic.Maia.desktop"));
    assert_eq!(default_in("[Default Applications]\ninode/directory= ;thunar.desktop\n").as_deref(), Some("thunar.desktop"));
    assert_eq!(default_in("[Added Associations]\ninode/directory=x.desktop\n"), None, "only Default Applications counts");
    assert_eq!(default_in(""), None);
}

#[test]
fn archive_listings_parse_like_bsdtar_prints_them() {
    use crate::archive::parse_list;
    let now = std::time::SystemTime::now();
    // Captured from bsdtar 3.8: a zip (entries stored as "./name"), a tar.gz with a
    // symlink, and a 7z that lists a folder after its file.
    let zip = "drwxr-xr-x  0 1000   1000        0 Oct 14 22:57 ./\n-rw-r--r--  0 1000   1000        0 Mar  5  2020 ./old.txt\nlrwxrwxrwx  0 1000   1000        0 Oct 14 22:57 ./link -> top.txt\n-rw-r--r--  0 1000   1000        2 Oct 14 22:57 ./top.txt\ndrwxr-xr-x  0 1000   1000        0 Oct 14 22:57 ./docs dir/\n-rw-r--r--  0 1000   1000        6 Oct 14 22:57 ./docs dir/read me.md\n";
    let items = parse_list(zip, now);
    let names: Vec<String> = items.iter().map(|i| i.path.display().to_string()).collect();
    assert_eq!(names, ["old.txt", "link", "top.txt", "docs dir", "docs dir/read me.md"], "./ stripped, the root entry dropped");
    assert!(items.iter().all(|i| i.dot), "stored with ./");
    let link = &items[1];
    assert!(link.link && !link.is_dir, "symlink without its target");
    assert!(items[3].is_dir && items[4].size == 6);
    // "Mar  5  2020" is that day; "Oct 14 22:57" is this year (or the last).
    let year_of = |t: std::time::SystemTime| crate::timefmt::format(t, "%Y");
    assert_eq!(year_of(items[0].modified.unwrap()), "2020");
    assert!(items[2].modified.unwrap() <= now + std::time::Duration::from_secs(86_400));
    // Folders that are only prefixes get rows; "../" escapes are refused.
    let bare = "-rw-r--r--  0 0      0           2 Oct 14 22:57 a/b/c.txt\n-rw-r--r--  0 0 0 1 Oct 14 22:57 ../evil\n";
    let items = parse_list(bare, now);
    let mut names: Vec<String> = items.iter().map(|i| format!("{}{}", i.path.display(), if i.is_dir { "/" } else { "" })).collect();
    names.sort();
    assert_eq!(names, ["a/", "a/b/", "a/b/c.txt"]);
    // bsdtar's escapes for odd names, and garbage lines.
    let odd = "-rw-r--r--  0 0      0           2 Oct 14 22:57 back\\\\slash \\303\\251.txt\nnonsense\n\n-rw-r--r-- 0\n";
    assert_eq!(parse_list(odd, now).iter().map(|i| i.path.display().to_string()).collect::<Vec<_>>(), ["back\\slash é.txt"]);
    assert_eq!(crate::archive::stem("photos.tar.gz"), "photos");
    assert_eq!(crate::archive::stem("Doc.ZIP"), "Doc");
    assert!(crate::archive::is_archive("x.tar.zst") && !crate::archive::is_archive(".zip") && !crate::archive::is_archive("notes.txt"));
}

#[test]
fn archives_round_trip_through_bsdtar() {
    if !crate::archive::available() {
        eprintln!("bsdtar missing: skipped");
        return;
    }
    let d = tmpdir("archive");
    let src = d.join("src");
    std::fs::create_dir_all(src.join("Trip photos")).unwrap();
    std::fs::write(src.join("Trip photos/beach – é.txt"), "sand").unwrap();
    std::fs::write(src.join("notes.txt"), "hello").unwrap();
    let names = vec![std::ffi::OsString::from("Trip photos"), std::ffi::OsString::from("notes.txt")];
    for ext in ["zip", "tar.gz", "tar.xz", "7z"] {
        let out = d.join(format!("pack.{ext}"));
        crate::archive::create(&out, &src, &names).unwrap_or_else(|e| panic!("{ext}: {e}"));
        assert!(out.is_file() && !d.join(format!(".pack.{ext}.part")).exists(), "{ext}: written in one go");
        let top: Vec<String> = crate::archive::entries(&out, std::path::Path::new("")).unwrap().iter().map(|e| e.name.clone()).collect();
        assert!(top.contains(&"Trip photos".into()) && top.contains(&"notes.txt".into()), "{ext}: {top:?}");
        let inner = crate::archive::entries(&out, std::path::Path::new("Trip photos")).unwrap();
        assert_eq!(inner.len(), 1, "{ext}");
        assert_eq!(inner[0].path, out.join("Trip photos/beach – é.txt"), "{ext}: rows continue the archive's path");
        assert!(crate::archive::entries(&out, std::path::Path::new("nope")).is_err());
        let x = d.join(format!("x-{ext}"));
        crate::archive::extract(&out, &[std::path::PathBuf::from("Trip photos")], &x).unwrap();
        assert_eq!(std::fs::read_to_string(x.join("Trip photos/beach – é.txt")).unwrap(), "sand", "{ext}");
        assert!(!x.join("notes.txt").exists(), "{ext}: only what was asked");
        let (a, i) = crate::archive::split(&out.join("Trip photos/beach – é.txt")).unwrap();
        assert_eq!((a, i), (out.clone(), std::path::PathBuf::from("Trip photos/beach – é.txt")));
    }
    assert!(crate::archive::split(&src.join("notes.txt")).is_none());
    assert!(crate::archive::create(&d.join("bad.zip"), &src, &[std::ffi::OsString::from("missing")]).is_err(), "errors come back");
    assert!(!d.join("bad.zip").exists() && !d.join(".bad.zip.part").exists(), "nothing left behind");
}

#[test]
fn merge_sort_is_stable_and_matches_sort_by() {
    let mut x: u64 = 0x2545f4914f6cdd1d;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for n in [0usize, 1, 2, 23, 24, 25, 47, 48, 49, 100, 1000, 4097, 50_000] {
        for shape in 0..4 {
            // (key, original position): few distinct keys, so stability shows.
            let v: Vec<(u32, u32)> = (0..n as u32)
                .map(|i| match shape {
                    0 => ((rnd() % 7) as u32, i),
                    1 => (i, i),
                    2 => (n as u32 - i, i),
                    _ => ((i / 3) as u32, i),
                })
                .collect();
            let mut a = v.clone();
            let mut b = v.clone();
            crate::tab::merge_sort(&mut a, &|p: &(u32, u32), q: &(u32, u32)| p.0.cmp(&q.0));
            b.sort_by(|p, q| p.0.cmp(&q.0));
            assert_eq!(a, b, "n {n} shape {shape}");
        }
    }
}

/// Run git in `dir` (as a fixed test identity), failing the test if it fails.
pub(crate) fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok || args.first() == Some(&"merge"), "git {args:?}");
}

/// A work tree with one change of every kind: a merge conflict, a modification, a staged
/// new file, an untracked file, a staged deletion, a rename and a change two folders down.
pub(crate) fn git_fixture(d: &Path) {
    touch(&d.join("keep.txt"), "same\n");
    touch(&d.join("mod.txt"), "one\n");
    touch(&d.join("old.txt"), "to be renamed\n");
    touch(&d.join("gone.txt"), "to be removed\n");
    touch(&d.join("conflict.txt"), "base\n");
    touch(&d.join("sub/inner/deep.txt"), "deep\n");
    git(d, &["init", "-q"]);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    git(d, &["checkout", "-qb", "other"]);
    touch(&d.join("conflict.txt"), "theirs\n");
    git(d, &["commit", "-qam", "theirs"]);
    git(d, &["checkout", "-q", "main"]);
    touch(&d.join("conflict.txt"), "ours\n");
    git(d, &["commit", "-qam", "ours"]);
    git(d, &["merge", "-q", "other"]);
    touch(&d.join("mod.txt"), "two\n");
    touch(&d.join("staged.txt"), "new\n");
    git(d, &["add", "staged.txt"]);
    touch(&d.join("new.txt"), "untracked\n");
    git(d, &["rm", "-q", "gone.txt"]);
    git(d, &["mv", "old.txt", "renamed.txt"]);
    touch(&d.join("sub/inner/deep.txt"), "deeper\n");
}

#[test]
fn git_status_marks_every_kind_of_change() {
    use crate::gitstatus::{Mark, query};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("git");
    git_fixture(&d);
    let r = query(&d.join("sub")).expect("a work tree");
    assert_eq!(r.root, d);
    assert_eq!(r.branch, "main");
    let m = |p: &str| r.marks.get(&d.join(p)).copied();
    assert_eq!(m("conflict.txt"), Some(Mark::Conflict));
    assert_eq!(m("mod.txt"), Some(Mark::Modified));
    assert_eq!(m("staged.txt"), Some(Mark::Staged));
    assert_eq!(m("new.txt"), Some(Mark::Untracked));
    assert_eq!(m("gone.txt"), Some(Mark::Deleted));
    assert_eq!(m("renamed.txt"), Some(Mark::Renamed));
    assert_eq!(m("old.txt"), None, "a rename's source is not marked");
    assert_eq!(m("sub/inner/deep.txt"), Some(Mark::Modified));
    assert_eq!(m("sub"), Some(Mark::Inside));
    assert_eq!(m("sub/inner"), Some(Mark::Inside));
    assert_eq!(m("keep.txt"), None);
    // Reached through a symlink, marks use the path as the user sees it.
    let link = std::env::temp_dir().join(format!("maia-test-gitlink-{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&d, &link).unwrap();
    let r = query(&link.join("sub/inner")).expect("a work tree through a link");
    assert_eq!(r.root, link);
    assert_eq!(r.marks.get(&link.join("mod.txt")), Some(&Mark::Modified));
    let _ = std::fs::remove_file(&link);
    // Outside a work tree, and inside .git itself, there is nothing to mark.
    let plain = tmpdir("nogit");
    assert!(query(&plain).is_none());
    assert!(query(&d.join(".git")).is_none());
    // A fresh repository with no commits yet still names its branch.
    git(&plain, &["init", "-q"]);
    touch(&plain.join("a.txt"), "a");
    let r = query(&plain).expect("an empty repository");
    assert_eq!((r.branch.as_str(), r.marks.get(&plain.join("a.txt"))), ("main", Some(&Mark::Untracked)));
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&plain);
}

/// Two folders with one difference of every kind (the compare journey uses them too):
/// a newer file, a file of another size, files and folders on one side only, a name that
/// is a file here and a folder there, and identical entries.
pub(crate) fn compare_fixture(a: &Path, b: &Path) {
    let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let file = |p: PathBuf, body: &str, when: std::time::SystemTime| {
        touch(&p, body);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(when).unwrap();
    };
    for d in [a, b] {
        file(d.join("same.txt"), "same", t0);
        std::fs::create_dir_all(d.join("both")).unwrap();
    }
    file(a.join("newer.txt"), "new", t0 + Duration::from_secs(3600));
    file(b.join("newer.txt"), "old", t0);
    file(a.join("size.txt"), "longer text", t0);
    file(b.join("size.txt"), "short", t0);
    file(a.join("only-a.txt"), "a", t0);
    file(b.join("only-b.txt"), "b", t0);
    std::fs::create_dir_all(a.join("folder-a")).unwrap();
    file(a.join("kind"), "a file", t0);
    std::fs::create_dir_all(b.join("kind")).unwrap();
}

#[test]
fn compare_marks_only_newer_and_size_differences() {
    use crate::compare::{Diff, side};
    let d = tmpdir("compare");
    let (a, b) = (d.join("a"), d.join("b"));
    compare_fixture(&a, &b);
    let (la, lb) = (crate::fsx::list_dir(&a).unwrap(), crate::fsx::list_dir(&b).unwrap());
    let (ea, eb): (Vec<_>, Vec<_>) = (la.iter().collect(), lb.iter().collect());
    let mut m = HashMap::new();
    side(&ea, &eb, &mut m);
    side(&eb, &ea, &mut m);
    let want: HashMap<PathBuf, Diff> = [
        (a.join("newer.txt"), Diff::Newer),
        (a.join("size.txt"), Diff::Size),
        (b.join("size.txt"), Diff::Size),
        (a.join("only-a.txt"), Diff::Only),
        (b.join("only-b.txt"), Diff::Only),
        (a.join("folder-a"), Diff::Only),
        (a.join("kind"), Diff::Only),
        (b.join("kind"), Diff::Only),
    ]
    .into_iter()
    .collect();
    assert_eq!(m, want, "identical files, the older side and folders on both sides stay unmarked");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn aliases_set_change_clear_and_refuse_duplicates() {
    use crate::popups::set_alias;
    let mut ud = crate::config::UserData::default();
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, "  hh "), Ok(()));
    assert_eq!(ud.aliases.get("toggle_hidden").map(String::as_str), Some("hh"), "trimmed and stored by command id");
    assert_eq!(set_alias(&mut ud, Cmd::ToggleSidebar, "HH"), Err(Cmd::ToggleHidden), "another command's alias, in any case, is refused");
    assert!(!ud.aliases.contains_key("toggle_sidebar"));
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, "hid"), Ok(()));
    assert_eq!(set_alias(&mut ud, Cmd::ToggleSidebar, "hh"), Ok(()), "a changed alias frees the old one");
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, "hid"), Ok(()), "setting the same alias again is fine");
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, ""), Ok(()));
    assert!(!ud.aliases.contains_key("toggle_hidden"), "empty text clears it");
    // An alias left behind by a command that no longer exists blocks nothing.
    ud.aliases.insert("gone_command".into(), "zz".into());
    assert_eq!(set_alias(&mut ud, Cmd::GoTo, "zz"), Ok(()));
}

#[test]
fn network_places_parse_from_gio() {
    use crate::network::{is_uri, label, normalize, parse_mounts};
    let text = "Drive(0): Micron MTFDKBA1T0TFH\n  Type: GProxyDrive (GProxyVolumeMonitorUDisks2)\n  Volume(0): 681 GB Volume\n    Type: GProxyVolume (GProxyVolumeMonitorUDisks2)\n    can_mount=1\nVolume(0): Pixel 7\n  Type: GProxyVolume (GProxyVolumeMonitorMTP)\n  activation_root=mtp://Google_Pixel_7_2A09/\n  can_mount=1\nVolume(1): Camera\n  Type: GProxyVolume (GProxyVolumeMonitorGPhoto2)\n  activation_root=gphoto2://Canon_EOS/\n  Mount(0): Camera -> gphoto2://Canon_EOS/\n    Type: GProxyShadowMount (GProxyVolumeMonitorGPhoto2)\nMount(0): media on nas -> smb://nas/media/\n  Type: GDaemonMount\n  default_location=smb://nas/media/\nMount(1): me on host -> sftp://me@host/\n  Type: GDaemonMount\nMount(2): Backup -> file:///run/media/me/Backup\n  Type: GProxyMount (GProxyVolumeMonitorUDisks2)\n";
    let p = parse_mounts(text);
    let got: Vec<(String, bool, bool)> = p.iter().map(|x| (x.uri.clone(), x.device, x.mounted)).collect();
    assert_eq!(
        got,
        [("mtp://Google_Pixel_7_2A09/".into(), true, false), ("gphoto2://Canon_EOS/".into(), true, true), ("smb://nas/media/".into(), false, true), ("sftp://me@host/".into(), false, true)],
        "local disks left to Drives, phones and shares kept"
    );
    assert_eq!(p[0].name, "Pixel 7");
    assert!(is_uri("smb://nas/media") && is_uri("SFTP://me@host") && !is_uri("/home/me") && !is_uri("smb://") && !is_uri("http://x"));
    assert_eq!(normalize(" ssh://me@host/srv "), "sftp://me@host/srv");
    assert_eq!(label("smb://nas/media/"), "media on nas");
    assert_eq!(label("sftp://me@host/"), "me@host");
    assert!(parse_mounts("garbage\nMount(\nVolume(3)\n").is_empty());
}

#[test]
fn administrator_commands_are_exact() {
    use crate::admin::{Task, denied};
    let s = |v: Vec<std::ffi::OsString>| v.into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
    let a = s(Task::Copy { srcs: vec!["/home/me/a b".into(), "/home/me/-x".into()], dest: "/etc".into() }.argv());
    assert!(a[0].ends_with("/cp"), "absolute program: {}", a[0]);
    assert_eq!(&a[1..], ["-a", "--", "/home/me/a b", "/home/me/-x", "/etc"], "-- before names that look like options");
    assert_eq!(&s(Task::Delete { paths: vec!["/srv/x".into()] }.argv())[1..], ["-rf", "--", "/srv/x"]);
    assert_eq!(&s(Task::Rename { from: "/etc/a".into(), to: "/etc/b".into() }.argv())[1..], ["-T", "--", "/etc/a", "/etc/b"]);
    assert!(s(Task::NewFolder("/opt/n".into()).argv())[0].ends_with("/mkdir"));
    assert!(denied(&std::io::Error::from_raw_os_error(libc::EACCES)) && denied(&std::io::Error::from_raw_os_error(libc::EPERM)));
    assert!(!denied(&std::io::Error::from_raw_os_error(libc::EROFS)), "a read-only disk isn't a permission problem");
}

#[test]
fn hand_written_png_is_valid_and_compact() {
    // A photo-like 256 x 192 image: smooth gradients with some noise.
    let (w, h) = (256u32, 192u32);
    let mut x: u32 = 7;
    let px: Vec<u8> = (0..w * h).flat_map(|i| {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        let (cx, cy) = (i % w, i / w);
        [(cx / 2) as u8, (cy + (x >> 29)) as u8, ((cx + cy) / 3) as u8, 255]
    }).collect();
    let img = crate::imgdec::Rgba { w, h, px };
    let ours = crate::thumbcache::encode_png(&img, &[("Thumb::URI", "file:///x/a%20b.jpg".into()), ("Software", "Maia".into())]).unwrap();
    let (back, text) = crate::imgdec::png_with_text(&ours).expect("decodes");
    assert_eq!((back.w, back.h), (w, h));
    assert_eq!(back.px, img.px, "lossless");
    assert!(text.iter().any(|(k, v)| k == "Thumb::URI" && v == "file:///x/a%20b.jpg"));
    // Within reach of png's own default encoder (which is what other apps write).
    let mut theirs = vec![];
    {
        let mut e = png::Encoder::new(&mut theirs, w, h);
        e.set_color(png::ColorType::Rgba);
        e.write_header().unwrap().write_image_data(&img.px).unwrap();
    }
    assert!(ours.len() < theirs.len() * 2, "ours {} vs png {}", ours.len(), theirs.len());
    eprintln!("png sizes: ours {} png crate {}", ours.len(), theirs.len());
    if let Some(d) = std::env::var_os("COMET_ICON_DUMP") {
        std::fs::write(std::path::Path::new(&d).join("hand.png"), &ours).unwrap();
    }
}

/// Rows of `line` wrapped at `cols`, as text.
fn wrapped(line: &str, cols: usize) -> Vec<String> {
    let mut starts = vec![];
    crate::inspector::wrap_line(line, cols, &mut starts);
    starts.iter().enumerate().map(|(k, &a)| line[a as usize..starts.get(k + 1).map_or(line.len(), |&b| b as usize)].to_string()).collect()
}

#[test]
fn word_wrap_keeps_every_character_and_breaks_at_spaces() {
    let width = |s: &str| s.chars().map(crate::inspector::char_cols).sum::<usize>();
    // Real text: the licence and this crate's own sources, at narrow to wide columns.
    let mut text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/LICENSE")).unwrap();
    text.push_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/inspector.rs")).unwrap());
    let mut wrapped_some = 0;
    for cols in [8, 13, 40, 79] {
        for line in text.lines() {
            let rows = wrapped(line, cols);
            assert_eq!(rows.concat(), line, "nothing lost or doubled at {cols}");
            for (k, r) in rows.iter().enumerate() {
                assert!(width(r) <= cols, "{r:?} wider than {cols}");
                // A row ends at a space unless its next word would not fit on any row.
                if let Some(next) = rows.get(k + 1) {
                    let word = next.split(' ').next().unwrap_or("");
                    assert!(r.ends_with(' ') || !r.contains(' ') || width(word) + width(r.rsplit(' ').next().unwrap()) > cols, "{r:?} | {next:?} broke mid-word at {cols}");
                }
            }
            if rows.len() > 1 {
                wrapped_some += 1;
            }
            if width(line) <= cols {
                assert_eq!(rows.len(), 1, "{line:?} fits in {cols}");
            }
        }
    }
    assert!(wrapped_some > 1000, "long lines wrapped: {wrapped_some}");
    // A word longer than a row breaks inside it; wide characters count twice.
    assert_eq!(wrapped("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    assert_eq!(wrapped("the quick brown fox", 10), ["the quick ", "brown fox"]);
    assert_eq!(wrapped("日本語のテキスト", 6), ["日本語", "のテキ", "スト"]);
    assert_eq!(wrapped("", 10), [""]);
}

#[test]
fn checksums_match_known_values_and_pasted_formats() {
    use crate::checksum::{Algo, Job, clean};
    let d = tmpdir("checksums");
    // A name with a newline makes the tools escape their output line.
    let odd = d.join("we\nird name.txt");
    touch(&odd, "abc");
    let ctx = egui::Context::default();
    let want = [
        (Algo::Md5, "900150983cd24fb0d6963f7d28e17f72"),
        (Algo::Sha1, "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (Algo::Sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (Algo::Sha512, "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
    ];
    for (algo, hex) in want {
        assert_eq!(Algo::for_hex(hex), Some(algo));
        let job = Job::start(&odd, algo, &ctx);
        let t0 = Instant::now();
        let got = loop {
            if let Some(r) = job.poll() {
                break r;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "{algo:?} never finished");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(got.as_deref(), Ok(hex), "{algo:?}");
    }
    // What download pages and tools print comes down to the digits.
    let sha = want[2].1;
    assert_eq!(clean(&format!("  {}  ", sha.to_uppercase())), sha);
    assert_eq!(clean(&format!("{sha}  ubuntu.iso")), sha);
    assert_eq!(clean(&format!("SHA256 (ubuntu.iso) = {sha}")), sha);
    assert_eq!(clean("ba7816bf 8f01cfea 414140de 5dae2223 b00361a3 96177a9c b410ff61 f20015ad"), sha);
    assert_eq!(Algo::for_hex(&clean("not a checksum")), None);
    assert_eq!(Algo::for_hex("xyz7816bf8f01cfea414140de5dae2223"), None);
    // A file that is gone says so.
    let job = Job::start(&d.join("gone"), Algo::Sha256, &ctx);
    let t0 = Instant::now();
    let r = loop {
        if let Some(r) = job.poll() {
            break r;
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(r, Err("No such file or directory".into()));
}

#[test]
fn filter_conditions_by_size_and_date() {
    use std::time::SystemTime;
    let d = tmpdir("filterq");
    let now = SystemTime::now();
    let day = Duration::from_secs(86_400);
    // name, bytes, age
    let files = [("tiny.txt", 10, 0), ("page.html", 3000, 2), ("photo.jpg", 2 << 20, 10), ("movie.mkv", 150 << 20, 400), ("empty.log", 0, 40)];
    for (name, n, age) in files {
        let f = std::fs::File::create(d.join(name)).unwrap();
        f.set_len(n).unwrap();
        f.set_modified(now - day * age).unwrap();
    }
    std::fs::create_dir(d.join("Big folder")).unwrap();
    let mut stats = HashMap::new();
    let mut t = Tab::new(77, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let mut shown = |t: &mut Tab, stats: &HashMap<PathBuf, crate::stats::DirStats>, f: &str| -> Vec<String> {
        t.filter = f.into();
        pump(t, stats);
        let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(shown(&mut t, &stats, "size>1M"), ["movie.mkv", "photo.jpg"]);
    assert_eq!(shown(&mut t, &stats, "size<2k"), ["empty.log", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "size<3k"), ["empty.log", "page.html", "tiny.txt"], "k is 1024");
    assert_eq!(shown(&mut t, &stats, "size<=3000"), ["empty.log", "page.html", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "size:0"), ["empty.log"]);
    assert_eq!(shown(&mut t, &stats, "SIZE>1.5mb size<100MiB"), ["photo.jpg"]);
    assert_eq!(shown(&mut t, &stats, "modified<7d"), ["Big folder", "page.html", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "modified>1y"), ["movie.mkv"]);
    assert_eq!(shown(&mut t, &stats, "date:today"), ["Big folder", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "mod>=30d size>1k"), ["movie.mkv"]);
    // Conditions and a name search together; a folder's size counts once it is known.
    assert_eq!(shown(&mut t, &stats, "o size>1k"), ["movie.mkv", "photo.jpg"]);
    stats.insert(d.join("Big folder"), crate::stats::DirStats { size: 5 << 30, ..Default::default() });
    assert_eq!(shown(&mut t, &stats, "size>1g"), ["Big folder"]);
    // Words that only look like conditions stay part of the name search.
    assert_eq!(shown(&mut t, &stats, "size>huge"), Vec::<String>::new());
    assert_eq!(crate::query::name_part("size>huge"), "size>huge");
    assert_eq!(crate::query::name_part("report modified<2w"), "report");
    assert_eq!(crate::query::name_part("a:b"), "a:b");
    // Dates are local days, months and years.
    let tm = crate::timefmt::local(now);
    let (y, m, dd) = (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday);
    assert_eq!(shown(&mut t, &stats, &format!("modified:{y:04}-{m:02}-{dd:02}")), ["Big folder", "tiny.txt"]);
    let before_year = shown(&mut t, &stats, &format!("modified<{y:04}"));
    assert!(before_year.contains(&"movie.mkv".to_string()) && !before_year.contains(&"tiny.txt".to_string()), "{before_year:?}");
    let this_month = shown(&mut t, &stats, &format!("modified>={y:04}-{m:02}"));
    assert!(this_month.contains(&"tiny.txt".to_string()) && !this_month.contains(&"movie.mkv".to_string()), "{this_month:?}");
    // Spans mark exactly the recognised words.
    let f = crate::query::parse("big size>10M  modified<7d x", now);
    assert_eq!(f.spans, vec![(4, 12), (14, 25)]);
    assert_eq!(f.name, "big x");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn chosen_view_stays_while_browsing_and_saved_views_still_apply() {
    use crate::config::{SaveScope, SavedView, UserData};
    let d = tmpdir("viewstays");
    for f in ["a/inner", "b", "pics/trip"] {
        std::fs::create_dir_all(d.join(f)).unwrap();
    }
    let mut ud = UserData::default();
    let mut pics = ViewSettings::default();
    pics.zoom = 0.9;
    ud.views.insert(d.join("pics"), SavedView { scope: SaveScope::Subfolders, view: pics });
    let stats = HashMap::new();
    let mut t = Tab::new(5, d.clone(), ud.view_for(&d).0, egui::Context::default());
    pump(&mut t, &stats);
    let go = |t: &mut Tab, p: PathBuf| t.navigate(p.clone(), crate::app::App::nav_view(&ud, &p));
    // Medium icons, say: kept into a folder, its child, and back up.
    t.settings.zoom = 0.4;
    go(&mut t, d.join("a"));
    assert_eq!(t.settings.zoom, 0.4, "the chosen view follows into another folder");
    go(&mut t, d.join("a/inner"));
    assert_eq!(t.settings.zoom, 0.4);
    t.up(|p| crate::app::App::nav_view(&ud, p));
    assert_eq!(t.settings.zoom, 0.4, "and back up");
    // A folder with a saved view (and its subfolders) shows that one...
    go(&mut t, d.join("pics"));
    assert_eq!(t.settings.zoom, 0.9);
    go(&mut t, d.join("pics/trip"));
    assert_eq!(t.settings.zoom, 0.9, "inherited by subfolders");
    // ...and leaving it puts back the view chosen before, with Back and Forward too.
    go(&mut t, d.join("b"));
    assert_eq!(t.settings.zoom, 0.4, "the free view returns");
    t.back(|p| crate::app::App::nav_view(&ud, p));
    assert_eq!((t.path.clone(), t.settings.zoom), (d.join("pics/trip"), 0.9));
    t.forward(|p| crate::app::App::nav_view(&ud, p));
    assert_eq!((t.path.clone(), t.settings.zoom), (d.join("b"), 0.4));
    // Changing the view again carries on from here.
    t.settings.zoom = 0.0;
    go(&mut t, d.join("a"));
    assert_eq!(t.settings.zoom, 0.0);
    let _ = std::fs::remove_dir_all(&d);
}
