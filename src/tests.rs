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
    let text = [include_str!("../LICENSE"), include_str!("inspector.rs")].concat();
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

/// Make encrypted archives of `src` with the real tools: (ZipCrypto zip, AES zip,
/// 7z, 7z with its names encrypted), or None when zip or 7z is missing.
fn encrypted_archives(d: &Path, src: &Path, pw: &str) -> Option<[PathBuf; 4]> {
    let run = |prog: &str, args: &[&str]| std::process::Command::new(prog).args(args).current_dir(src).stdout(std::process::Stdio::null()).status().ok().filter(|s| s.success());
    let out = [d.join("crypto.zip"), d.join("aes.zip"), d.join("plain-names.7z"), d.join("hidden-names.7z")];
    let p = |i: usize| out[i].to_str().unwrap().to_string();
    run("zip", &["-q", "-r", "-P", pw, &p(0), "."])?;
    run("7z", &["a", &format!("-p{pw}"), "-tzip", "-mem=AES256", &p(1), "."])?;
    run("7z", &["a", &format!("-p{pw}"), &p(2), "."])?;
    run("7z", &["a", &format!("-p{pw}"), "-mhe=on", &p(3), "."])?;
    Some(out)
}

#[test]
fn password_protected_archives_list_extract_and_refuse() {
    use crate::archive::{self, NEEDS_PASSWORD, WRONG_PASSWORD};
    let d = tmpdir("encrypted");
    let src = d.join("src");
    std::fs::create_dir_all(src.join("Trip photos")).unwrap();
    touch(&src.join("notes.txt"), "the secret plan");
    touch(&src.join("Trip photos/beach.txt"), "sand");
    let Some(all) = encrypted_archives(&d, &src, "hunter2") else {
        eprintln!("zip or 7z missing: skipped");
        return;
    };
    for a in &all {
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        // Zips and plain-named 7z list without the password; hidden names need it.
        let listed = archive::list(a);
        if name.starts_with("hidden") {
            assert_eq!(listed.err().as_deref(), Some(NEEDS_PASSWORD), "{name}");
        } else {
            let items = listed.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(items.iter().any(|i| i.path == Path::new("Trip photos/beach.txt")), "{name}");
        }
        // Without it: refused, and nothing half-written is left to open later.
        let x = d.join(format!("x-{name}"));
        assert_eq!(archive::extract(a, &[PathBuf::from("notes.txt")], &x).err().as_deref(), Some(NEEDS_PASSWORD), "{name}");
        assert_eq!(archive::extract_for_open(a, Path::new("notes.txt")).err().as_deref(), Some(NEEDS_PASSWORD), "{name}");
        assert!(!archive::open_cache(a).join("notes.txt").exists(), "{name}: nothing cached");
        archive::set_password(a, "wrong".into());
        let r = archive::extract_for_open(a, Path::new("notes.txt"));
        assert_eq!(r.err().as_deref(), Some(WRONG_PASSWORD), "{name}");
        assert!(!archive::open_cache(a).join("notes.txt").exists(), "{name}: no zero-filled file cached");
        // With it: listed and extracted whole, folders with their contents.
        archive::set_password(a, "hunter2".into());
        assert!(archive::list(a).unwrap().iter().any(|i| i.path == Path::new("Trip photos") && i.is_dir), "{name}");
        let f = archive::extract_for_open(a, Path::new("notes.txt")).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(std::fs::read_to_string(f).unwrap(), "the secret plan", "{name}");
        let y = d.join(format!("y-{name}"));
        archive::extract(a, &[PathBuf::from("Trip photos")], &y).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(std::fs::read_to_string(y.join("Trip photos/beach.txt")).unwrap(), "sand", "{name}");
    }
    // 7z's own listing format.
    let items = archive::parse_slt("Path = sub\nSize = 0\nModified = 2026-10-15 19:19:57.7977160\nAttributes = D drwxr-xr-x\n\nPath = sub/b.txt\nSize = 5\nModified = 2026-10-15 19:19:57.7977160\nAttributes = A -rw-r--r--\n\nPath = ../evil\nSize = 1\n\nPath = deep/x.txt\nSize = 2\n");
    let get = |p: &str| items.iter().find(|i| i.path == Path::new(p)).cloned();
    assert!(get("sub").is_some_and(|i| i.is_dir) && get("sub/b.txt").is_some_and(|i| i.size == 5 && i.modified.is_some()));
    assert!(get("deep").is_some_and(|i| i.is_dir), "implied folder");
    assert!(get("../evil").is_none(), "no climbing out");
    for c in &all {
        let _ = std::fs::remove_dir_all(archive::open_cache(c));
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn tags_live_on_the_files_and_filter_the_list() {
    use crate::tags;
    let d = tmpdir("tags");
    let (a, b, c) = (d.join("a.txt"), d.join("b.txt"), d.join("c.txt"));
    for p in [&a, &b, &c] {
        touch(p, "x");
    }
    // Stored as KDE's file managers store them: one comma-separated attribute.
    if tags::write(&a, &["Red".into(), "work".into()]).is_err() {
        eprintln!("no user xattrs here: skipped");
        return;
    }
    let raw = |p: &Path| {
        let mut buf = [0u8; 256];
        let cp = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        let n = unsafe { libc::getxattr(cp.as_ptr(), c"user.xdg.tags".as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    };
    assert_eq!(raw(&a).as_deref(), Some("Red,work"));
    assert_eq!(tags::read(&a), ["Red", "work"]);
    assert_eq!(tags::parse(" Blue, ,taxes,Blue,"), ["Blue", "taxes"]);
    // One tag added to several files, then removed from all; the last one gone takes the
    // attribute with it.
    tags::toggle(&[a.clone(), b.clone()], "Green", true).unwrap();
    assert_eq!(tags::read(&a), ["Red", "work", "Green"]);
    assert_eq!(tags::read(&b), ["Green"]);
    tags::toggle(&[a.clone(), b.clone()], "green", false).unwrap();
    assert_eq!(tags::read(&b), Vec::<String>::new());
    assert_eq!(raw(&b), None, "no empty attribute left behind");
    // Tags written by another app are read too.
    let cp = std::ffi::CString::new(c.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::setxattr(cp.as_ptr(), c"user.xdg.tags".as_ptr(), b"Blue,taxes".as_ptr().cast(), 10, 0) }, 0);
    // The list filters by tag once the reader has them.
    let stats = HashMap::new();
    let mut t = Tab::new(91, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let ctx = egui::Context::default();
    let shown = |t: &mut Tab, f: &str| -> Vec<String> {
        t.filter = f.into();
        let start = Instant::now();
        loop {
            pump(t, &stats);
            tags::flush(&ctx);
            let pending = t.entries.iter().any(|e| tags::get(&e.path, |_| ()).is_none());
            if !pending || start.elapsed() > Duration::from_secs(5) {
                pump(t, &stats);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(shown(&mut t, "tag:red"), ["a.txt"]);
    assert_eq!(shown(&mut t, "tag:TAXES"), ["c.txt"]);
    assert_eq!(shown(&mut t, "tag:red tag:work"), ["a.txt"]);
    assert_eq!(shown(&mut t, "tag:red tag:taxes"), Vec::<String>::new());
    assert_eq!(crate::query::name_part("report tag:work"), "report");
    // A change made elsewhere is picked up once the watcher says the file changed.
    tags::write(&a, &[]).unwrap();
    tags::invalidate(&c);
    assert_eq!(shown(&mut t, "tag:red"), Vec::<String>::new());
    assert!(tags::known().contains(&"taxes".to_string()));
    let _ = std::fs::remove_dir_all(&d);
}

/// A history for the graph, written in one `git fast-import`: `n` commits on main, a
/// side branch merged back every 25 commits, a tag every 100, a rename, and at the top
/// ten branches joined by one octopus merge (more lanes than the graph draws).
pub(crate) fn git_history_fixture(d: &Path, n: usize) {
    use std::io::Write;
    std::fs::create_dir_all(d).unwrap();
    git(d, &["init", "-q"]);
    let mut s = String::new();
    let mut mark = 0;
    let mut commit = |s: &mut String, branch: &str, msg: &str, from: Option<usize>, merges: &[usize], files: &[(&str, String)]| -> usize {
        mark += 1;
        let t = 1_700_000_000 + mark as i64 * 600;
        let who = ["Ada Lovelace", "Grace Hopper", "Linus"][mark % 3];
        s.push_str(&format!("commit refs/heads/{branch}\nmark :{mark}\nauthor {who} <dev@example.invalid> {t} +0000\ncommitter {who} <dev@example.invalid> {t} +0000\ndata {}\n{msg}\n", msg.len()));
        if let Some(f) = from {
            s.push_str(&format!("from :{f}\n"));
        }
        for m in merges {
            s.push_str(&format!("merge :{m}\n"));
        }
        for (path, body) in files {
            if let Some(old) = path.strip_prefix("R ") {
                s.push_str(&format!("R {old}\n"));
            } else {
                s.push_str(&format!("M 644 inline {path}\ndata {}\n{body}\n", body.len()));
            }
        }
        mark
    };
    let mut main = commit(&mut s, "main", "Start the notes", None, &[], &[("notes.txt", "notes\n".into()), ("old name.txt", "x\n".into())]);
    for k in 2..=n {
        if k % 25 == 0 {
            let topic = format!("topic-{k}");
            let mut t = main;
            for j in 1..=3 {
                t = commit(&mut s, &topic, &format!("Work on topic {k}, part {j}"), Some(t), &[], &[(&format!("src/topic_{k}.rs"), format!("fn part_{j}() {{}}\n"))]);
            }
            main = commit(&mut s, "main", &format!("Note {k} while topic {k} is open"), Some(main), &[], &[("notes.txt", format!("note {k}\n"))]);
            // fast-import takes a merge's tree from its first parent: bring the topic's file in.
            main = commit(&mut s, "main", &format!("Merge topic {k}"), Some(main), &[t], &[(&format!("src/topic_{k}.rs"), "fn part_3() {}\n".into())]);
        } else if k == 40 {
            main = commit(&mut s, "main", "Rename the old file", Some(main), &[], &[("R \"old name.txt\" \"new name.txt\"", String::new())]);
        } else {
            main = commit(&mut s, "main", &format!("Note {k}"), Some(main), &[], &[("notes.txt", format!("notes\nnote {k}\n"))]);
        }
        if k % 100 == 0 {
            s.push_str(&format!("reset refs/tags/v{}\nfrom :{main}\n", k / 100));
        }
    }
    let wide: Vec<usize> = (1..=10).map(|w| commit(&mut s, &format!("wide-{w}"), &format!("Wide branch {w}"), Some(main), &[], &[(&format!("wide/{w}.txt"), format!("{w}\n"))])).collect();
    let all: Vec<(String, String)> = (1..=10).map(|w| (format!("wide/{w}.txt"), format!("{w}\n"))).collect();
    commit(&mut s, "main", "Join ten branches at once", Some(main), &wide, &all.iter().map(|(p, b)| (p.as_str(), b.clone())).collect::<Vec<_>>());
    let mut c = std::process::Command::new("git").args(["fast-import", "--quiet"]).current_dir(d).stdin(std::process::Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    assert!(c.wait().unwrap().success(), "fast-import");
    git(d, &["reset", "-q", "--hard"]);
}

/// A PNG of `w`×`h` with a pattern made from `seed`.
pub(crate) fn png_bytes(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut out = vec![];
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let px: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |x| [(x * 9 + seed * 70) as u8, (y * 7 + seed * 30) as u8, (120 + seed * 50) as u8, 255])).collect();
    e.write_header().unwrap().write_image_data(&px).unwrap();
    out
}

/// A small project for the commit view, five commits by three authors. "Lay out the
/// panels" changes twelve files in several folders in every way git records: added,
/// modified, deleted, renamed with an edit, a picture changed and one added, a binary
/// file, a file that became a link. Then a long line, a deleted picture and one more
/// edit. main is in step with origin/main, v0.1 and v1.0 are tags, and origin/feature
/// is a branch only the remote has.
pub(crate) fn commit_view_fixture(d: &Path) {
    std::fs::create_dir_all(d).unwrap();
    let mut day = 0;
    let mut commit = |who: &str, msg: &str| {
        day += 1;
        let (name, email) = who.split_once(" <").unwrap();
        let date = format!("2026-09-{day:02}T10:00:00+00:00");
        let ok = std::process::Command::new("git")
            .args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={}", email.trim_end_matches('>')), "-c", "commit.gpgsign=false", "add", "-A"])
            .current_dir(d)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git add");
        let ok = std::process::Command::new("git")
            .args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={}", email.trim_end_matches('>')), "-c", "commit.gpgsign=false", "commit", "-q", "-m", msg])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .current_dir(d)
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git commit {msg}");
    };
    let lines = |n: usize, word: &str| (1..=n).map(|i| format!("{word} line {i}\n")).collect::<String>();
    git(d, &["init", "-q"]);
    touch(&d.join("README.md"), "# Project\n\nA small project.\n");
    touch(&d.join("src/main.rs"), &format!("fn main() {{\n{}}}\n", lines(8, "    // main")));
    touch(&d.join("src/ui/view.rs"), &lines(6, "// view"));
    touch(&d.join("docs/guide.md"), &lines(12, "Guide"));
    touch(&d.join("old.txt"), &lines(3, "old"));
    touch(&d.join("notes.md"), "# Notes\n");
    touch(&d.join("tool.sh"), "#!/bin/sh\necho tool\n");
    std::fs::create_dir_all(d.join("assets/icons")).unwrap();
    std::fs::write(d.join("assets/logo.png"), png_bytes(48, 32, 1)).unwrap();
    std::fs::write(d.join("data.bin"), [0u8, 1, 2, 3, 0, 9, 8, 7]).unwrap();
    commit("Ada Lovelace <ada@example.invalid>", "Start the project");
    git(d, &["tag", "v0.1"]);
    touch(&d.join("src/main.rs"), &format!("mod ui;\n\nfn main() {{\n{}    ui::run();\n    ui::stop();\n}}\n", lines(7, "    // main")));
    touch(&d.join("src/ui/panel.rs"), &lines(12, "// panel"));
    touch(&d.join("src/ui/view.rs"), &format!("{}// view two\n// view three\n", lines(4, "// view")));
    std::fs::remove_file(d.join("old.txt")).unwrap();
    std::fs::remove_file(d.join("docs/guide.md")).unwrap();
    touch(&d.join("docs/handbook.md"), &format!("{}The handbook ends here.\n", lines(12, "Guide")));
    std::fs::write(d.join("assets/logo.png"), png_bytes(64, 40, 2)).unwrap();
    std::fs::write(d.join("assets/icons/icon.png"), png_bytes(24, 24, 3)).unwrap();
    std::fs::write(d.join("data.bin"), [0u8, 1, 2, 3, 0, 9, 8, 6, 5]).unwrap();
    std::fs::remove_file(d.join("tool.sh")).unwrap();
    std::os::unix::fs::symlink("src/main.rs", d.join("tool.sh")).unwrap();
    touch(&d.join("tests/layout.rs"), &lines(5, "// layout test"));
    touch(&d.join("tests/panel.rs"), &lines(4, "// panel test"));
    touch(&d.join("README.md"), "# Project\n\nA small project with panels.\n");
    commit("Grace Hopper <grace@example.invalid>", "Lay out the panels\n\nSplit the view into panels. The old layout code goes,\nand the guide is now the handbook.");
    git(d, &["tag", "v1.0"]);
    let long = "A long line in the notes keeps going well past the right edge of the panel, and wrapped it stays readable. ".repeat(3);
    touch(&d.join("notes.md"), &format!("# Notes\n{long}\n"));
    commit("Linus <linus@example.invalid>", "Write the long notes");
    git(d, &["update-ref", "refs/remotes/origin/feature", "HEAD"]);
    std::fs::remove_file(d.join("assets/logo.png")).unwrap();
    commit("Ada Lovelace <ada@example.invalid>", "Remove the logo");
    touch(&d.join("src/ui/panel.rs"), &format!("{}// tuned\n", lines(12, "// panel")));
    commit("Grace Hopper <grace@example.invalid>", "Tune the panel");
    git(d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
}

/// The history's commits as the graph reads them, all at once.
fn git_log_all(d: &Path) -> Vec<crate::gitlog::Commit> {
    let out = std::process::Command::new("git").arg("-C").arg(d).args(crate::gitlog::log_args()).output().unwrap();
    crate::gitlog::read_page(&mut &out.stdout[..], usize::MAX)
}

#[test]
fn git_history_lanes_edges_and_changes() {
    use crate::gitlog::{Lanes, RefKind, Row};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("gitlog");
    touch(&d.join("a"), "a\n");
    git(&d, &["init", "-q"]);
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "c1"]);
    touch(&d.join("a"), "a\nb\n");
    git(&d, &["commit", "-qam", "c2"]);
    git(&d, &["tag", "v1"]);
    git(&d, &["checkout", "-qb", "feat"]);
    touch(&d.join("f"), "f\n");
    git(&d, &["add", "f"]);
    git(&d, &["commit", "-qm", "f1"]);
    touch(&d.join("f"), "f\nf\n");
    git(&d, &["commit", "-qam", "f2"]);
    git(&d, &["checkout", "-q", "main"]);
    touch(&d.join("a"), "a\nb\nc\n");
    git(&d, &["commit", "-qam", "c3"]);
    git(&d, &["merge", "-q", "--no-ff", "-m", "merge feat", "feat"]);
    git(&d, &["mv", "a", "b"]);
    git(&d, &["commit", "-qm", "ren"]);
    let log = git_log_all(&d);
    let subjects: Vec<&str> = log.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["ren", "merge feat", "f2", "f1", "c3", "c2", "c1"]);
    assert_eq!(log[0].refs, [(RefKind::Head, "main".to_string())]);
    assert_eq!(log[2].refs, [(RefKind::Branch, "feat".to_string())]);
    assert_eq!(log[5].refs, [(RefKind::Tag, "v1".to_string())]);
    assert_eq!((log[1].parents.len(), log[6].parents.len(), log[0].author.as_str()), (2, 0, "Test"));
    let mut lanes = Lanes::default();
    let rows: Vec<Row> = log.iter().map(|c| lanes.place(c)).collect();
    let r = |col, up, ins: &[u16], outs: &[u16], pass: &[u16]| Row { col, up, ins: ins.to_vec(), outs: outs.to_vec(), pass: pass.to_vec() };
    // The trunk stays in lane 0; the branch takes lane 1 from the merge down to where it
    // began, and curves back into the trunk there.
    assert_eq!(rows[0], r(0, false, &[], &[0], &[]));
    assert_eq!(rows[1], r(0, true, &[], &[0, 1], &[]));
    assert_eq!(rows[2], r(1, true, &[], &[1], &[0]));
    assert_eq!(rows[3], r(1, true, &[], &[1], &[0]));
    assert_eq!(rows[4], r(0, true, &[], &[0], &[1]));
    assert_eq!(rows[5], r(0, true, &[1], &[0], &[]));
    assert_eq!(rows[6], r(0, true, &[], &[], &[]));
    // What each commit changed, and a file's diff.
    let (body, ch) = crate::gitlog::changes(&d, &log[0].hash, &log[0].parents);
    assert_eq!((body.as_str(), ch.len(), ch[0].status, ch[0].path.as_str(), ch[0].from.as_deref()), ("", 1, b'R', "b", Some("a")));
    let (_, ch) = crate::gitlog::changes(&d, &log[1].hash, &log[1].parents);
    assert_eq!(ch.iter().map(|c| (c.status, c.path.as_str())).collect::<Vec<_>>(), [(b'A', "f")], "a merge shows what it brought in");
    let (_, ch) = crate::gitlog::changes(&d, &log[6].hash, &log[6].parents);
    assert_eq!((ch.len(), ch[0].status), (1, b'A'), "the root commit adds its files");
    let (_, ch) = crate::gitlog::changes(&d, &log[4].hash, &log[4].parents);
    let (lines, _, cut) = crate::gitlog::diff(&d, &log[4].hash, &log[4].parents, &ch[0]);
    assert_eq!(lines, ["@@ -1,2 +1,3 @@", " a", " b", "+c"]);
    assert!(!cut);
    assert_eq!(crate::gitlog::ago(1000, 1000 + 3 * 86_400 + 5), "3 days ago");
    assert_eq!(crate::gitlog::ago(1000, 1000 + 3600), "1 hour ago");
    assert_eq!(crate::gitlog::ago(1000, 1010), "just now");
    // Not a repository, or one without commits: nothing to read.
    let empty = tmpdir("gitlog-empty");
    git(&empty, &["init", "-q"]);
    assert!(git_log_all(&empty).is_empty());
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn git_history_pages_and_wide_merges() {
    use crate::gitlog::{Lanes, PAGE, read_page};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("gitlog-big");
    git_history_fixture(&d, 700);
    let all = git_log_all(&d);
    assert!(all.len() > 2 * PAGE + 50, "{} commits", all.len());
    // Read page by page from one running git, lanes carried across: the same graph as
    // all at once.
    let mut child = std::process::Command::new("git").arg("-C").arg(&d).args(crate::gitlog::log_args()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut r = std::io::BufReader::new(child.stdout.take().unwrap());
    let (mut paged, mut one) = (Lanes::default(), Lanes::default());
    let mut n = 0;
    loop {
        let page = read_page(&mut r, PAGE);
        for c in &page {
            assert_eq!(paged.place(c), one.place(&all[n]), "row {n}");
            assert_eq!(c.hash, all[n].hash);
            n += 1;
        }
        if page.len() < PAGE {
            break;
        }
    }
    let _ = child.wait();
    assert_eq!(n, all.len());
    // The octopus at the top fans out into eleven lanes; the trunk keeps lane 0 and the
    // lanes are free again further down.
    let mut lanes = Lanes::default();
    let rows: Vec<_> = all.iter().map(|c| lanes.place(c)).collect();
    assert_eq!(all[0].subject, "Join ten branches at once");
    assert_eq!(rows[0].outs.len(), 11);
    assert!(rows[0].outs.iter().any(|&l| l as usize >= crate::gitlog::LANES), "more lanes than drawn");
    let trunk: Vec<usize> = all.iter().enumerate().filter(|(_, c)| c.subject.starts_with("Note ")).map(|(i, _)| i).collect();
    assert!(trunk.iter().all(|&i| rows[i].col == 0), "main stays in the first lane");
    let topic = all.iter().position(|c| c.subject == "Work on topic 50, part 3").unwrap();
    assert_eq!(rows[topic].col, 1);
    assert!(rows.iter().skip(20).all(|r| r.pass.len() <= 2), "lanes are given back");
    let tags: Vec<&str> = all.iter().flat_map(|c| c.refs.iter().filter(|r| r.0 == crate::gitlog::RefKind::Tag).map(|r| r.1.as_str())).collect();
    assert_eq!(tags, ["v7", "v6", "v5", "v4", "v3", "v2", "v1"]);
    let ren = all.iter().find(|c| c.subject == "Rename the old file").unwrap();
    let (_, ch) = crate::gitlog::changes(&d, &ren.hash, &ren.parents);
    assert_eq!((ch[0].status, ch[0].path.as_str(), ch[0].from.as_deref()), (b'R', "new name.txt", Some("old name.txt")));
    let _ = std::fs::remove_dir_all(&d);
}

/// Line counts as `git diff-tree -z --numstat` writes them (binary files, a rename with
/// its two paths) joined to the name-status list, then against real git.
#[test]
fn git_numstat_and_renames_in_a_commit() {
    use crate::gitlog::{Change, join_numstat, parse_changes, parse_numstat};
    let num = b"-\t-\tb.bin\x001\t0\t\x00a.txt\x00d/a2.txt\x000\t1\td/x\x001\t0\tnew\x00";
    let counts = parse_numstat(num);
    assert_eq!(counts, [(None, "b.bin".to_string()), (Some((1, 0)), "d/a2.txt".into()), (Some((0, 1)), "d/x".into()), (Some((1, 0)), "new".into())]);
    let mut files = parse_changes(b"M\x00b.bin\x00R075\x00a.txt\x00d/a2.txt\x00D\x00d/x\x00A\x00new\x00");
    join_numstat(&mut files, &counts);
    let ch = |status: u8, path: &str, from: Option<&str>, lines: Option<(u32, u32)>| Change { status, path: path.into(), from: from.map(String::from), lines };
    assert_eq!(files, [ch(b'M', "b.bin", None, None), ch(b'R', "d/a2.txt", Some("a.txt"), Some((1, 0))), ch(b'D', "d/x", None, Some((0, 1))), ch(b'A', "new", None, Some((1, 0)))]);
    // A count list cut short or out of order is matched by path.
    let mut files = parse_changes(b"A\x00new\x00D\x00d/x\x00");
    join_numstat(&mut files, &counts[2..]);
    assert_eq!((files[0].lines, files[1].lines), (Some((1, 0)), Some((0, 1))));
    if !crate::media::have("git") {
        eprintln!("git not installed: the rest skipped");
        return;
    }
    let d = tmpdir("commitview");
    commit_view_fixture(&d);
    let log = git_log_all(&d);
    let panels = log.iter().find(|c| c.subject == "Lay out the panels").unwrap();
    let (body, files) = crate::gitlog::changes(&d, &panels.hash, &panels.parents);
    assert!(body.starts_with("Split the view into panels."), "{body:?}");
    let get = |p: &str| files.iter().find(|f| f.path == p).unwrap_or_else(|| panic!("{p} in {files:?}"));
    assert_eq!(files.len(), 12);
    assert_eq!((get("src/main.rs").status, get("src/main.rs").lines), (b'M', Some((4, 1))));
    assert_eq!((get("src/ui/panel.rs").status, get("src/ui/panel.rs").lines), (b'A', Some((12, 0))));
    assert_eq!((get("old.txt").status, get("old.txt").lines), (b'D', Some((0, 3))));
    assert_eq!((get("docs/handbook.md").status, get("docs/handbook.md").from.as_deref(), get("docs/handbook.md").lines), (b'R', Some("docs/guide.md"), Some((1, 0))));
    assert_eq!((get("assets/logo.png").status, get("assets/logo.png").lines), (b'M', None), "a picture is binary");
    assert_eq!(get("data.bin").lines, None);
    assert_eq!(get("tool.sh").status, b'T', "a file that became a link");
    assert_eq!(files.iter().filter_map(|f| f.lines).map(|l| l.0).sum::<u32>(), 30);
    // Both sides of the changed picture, only the new side of the added one; the file
    // as of the commit; one file's commits followed through its rename.
    let logo = get("assets/logo.png");
    let side = |old: bool, f: &Change| crate::gitlog::picture_side(&d, &panels.hash, &panels.parents, f, old, 512).map(|p| p.1);
    assert_eq!((side(true, logo), side(false, logo)), (Some([48, 32]), Some([64, 40])));
    assert_eq!(side(true, get("assets/icons/icon.png")), None);
    assert_eq!(side(false, get("assets/icons/icon.png")), Some([24, 24]));
    match crate::gitlog::blob(&d, &panels.hash, "src/main.rs") {
        crate::gitlog::Blob::Text(l, spans, cut) => assert!(l[0] == "mod ui;" && spans.is_some() && !cut),
        _ => panic!("main.rs as text"),
    }
    assert!(matches!(crate::gitlog::blob(&d, &panels.hash, "data.bin"), crate::gitlog::Blob::Nothing("Binary file")));
    assert!(matches!(crate::gitlog::blob(&d, &panels.hash, "assets/logo.png"), crate::gitlog::Blob::Picture(_, [64, 40])));
    let out = std::process::Command::new("git").arg("-C").arg(&d).args(crate::gitlog::file_log_args("docs/handbook.md")).output().unwrap();
    let follow: Vec<String> = crate::gitlog::read_page(&mut &out.stdout[..], 100).into_iter().map(|c| c.subject).collect();
    assert_eq!(follow, ["Lay out the panels", "Start the project"]);
    let _ = std::fs::remove_dir_all(&d);
}

/// A commit's header: author and email, and its refs from git's decorations (a remote's
/// HEAD dropped, a branch in step with its remote one pill, a remote-only branch with
/// its own, tags).
#[test]
fn git_header_refs_and_author() {
    use crate::gitlog::{RefKind, labels, parse};
    let rec = "\n0123456789abcdef0123456789abcdef01234567\x1faaaaaaa bbbbbbb\x1fAda Lovelace\x1fada@example.invalid\x1f1700000000\x1fHEAD -> refs/heads/main, refs/remotes/origin/main, refs/remotes/origin/HEAD, tag: refs/tags/v1.0, refs/remotes/origin/feature, refs/stash\x1fLay out the panels";
    let c = parse(rec.as_bytes()).expect("a record");
    assert_eq!((c.author.as_str(), c.email.as_str(), c.time, c.parents.len(), c.subject.as_str()), ("Ada Lovelace", "ada@example.invalid", 1_700_000_000, 2, "Lay out the panels"));
    assert_eq!(c.refs, [(RefKind::Head, "main".to_string()), (RefKind::Remote, "origin/main".into()), (RefKind::Tag, "v1.0".into()), (RefKind::Remote, "origin/feature".into())]);
    assert_eq!(labels(&c.refs), [(RefKind::Head, "main", Some("origin")), (RefKind::Tag, "v1.0", None), (RefKind::Remote, "origin/feature", None)]);
    let detached = parse(b"0123456789abcdef0123456789abcdef01234567\x1f\x1fX\x1f\x1f0\x1fHEAD\x1froot").unwrap();
    assert_eq!((detached.refs.clone(), detached.parents.len()), (vec![(RefKind::Head, "HEAD".to_string())], 0));
    assert!(parse(b"short\x1f\x1fX\x1f\x1f0\x1f\x1fs").is_none(), "not a hash");
}

/// An author keeps one colour: from the email, whatever its case or spacing, else from
/// the name; the hash is fixed, so it stays across sessions. Initials read the first
/// and last words.
#[test]
fn git_author_colour_is_stable() {
    use crate::gitlog::{author_color, author_hue, initials};
    let a = author_hue("ada@example.invalid", "Ada Lovelace");
    assert_eq!(a, author_hue(" ADA@Example.invalid ", "someone else"));
    assert_eq!(author_hue("", "Linus"), author_hue("", "linus"), "no email: the name");
    // Pinned: a change to the hash would recolour every author.
    assert_eq!((a, author_hue("grace@example.invalid", ""), author_hue("linus@example.invalid", "")), (8, 0, 9));
    let hues: HashSet<usize> = (0..40).map(|i| author_hue(&format!("dev{i}@example.invalid"), "")).collect();
    assert!(hues.len() >= 6, "authors spread over the colours: {hues:?}");
    let (dark, light) = (crate::theme::palette(crate::theme::ThemeName::Midnight), crate::theme::palette(crate::theme::ThemeName::Paper));
    assert_eq!(author_color(&dark, "ada@example.invalid", ""), author_color(&dark, "ADA@example.invalid", ""));
    assert_ne!(author_color(&dark, "ada@example.invalid", ""), author_color(&light, "ada@example.invalid", ""), "toned to the theme");
    assert_eq!(initials("Ada Lovelace"), "AL");
    assert_eq!(initials("Grace Brewster Murray Hopper"), "GH");
    assert_eq!(initials("linus"), "L");
    assert_eq!(initials("first.last"), "FL");
    assert_eq!(initials("  "), "?");
    assert_eq!(initials("émile zola"), "ÉZ");
}

/// Many files grouped by folder: folders first, a folder holding only a folder as one
/// heading, each heading knowing how many rows and files it holds. Few files, or files
/// in one folder, stay a list. And the counts' five blocks and summary.
#[test]
fn git_files_grouped_by_folder() {
    use crate::gitlog::{Change, Node, blocks, group, grouped, summary};
    let f = |p: &str| Change { status: b'M', path: p.into(), from: None, lines: Some((1, 1)) };
    let files: Vec<Change> = ["README.md", "src/main.rs", "src/ui/view.rs", "src/ui/panel.rs", "docs/a/b/guide.md", "tests/x.rs", "tests/y.rs", "Cargo.toml", "src/lib.rs"].iter().map(|p| f(p)).collect();
    assert!(grouped(&files));
    assert!(!grouped(&files[..8]), "eight files stay a list");
    let one: Vec<Change> = (0..20).map(|i| f(&format!("src/f{i}.rs"))).collect();
    assert!(!grouped(&one), "all in one folder");
    let dir = |path: &str, name: &str, depth: u16, len: usize, files: usize| Node::Dir { path: path.into(), name: name.into(), depth, len, files };
    let file = |idx: usize, depth: u16| Node::File { idx, depth };
    assert_eq!(
        group(&files),
        [
            dir("docs/a/b", "docs/a/b", 0, 1, 1),
            file(4, 1),
            dir("src", "src", 0, 5, 4),
            dir("src/ui", "ui", 1, 2, 2),
            file(3, 2),
            file(2, 2),
            file(8, 1),
            file(1, 1),
            dir("tests", "tests", 0, 2, 2),
            file(5, 1),
            file(6, 1),
            file(7, 0),
            file(0, 0),
        ]
    );
    assert_eq!(blocks(0, 0), (0, 0));
    assert_eq!(blocks(3, 0), (3, 0), "fewer lines than blocks");
    assert_eq!(blocks(100, 0), (5, 0));
    assert_eq!(blocks(1, 99), (1, 4), "a side with lines keeps a block");
    assert_eq!(blocks(99, 1), (4, 1));
    assert_eq!(blocks(30, 30), (3, 2));
    assert_eq!((summary(1), summary(12)), ("1 file changed".to_string(), "12 files changed".to_string()));
    assert!(crate::gitlog::is_picture("assets/Logo.PNG") && crate::gitlog::is_picture("a.svg") && !crate::gitlog::is_picture("png") && !crate::gitlog::is_picture("x.png/notes"));
}

/// The history's git runs a page ahead of nothing: after the first page it waits on its
/// pipe until asked, and closing the history kills it.
#[test]
fn git_history_reads_on_demand_and_stops() {
    use crate::gitlog::{History, PAGE};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("gitlog-feed");
    git_history_fixture(&d, 700);
    let ctx = egui::Context::default();
    let mut h = History::new(d.clone(), "main".into(), false);
    let wait = |h: &mut History, n: usize| {
        let t0 = Instant::now();
        while h.state().0 < n && t0.elapsed() < Duration::from_secs(10) {
            h.poll(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        h.state().0
    };
    assert_eq!(wait(&mut h, PAGE), PAGE);
    std::thread::sleep(Duration::from_millis(300));
    h.poll(&ctx);
    assert_eq!(h.state().0, PAGE, "nothing more read until asked");
    let pid = h.git_pid();
    assert!(pid != 0 && std::path::Path::new(&format!("/proc/{pid}")).exists(), "git waits on its pipe");
    h.more(&ctx);
    assert_eq!(wait(&mut h, 2 * PAGE), 2 * PAGE);
    drop(h);
    let t0 = Instant::now();
    let gone = || std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.split_whitespace().nth(2) == Some("Z"));
    while !gone() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(gone(), "git is stopped with its history");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_repositorys_own_config_runs_nothing_when_maia_reads_it() {
    let d = tmpdir("evilrepo");
    let g = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&d).stdout(std::process::Stdio::null()).status().is_ok_and(|s| s.success()), "git {args:?}");
    g(&["init", "-q"]);
    touch(&d.join("notes.txt"), "one");
    g(&["add", "notes.txt"]);
    g(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "first"]);
    // As a downloaded repository could arrive: an fsmonitor hook and filters (one named
    // with a space) that each leave a marker if run, and files whose stat no longer
    // matches the index, so status re-reads them through the filters.
    let mark = |n: &str| format!("touch '{}'", d.join(n).display());
    g(&["config", "core.fsmonitor", &mark("ran-fsmonitor")]);
    g(&["config", "filter.x.clean", &format!("sh -c \"{}; cat\"", mark("ran-clean"))]);
    g(&["config", "filter.my x.process", &mark("ran-process")]);
    touch(&d.join(".gitattributes"), "*.txt filter=x\n*.md filter=my\\ x\n");
    touch(&d.join("notes.txt"), "two");
    touch(&d.join("readme.md"), "new");
    let repo = crate::gitstatus::query(&d).expect("read the status");
    assert!(!repo.marks.is_empty(), "the status was still read");
    let (_, changed) = crate::gitlog::changes(&d, "HEAD", &[]);
    assert!(!changed.is_empty());
    for m in ["ran-fsmonitor", "ran-clean", "ran-process"] {
        assert!(!d.join(m).exists(), "{m}: the repository's config ran a command");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ahead_behind_and_branch_labels_from_real_remotes() {
    use crate::gitlog::{RefKind, labels};
    let d = tmpdir("remotes");
    let run = |at: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args).current_dir(at).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // A "server" and two clones of it: ours, and someone else's that pushes once.
    std::fs::create_dir_all(d.join("server.git")).unwrap();
    run(&d.join("server.git"), &["init", "-q", "--bare"]);
    run(&d, &["clone", "-q", "server.git", "ours"]);
    let ours = d.join("ours");
    touch(&ours.join("a.txt"), "a");
    run(&ours, &["add", "."]);
    run(&ours, &["commit", "-qm", "first"]);
    run(&ours, &["push", "-q", "-u", "origin", "main"]);
    run(&ours, &["branch", "docs"]);
    run(&ours, &["push", "-q", "origin", "docs"]);
    run(&d, &["clone", "-q", "server.git", "theirs"]);
    touch(&d.join("theirs/b.txt"), "b");
    run(&d.join("theirs"), &["add", "."]);
    run(&d.join("theirs"), &["commit", "-qm", "theirs"]);
    run(&d.join("theirs"), &["push", "-q"]);
    for n in ["c", "e"] {
        touch(&ours.join(n), n);
        run(&ours, &["add", "."]);
        run(&ours, &["commit", "-qm", n]);
    }
    run(&ours, &["fetch", "-q"]);
    let repo = crate::gitstatus::query(&ours).expect("status");
    assert_eq!((repo.branch.as_str(), repo.ahead, repo.behind), ("main", 2, 1), "two to push, one to pull");
    // Status lines git writes, parsed.
    let line = |s: &str| crate::gitstatus::parse(ours.clone(), format!("## {s}\0").as_bytes());
    assert_eq!((line("main...origin/main [behind 7]").ahead, line("main...origin/main [behind 7]").behind), (0, 7));
    assert_eq!((line("main...origin/main [gone]").ahead, line("main...origin/main [gone]").behind), (0, 0));
    assert_eq!((line("topic").branch.as_str(), line("topic").ahead), ("topic", 0), "no upstream");
    // The refs on each commit, from git's own decorations: docs sits with origin/docs (one
    // pill), main is ahead of origin/main (two pills, the remote one marked as such).
    let log = run(&ours, &["log", "-z", "--decorate=full", crate::gitlog::FORMAT, "--branches", "--remotes", "--tags", "HEAD", "--"]);
    let commits: Vec<crate::gitlog::Commit> = log.split('\0').filter_map(|r| crate::gitlog::parse(r.as_bytes())).collect();
    let on = |subject: &str| commits.iter().find(|c| c.subject == subject).map(|c| labels(&c.refs).into_iter().map(|(k, n, r)| (k, n.to_string(), r.map(String::from))).collect::<Vec<_>>()).unwrap();
    assert_eq!(on("e"), vec![(RefKind::Head, "main".into(), None)]);
    assert_eq!(on("first"), vec![(RefKind::Branch, "docs".into(), Some("origin".into()))]);
    assert_eq!(on("theirs"), vec![(RefKind::Remote, "origin/main".into(), None)]);
    let _ = std::fs::remove_dir_all(&d);
}

fn unbase64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let b: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    b.chunks(4).flat_map(|c| {
        let n = c.iter().enumerate().fold(0u32, |n, (k, &x)| n | (val(x) as u32) << (18 - 6 * k));
        (0..c.len() - 1).map(move |k| (n >> (16 - 8 * k)) as u8)
    }).collect()
}

/// What test pictures can show; a picture's red tells which (it survives the shrinking
/// a picture gets on its way to the model).
const CAPTIONS: &[&str] = &["a puppy playing in the sand", "a spreadsheet of numbers", "icon"];

/// The largest side of a picture the fake Ollama was sent.
static PICTURE_SIDE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What the fake Ollama "sees" in a picture.
fn caption(png: &[u8]) -> String {
    let Some(img) = crate::imgdec::decode_bytes(png) else { return String::new() };
    PICTURE_SIDE.fetch_max(img.w.max(img.h), std::sync::atomic::Ordering::Relaxed);
    let red = img.px.first().copied().unwrap_or(0) as usize;
    CAPTIONS.get(red / 40).map_or(String::new(), |c| c.to_string())
}

/// A picture for the tests, 640 by 480 (a photo's shape) showing `caption`, one of CAPTIONS.
pub(crate) fn captioned_png(path: &Path, caption: &str) {
    sized_png(path, caption, 640, 480)
}

pub(crate) fn sized_png(path: &Path, caption: &str, w: u32, h: u32) {
    let red = CAPTIONS.iter().position(|c| *c == caption).expect("a known caption") as u8 * 40 + 20;
    let mut seed = 0x2545f491u32;
    let px = (0..w * h).flat_map(|_| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        [red, (seed >> 8) as u8, (seed >> 16) as u8, 255]
    }).collect();
    let img = crate::imgdec::Rgba { w, h, px };
    std::fs::write(path, crate::thumbcache::encode_png(&img, &[]).unwrap()).unwrap();
}

/// A stand-in for Ollama on a local port, speaking its HTTP API: /api/tags, a chunked
/// /api/pull with progress, and /api/embed. Its "model" puts words about one topic in
/// the same direction (money, holidays, cooking, pets, meetings), so a question finds a
/// file about the same thing without sharing its words. Returns its address, whether the
/// model is "downloaded", and how many texts it has embedded.
pub(crate) fn fake_ollama(has_model: bool) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let model = std::sync::Arc::new(AtomicBool::new(has_model));
    let embedded = std::sync::Arc::new(AtomicUsize::new(0));
    let (m, n) = (model.clone(), embedded.clone());
    const TOPICS: &[&[&str]] = &[
        &["invoice", "bill", "receipt", "payment", "tax", "taxes", "refund", "bank", "money", "government", "owe", "paid"],
        &["holiday", "vacation", "trip", "beach", "travel", "flight", "hotel", "sea", "sand", "summer"],
        &["recipe", "cooking", "pasta", "dinner", "bake", "kitchen", "soup", "eat", "food", "sauce"],
        &["dog", "cat", "pet", "puppy", "vet", "animal", "walk", "leash"],
        &["meeting", "agenda", "minutes", "schedule", "calendar", "standup", "monday"],
    ];
    let vector = |text: &str| -> Vec<f32> {
        let body = text.rsplit_once(" | query: ").or_else(|| text.rsplit_once(" | text: ")).map_or(text, |(_, b)| b);
        let mut v = vec![0.0f32; 768];
        for w in body.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase) {
            match TOPICS.iter().position(|t| t.contains(&w.as_str())) {
                Some(t) => v[t] += 3.0,
                None => {
                    let h = w.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
                    v[32 + (h as usize % 700)] += 0.4;
                }
            }
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter().map(|x| x / norm).collect()
    };
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            let (method, path) = { let mut p = line.split_whitespace(); (p.next().unwrap_or("").to_string(), p.next().unwrap_or("").to_string()) };
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let mut s = s;
            let reply = |s: &mut std::net::TcpStream, code: &str, json: String| {
                let _ = write!(s, "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len());
            };
            match (method.as_str(), path.as_str()) {
                ("GET", "/api/tags") => {
                    let models = if m.load(Relaxed) { r#"[{"name":"hf.co/ggml-org/embeddinggemma-2-GGUF:Q8_0","size":864000000}]"# } else { "[]" };
                    reply(&mut s, "200 OK", format!(r#"{{"models":{models}}}"#));
                }
                ("POST", "/api/pull") => {
                    // Streamed as Ollama streams it: chunked, one JSON object per line.
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    let mut lines = vec![r#"{"status":"pulling manifest"}"#.to_string()];
                    lines.extend((1..=4).map(|k| format!(r#"{{"status":"pulling","digest":"sha256:ab","total":300000000,"completed":{}}}"#, k * 75_000_000)));
                    lines.push(r#"{"status":"success"}"#.into());
                    for l in lines {
                        let chunk = format!("{l}\n");
                        let _ = write!(s, "{:x}\r\n{chunk}\r\n", chunk.len());
                        let _ = s.flush();
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    let _ = write!(s, "0\r\n\r\n");
                    m.store(true, Relaxed);
                }
                ("POST", "/api/embed") if m.load(Relaxed) => {
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    // Text as it is; a picture by the caption its PNG carries (the test's
                    // stand-in for what the real model sees in it).
                    let inputs: Vec<String> = v["input"].as_array().map(|a| a.iter().map(|x| match x.as_str() {
                        Some(t) => t.to_string(),
                        None => caption(&unbase64(x["image"].as_str().unwrap_or(""))),
                    }).collect()).unwrap_or_default();
                    n.fetch_add(inputs.len(), Relaxed);
                    let embs: Vec<Vec<f32>> = inputs.iter().map(|t| vector(t)).collect();
                    reply(&mut s, "200 OK", serde_json::json!({ "model": "embeddinggemma", "embeddings": embs }).to_string());
                }
                ("POST", "/api/embed") => reply(&mut s, "404 Not Found", r#"{"error":"model not found, try pulling it first"}"#.into()),
                _ => reply(&mut s, "404 Not Found", r#"{"error":"not found"}"#.into()),
            }
        }
    });
    (addr, model, embedded)
}

#[test]
fn search_by_meaning_through_ollama() {
    use crate::ollama::{self, Status};
    use crate::semantic::{self, Engine, Phase};
    use std::sync::atomic::Ordering::Relaxed;
    let set_host = |h: &str| *ollama::TEST_HOST.lock().unwrap() = Some(h.to_string());
    // Nothing listening: not running, said plainly and quickly.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    set_host(&closed);
    let t0 = Instant::now();
    assert_eq!(ollama::status(), Status::NotRunning);
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(ollama::embed(&["x".into()]).err().as_deref(), Some("Ollama isn't running"));
    // Running without the model: asked to fetch it, which streams progress.
    let (addr, _model, embedded) = fake_ollama(false);
    set_host(&addr);
    assert_eq!(ollama::status(), Status::NoModel);
    assert!(ollama::embed(&["x".into()]).err().unwrap().contains("not found"));
    let mut seen = vec![];
    ollama::pull(|d, a| seen.push((d, a)), &std::sync::atomic::AtomicBool::new(false)).unwrap();
    assert_eq!(seen.last(), Some(&(300_000_000, 300_000_000)));
    assert_eq!(seen.len(), 4, "every progress line, through the chunked stream");
    assert_eq!(ollama::status(), Status::Ready);
    let e = ollama::embed(&["task: search result | query: a bill".into(), "title: none | text: a beach".into()]).unwrap();
    assert_eq!(e.len(), 2);
    for v in &e {
        assert_eq!(v.len(), ollama::DIMS);
        assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-3, "length 1 after cutting to {}", ollama::DIMS);
    }
    // Passages: the whole text covered, each about a passage long, on character edges.
    let text = [include_str!("../LICENSE"), "日本語のテキスト。".repeat(300).as_str()].concat();
    let ps = semantic::cut(&text);
    assert!(ps.len() > 20);
    let mut reach = 0;
    for &(a, n) in &ps {
        assert!(text.is_char_boundary(a as usize) && text.is_char_boundary((a + n) as usize));
        assert!(n as usize <= 1800 + 4);
        assert!(a as usize <= reach, "a gap before {a}");
        reach = reach.max((a + n) as usize);
    }
    assert!(reach + 20 >= text.len() || ps.len() == 120, "covered to {reach} of {}", text.len());
    // A folder of files; the engine reads them and finds them by meaning.
    let d = tmpdir("meaning");
    touch(&d.join("letter from the bank.txt"), "Dear customer, your tax refund has been paid into your account. Keep this receipt.");
    touch(&d.join("summer.md"), "# Plans\n\nTwo weeks at the beach, flight on Friday, hotel near the sea.");
    touch(&d.join("Notes/dinner.txt"), "Pasta with tomato sauce: boil, stir, eat. A kitchen classic.");
    touch(&d.join("node_modules/skip.txt"), "invoice invoice invoice");
    touch(&d.join(".hidden/skip.txt"), "invoice invoice invoice");
    captioned_png(&d.join("IMG_0412.png"), "a puppy playing in the sand");
    // Pictures that aren't someone's: an icon, an app's artwork, frames cut from a video.
    sized_png(&d.join("app.png"), "icon", 64, 64);
    std::fs::create_dir_all(d.join("theme/icons")).unwrap();
    sized_png(&d.join("theme/icons/folder.png"), "icon", 256, 256);
    std::fs::create_dir_all(d.join("clip/frames")).unwrap();
    for k in 0..45 {
        sized_png(&d.join(format!("clip/frames/frame_{k:04}.png")), "a spreadsheet of numbers", 220, 220);
    }
    touch(&d.join("clip/frames/notes.txt"), "Frames cut from the holiday video at 30 per second.");
    // The same photo copied elsewhere: read once.
    std::fs::create_dir_all(d.join("Backup")).unwrap();
    std::fs::copy(d.join("IMG_0412.png"), d.join("Backup/IMG_0412.png")).unwrap();
    let _ = std::fs::remove_file(std::env::temp_dir().join("maia/meaning.idx"));
    let ctx = egui::Context::default();
    let start = embedded.load(Relaxed);
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20), "stuck at {:?}", engine.phase());
        std::thread::sleep(Duration::from_millis(10));
    }
    let idx = engine.index();
    assert_eq!(idx.files(), 6, "text and pictures; hidden and build folders, icons and video frames skipped");
    assert_eq!(embedded.load(Relaxed) - start, 5, "four texts and one picture: the copy shares its fingerprint");
    assert_eq!(PICTURE_SIDE.load(Relaxed), 512, "pictures shrunk before they go to the model");
    let best = |e: &Engine, q: &str| semantic::search(&e.index(), q, 3).unwrap().first().map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned());
    assert_eq!(best(&engine, "money back from the government").as_deref(), Some("letter from the bank.txt"), "no word in common, same topic");
    assert_eq!(best(&engine, "travel in summer").as_deref(), Some("summer.md"));
    assert_eq!(best(&engine, "cooking tonight").as_deref(), Some("dinner.txt"));
    assert_eq!(best(&engine, "photo of my dog").as_deref(), Some("IMG_0412.png"), "a picture by what it shows");
    let dogs = semantic::search(&engine.index(), "photo of my dog", 10).unwrap().iter().filter(|h| h.path.ends_with("IMG_0412.png")).count();
    assert_eq!(dogs, 1, "the copy in Backup isn't shown twice");
    let hit = &semantic::search(&engine.index(), "food", 1).unwrap()[0];
    assert!(hit.passage.starts_with("Pasta with tomato sauce"), "the passage shown: {:?}", hit.passage);
    // Saved, and on the next start only changed files are read again.
    let before = embedded.load(Relaxed);
    drop(engine);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(semantic::load(&std::env::temp_dir().join("maia/meaning.idx")).files(), 6, "saved to the cache");
    std::thread::sleep(Duration::from_secs(1));
    touch(&d.join("summer.md"), "# Plans\n\nWe changed our mind: a puppy instead of a trip. The dog needs a vet.");
    std::fs::remove_file(d.join("Notes/dinner.txt")).unwrap();
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(embedded.load(Relaxed) - before, 1, "only the changed file was read again");
    assert_eq!(engine.index().files(), 5, "the deleted file is gone from the index");
    assert_eq!(best(&engine, "summer plans changed for a pet").as_deref(), Some("summer.md"));
    drop(engine);
    // Pictures left out when "Include pictures" is off.
    let engine = Engine::start(vec![d.clone()], false, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(engine.index().files(), 3, "text only");
    drop(engine);
    // Code, data and vendored folders are left out: only writing is read. On battery a
    // new picture waits for the charger while that is still read.
    let body = (0..40).map(|k| format!("    // Step {k}: fetch the lyrics for the playing song and keep them in memory\n")).collect::<String>();
    touch(&d.join("lyrics.py"), &body);
    touch(&d.join("package-lock.json"), "{ \"name\": \"lyrics for the playing song and more words\" }");
    touch(&d.join("vendor/README.md"), "Lyrics for the playing song, fetched from a service and kept in memory for later.");
    touch(&d.join("song notes.md"), "Lyrics for the playing song, fetched from a service and kept in memory for later.");
    captioned_png(&d.join("IMG_0413.png"), "a puppy playing in the sand");
    semantic::TEST_BATTERY.store(true, Relaxed);
    let before = embedded.load(Relaxed);
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(embedded.load(Relaxed) - before, 1, "the note only: no code, data or vendored files");
    assert_eq!(engine.waiting(), 4, "pictures wait (the run with pictures off left them all out)");
    assert_eq!(engine.index().files(), 4, "the text is read meanwhile");
    semantic::TEST_BATTERY.store(false, Relaxed);
    engine.poke();
    let t0 = Instant::now();
    while engine.waiting() > 0 || engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(engine.index().files(), 7, "plugged in: the picture is read");
    drop(engine);
    // Without the model the engine waits for the user's say-so, then fetches it.
    let (addr2, model2, _) = fake_ollama(false);
    set_host(&addr2);
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::NoModel {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!model2.load(Relaxed), "nothing downloaded without asking");
    engine.download();
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", engine.phase());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(model2.load(Relaxed));
    drop(engine);
    *ollama::TEST_HOST.lock().unwrap() = None;
    assert_eq!(semantic::folders(" ~/Documents , /srv/notes,, "), vec![crate::fsx::home().join("Documents"), PathBuf::from("/srv/notes")]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn machine_named_pictures_told_from_photos() {
    use crate::semantic::machine_named;
    for n in ["frame_0042.png", "clip-000123.jpg", "000017.png", "3f9a1c0e-7b2d-4c1a.webp", "out.0001.png", "shot 004.png"] {
        assert!(machine_named(Path::new(n)), "{n}");
    }
    for n in ["IMG_1234.jpg", "PXL_20260912_101500.jpg", "DSC0042.JPG", "Screenshot_2026-10-15.png", "wallhaven-lygxjl.png", "Akeno.png", "17.png", "beach day.png"] {
        assert!(!machine_named(Path::new(n)), "{n}");
    }
}

#[test]
fn names_found_anywhere_under_home() {
    let d = tmpdir("names");
    for f in ["Notes/Read Me.md", "Notes/readme-old.txt", "Code/maia/README.md", "Code/maia/src/main.rs", "Code/maia/node_modules/readme.md", ".hidden/readme.md", "Photos/Été à Paris.jpg"] {
        touch(&d.join(f), "x");
    }
    let names = crate::namefind::Names::start(d.clone(), &egui::Context::default());
    let t0 = Instant::now();
    while names.walking() {
        assert!(t0.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    let find = |q: &str| names.find(q, 10).into_iter().map(|(p, _)| p.strip_prefix(&d).unwrap().display().to_string()).collect::<Vec<_>>();
    // Any case; the name that is just the word first; build and hidden folders left out.
    assert_eq!(find("readme"), vec!["Code/maia/README.md", "Notes/readme-old.txt"]);
    assert_eq!(find("read me")[0], "Notes/Read Me.md", "every word; the name that is just them first");
    assert_eq!(find("me read"), find("read me"), "in any order");
    // A word may be in a folder's name, as long as one is in the file's.
    assert_eq!(find("maia main"), vec!["Code/maia/src/main.rs"]);
    assert_eq!(find("maia"), vec!["Code/maia"], "folders too");
    assert_eq!(find("été"), vec!["Photos/Été à Paris.jpg"], "beyond ASCII");
    assert!(find("").is_empty() && find("nothing like it").is_empty());
    let _ = std::fs::remove_dir_all(&d);
}


/// Every theme reads well: its text on its list, and its code colours (the theme's own and
/// the classic set) on the preview's background, by the usual contrast measure.
#[test]
fn themes_and_their_code_colours_are_readable() {
    use crate::theme::{Syntax, ThemeName, palette};
    fn lum(c: egui::Color32) -> f32 {
        let f = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }
    let contrast = |a: egui::Color32, b: egui::Color32| {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    };
    let mut seen = std::collections::HashSet::new();
    for t in ThemeName::ALL {
        assert!(seen.insert(t.label()), "names differ");
        let p = palette(t);
        assert_eq!(p.dark, !t.is_light(), "{}", t.label());
        assert!(contrast(p.text, p.panel) >= 7.0, "{}: text {:.1}", t.label(), contrast(p.text, p.panel));
        assert!(contrast(p.text_dim, p.panel) >= 3.0, "{}: dim text {:.1}", t.label(), contrast(p.text_dim, p.panel));
        for s in [p.syntax, Syntax::classic(p.dark)] {
            for (what, c, min) in [("keyword", s.keyword, 3.0), ("type", s.kind, 3.0), ("string", s.string, 3.0), ("number", s.number, 3.0), ("comment", s.comment, 2.0)] {
                assert!(contrast(c, p.panel) >= min, "{}: {what} {:.2}", t.label(), contrast(c, p.panel));
            }
        }
    }
    assert_eq!(palette(ThemeName::TokyoNight).syntax, Syntax::of(ThemeName::TokyoNight));
    assert_ne!(Syntax::of(ThemeName::Gruvbox), Syntax::of(ThemeName::Mocha), "each its own");
}

/// A noreply address names its GitHub account: by number when it has one, else by name.
/// Anything else, or a malformed one, names nobody.
#[test]
fn github_noreply_addresses_name_the_picture() {
    use crate::avatars::{Source, noreply_url, source};
    assert_eq!(noreply_url("12345+octo@users.noreply.github.com").as_deref(), Some("https://avatars.githubusercontent.com/u/12345?s=96"));
    assert_eq!(noreply_url(" 12345+octo@Users.NoReply.GitHub.com ").as_deref(), Some("https://avatars.githubusercontent.com/u/12345?s=96"), "any case, trimmed");
    assert_eq!(noreply_url("octo-cat@users.noreply.github.com").as_deref(), Some("https://avatars.githubusercontent.com/octo-cat?s=96"));
    for none in ["octo@example.invalid", "octo@noreply.github.com", "octo@users.noreply.github.com.example.invalid", "x1+octo@users.noreply.github.com", "+octo@users.noreply.github.com", "-octo@users.noreply.github.com", "oc/to@users.noreply.github.com", "oc to@users.noreply.github.com", "@users.noreply.github.com", "users.noreply.github.com", ""] {
        assert_eq!(noreply_url(none), None, "{none:?}");
    }
    let repo = ("octo-org".to_string(), "demo".to_string());
    let sha = "0123456789abcdef0123456789abcdef01234567";
    // The cheapest way first: a noreply address needs no lookup, even on github.com.
    assert_eq!(source("12345+octo@users.noreply.github.com", sha, Some(&repo)), Some(Source::Picture("https://avatars.githubusercontent.com/u/12345?s=96".into())));
    assert_eq!(source("ada@example.invalid", sha, Some(&repo)), Some(Source::Api(format!("https://api.github.com/repos/octo-org/demo/commits?sha={sha}&per_page=1"))));
    assert_eq!(source("ada@example.invalid", sha, None), None, "not on github.com: no picture");
    assert_eq!(source("ada@example.invalid", "not a sha", Some(&repo)), None);
    assert_eq!(source("no address", sha, Some(&repo)), None);
}

/// Only a remote on github.com itself is looked up, in every form git writes it; an
/// Enterprise host or a look-alike is not github.com.
#[test]
fn github_remote_urls() {
    use crate::avatars::github_repo;
    let ok = Some(("octo-org".to_string(), "demo".to_string()));
    for u in [
        "https://github.com/octo-org/demo.git",
        "https://github.com/octo-org/demo",
        "https://github.com/octo-org/demo/",
        "http://github.com/octo-org/demo.git",
        "https://GitHub.com/octo-org/demo.git",
        "https://www.github.com/octo-org/demo",
        "https://someone:not-a-real-token@github.com/octo-org/demo.git",
        "git@github.com:octo-org/demo.git",
        "git@github.com:octo-org/demo",
        "github.com:octo-org/demo.git",
        "ssh://git@github.com/octo-org/demo.git",
        "ssh://git@ssh.github.com:443/octo-org/demo.git",
        "git://github.com/octo-org/demo.git",
        "  https://github.com/octo-org/demo.git\n",
    ] {
        assert_eq!(github_repo(u), ok, "{u:?}");
    }
    assert_eq!(github_repo("https://github.com/octo-org/my.site.git"), Some(("octo-org".into(), "my.site".into())));
    for u in [
        "https://github.example.com/octo-org/demo.git",
        "git@github.example.com:octo-org/demo.git",
        "ssh://git@ghe.example.invalid/octo-org/demo.git",
        "https://github.com.example.invalid/octo-org/demo.git",
        "https://notgithub.com/octo-org/demo.git",
        "https://gitlab.com/octo-org/demo.git",
        "https://example.invalid/github.com/octo-org/demo.git",
        "https://github.com@example.invalid/octo-org/demo.git",
        "file:///srv/github.com/octo-org/demo.git",
        "/srv/git/demo.git",
        "../demo.git",
        "https://github.com/octo-org",
        "https://github.com/octo-org/demo/tree/main",
        "https://github.com/../demo",
        "https://github.com/octo org/demo",
        "",
    ] {
        assert_eq!(github_repo(u), None, "{u:?}");
    }
}

/// GitHub's answer about a commit gives the account's picture at the size drawn, from
/// GitHub's picture host only; an author without an account is said to have none.
#[test]
fn github_api_answer_gives_the_picture() {
    use crate::avatars::{avatar_from_api, sized_avatar};
    let one = br#"{"sha":"0123abc","commit":{"author":{"name":"Octo"}},"author":{"login":"octo","id":12345,"avatar_url":"https://avatars.githubusercontent.com/u/12345?v=4"}}"#;
    assert_eq!(avatar_from_api(one), Some(Some("https://avatars.githubusercontent.com/u/12345?v=4&s=96".into())));
    let list = br#"[{"sha":"0123abc","author":{"avatar_url":"https://avatars.githubusercontent.com/u/777"}},{"sha":"9999"}]"#;
    assert_eq!(avatar_from_api(list), Some(Some("https://avatars.githubusercontent.com/u/777?s=96".into())), "the list form: its first commit");
    assert_eq!(avatar_from_api(br#"{"sha":"0123abc","author":null}"#), Some(None), "an address no account claims");
    assert_eq!(avatar_from_api(br#"[{"sha":"0123abc"}]"#), Some(None));
    assert_eq!(avatar_from_api(br#"{"sha":"0123abc","author":{"avatar_url":"https://example.invalid/me.png"}}"#), Some(None), "another host is not followed");
    assert_eq!(avatar_from_api(br#"{"sha":"0123abc","author":{"avatar_url":"http://avatars.githubusercontent.com/u/1"}}"#), Some(None), "https only");
    for bad in [&br#"{"message":"API rate limit exceeded"}"#[..], b"[]", b"not json", b"", b"null", br#""text""#] {
        assert_eq!(avatar_from_api(bad), None, "{:?}", String::from_utf8_lossy(bad));
    }
    assert_eq!(sized_avatar("https://avatars.githubusercontent.com/octo").as_deref(), Some("https://avatars.githubusercontent.com/octo?s=96"));
    assert_eq!(sized_avatar("https://avatars.githubusercontent.com/"), None);
    assert_eq!(sized_avatar("https://avatars.githubusercontent.com/u/1 --output x"), None);
}

/// An answer holds for a week, then a picture is shown while it is asked for again and a
/// "none" is forgotten. A clock set back doesn't make an answer last for ever.
#[test]
fn github_picture_cache_staleness() {
    use crate::avatars::{Cached, MAX_AGE, fresh, key, read_cache, write_hit, write_miss};
    use std::time::SystemTime;
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let day = Duration::from_secs(86_400);
    assert!(fresh(now, now) && fresh(now - 6 * day, now));
    assert!(!fresh(now - MAX_AGE, now) && !fresh(now - 30 * day, now));
    assert!(fresh(now + day, now), "a little ahead: the clock moved back a bit");
    assert!(!fresh(now + 8 * day, now), "far ahead: asked again");
    assert_eq!(key(" Ada@Example.invalid "), key("ada@example.invalid"), "one answer per address, whatever its case");
    assert_ne!(key("ada@example.invalid"), key("grace@example.invalid"));
    assert!(key("ada@example.invalid").bytes().all(|b| b.is_ascii_hexdigit()), "no address in a file name");
    let d = tmpdir("avatar-cache");
    let k = key("ada@example.invalid");
    let age = |name: &str, ago: Duration| std::fs::File::options().write(true).open(d.join(format!("{k}.{name}"))).unwrap().set_modified(SystemTime::now() - ago).unwrap();
    let now = SystemTime::now();
    assert_eq!(read_cache(&d, &k, now), Cached::Nothing);
    write_hit(&d, &k, b"picture");
    assert_eq!(read_cache(&d, &k, now), Cached::Hit(b"picture".to_vec()));
    age("img", 8 * day);
    assert_eq!(read_cache(&d, &k, now), Cached::Stale(b"picture".to_vec()));
    write_miss(&d, &k);
    assert_eq!(read_cache(&d, &k, now), Cached::Miss, "none, this week");
    assert!(!d.join(format!("{k}.img")).exists(), "the old picture goes with it");
    age("none", 8 * day);
    assert_eq!(read_cache(&d, &k, now), Cached::Nothing, "a week on, asked again");
    write_hit(&d, &k, b"new");
    assert!(!d.join(format!("{k}.none")).exists());
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no temporary file left behind");
    let _ = std::fs::remove_dir_all(&d);
}

/// One author's picture end to end against a stand-in for GitHub: fetched once, kept,
/// then read from the cache; a "none" kept the same way; too many requests and errors
/// keep nothing; a week-old picture stands in when GitHub can't be asked.
#[test]
fn github_find_asks_once_and_keeps_the_answer() {
    use crate::avatars::{Found, Http, Source, find, key, write_hit};
    use std::cell::RefCell;
    let d = tmpdir("avatar-find");
    let png = png_bytes(120, 80, 4);
    let now = std::time::SystemTime::now();
    let asked = RefCell::new(vec![]);
    let get = |answers: Vec<(&'static str, Http)>| {
        let asked = &asked;
        move |url: &str| {
            asked.borrow_mut().push(url.to_string());
            answers.iter().find(|(k, _)| url.contains(k)).map_or(Http::Failed, |(_, h)| h.clone())
        }
    };
    let pic = Source::Picture("https://avatars.githubusercontent.com/u/12345?s=96".into());
    let k = key("12345+octo@users.noreply.github.com");
    let g = get(vec![("/u/12345", Http::Ok(png.clone()))]);
    assert!(matches!(find(&d, &k, &pic, &g, now), Found::Picture(i) if (i.w, i.h) == (120, 80)));
    assert!(matches!(find(&d, &k, &pic, &g, now), Found::Picture(_)));
    assert_eq!(asked.borrow().len(), 1, "the second time from the cache");
    // Through the API: the commit's account, then its picture.
    let api = Source::Api("https://api.github.com/repos/octo-org/demo/commits?sha=0123abc&per_page=1".into());
    let json = br#"[{"sha":"0123abc","author":{"avatar_url":"https://avatars.githubusercontent.com/u/777?v=4"}}]"#.to_vec();
    asked.borrow_mut().clear();
    let g = get(vec![("api.github.com", Http::Ok(json)), ("/u/777?v=4&s=96", Http::Ok(png.clone()))]);
    assert!(matches!(find(&d, &key("grace@example.invalid"), &api, &g, now), Found::Picture(_)));
    assert_eq!(asked.borrow().len(), 2);
    // No account behind the address: kept as none, not asked again.
    asked.borrow_mut().clear();
    let g = get(vec![("api.github.com", Http::Ok(br#"{"sha":"0123abc","author":null}"#.to_vec()))]);
    assert!(matches!(find(&d, &key("linus@example.invalid"), &api, &g, now), Found::None));
    assert!(matches!(find(&d, &key("linus@example.invalid"), &api, &g, now), Found::None));
    assert_eq!(asked.borrow().len(), 1);
    // Too many requests, or an error: nothing kept, so it is asked again later.
    for (h, limited) in [(Http::Status(403), true), (Http::Status(429), true), (Http::Status(500), false), (Http::Failed, false)] {
        let g = get(vec![("api.github.com", h)]);
        let f = find(&d, &key("edsger@example.invalid"), &api, &g, now);
        assert!(if limited { matches!(f, Found::Limited) } else { matches!(f, Found::Failed) });
    }
    assert!(!std::fs::read_dir(&d).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with(&key("edsger@example.invalid"))));
    // A commit GitHub doesn't have: none, for a week.
    let g = get(vec![("api.github.com", Http::Status(422))]);
    assert!(matches!(find(&d, &key("barbara@example.invalid"), &api, &g, now), Found::None));
    // A picture that doesn't decode is not kept.
    let g = get(vec![("/u/12345", Http::Ok(b"<html>".to_vec()))]);
    assert!(matches!(find(&d, &key("12345+other@users.noreply.github.com"), &pic, &g, now), Found::Failed));
    // A week-old picture while GitHub can't be reached, or limits us.
    let ks = key("13+old@users.noreply.github.com");
    write_hit(&d, &ks, &png);
    std::fs::File::options().write(true).open(d.join(format!("{ks}.img"))).unwrap().set_modified(now - Duration::from_secs(8 * 86_400)).unwrap();
    asked.borrow_mut().clear();
    let g = get(vec![]);
    assert!(matches!(find(&d, &ks, &pic, &g, now), Found::Picture(_)));
    assert_eq!(asked.borrow().len(), 1, "a stale picture is asked for again");
    let _ = std::fs::remove_dir_all(&d);
}

/// curl's answer splits into body and status; the command reads no user settings, speaks
/// only https, caps the answer and the time, and names itself plainly.
#[test]
fn github_requests_through_curl() {
    use crate::avatars::{Http, curl_args, parse_curl};
    assert_eq!(parse_curl(b"\x89PNG\n\x00data\n200"), Http::Ok(b"\x89PNG\n\x00data".to_vec()));
    assert_eq!(parse_curl(b"{\"message\":\"rate\"}\n403"), Http::Status(403));
    assert_eq!(parse_curl(b"\n000"), Http::Failed, "no answer at all");
    assert_eq!(parse_curl(b""), Http::Failed);
    assert_eq!(parse_curl(b"cut short"), Http::Failed);
    let a = curl_args("https://avatars.githubusercontent.com/u/12345?s=96");
    assert_eq!(a[0], "-q", "first, or ~/.curlrc is read");
    let has = |a: &[String], k: &str, v: &str| a.windows(2).any(|w| w[0] == k && w[1] == v);
    assert!(has(&a, "--proto", "=https") && has(&a, "--proto-redir", "=https") && a.iter().any(|x| x == "--location"));
    assert!(has(&a, "--max-time", "10") && has(&a, "--max-filesize", "1048576") && has(&a, "--user-agent", "Maia"));
    assert!(!a.iter().any(|x| matches!(x.as_str(), "-u" | "--user" | "-n" | "--netrc" | "--netrc-optional" | "-b" | "--cookie" | "-K" | "--config")), "no credentials of any kind");
    assert!(!a.iter().any(|x| x.to_ascii_lowercase().contains("authorization")));
    assert_eq!(a.last().map(String::as_str), Some("https://avatars.githubusercontent.com/u/12345?s=96"));
    assert!(curl_args("https://api.github.com/repos/o/r/commits?sha=0123abc&per_page=1").iter().any(|x| x == "Accept: application/vnd.github+json"));
    assert!(!a.iter().any(|x| x.starts_with("Accept")), "pictures need no API header");
}

/// A picture is cut to the middle square at the size drawn, round, with a soft edge.
#[test]
fn github_picture_is_cut_round() {
    let img = crate::imgdec::decode_bytes(&png_bytes(120, 80, 2)).unwrap();
    let c = crate::avatars::round(&img, 37);
    assert_eq!(c.size, [37, 37]);
    let a = |x: usize, y: usize| c.pixels[y * 37 + x].a();
    assert_eq!((a(0, 0), a(36, 0), a(0, 36), a(36, 36)), (0, 0, 0, 0), "corners outside the circle");
    assert_eq!(a(18, 18), 255);
    assert!((1..255).contains(&a(18, 0)) || a(18, 0) == 255, "the top edge is inside or soft");
    assert!(c.pixels.iter().any(|p| (1..255).contains(&p.a())), "a soft edge, not a jagged one");
    let small = crate::imgdec::decode_bytes(&png_bytes(20, 20, 2)).unwrap();
    assert_eq!(crate::avatars::round(&small, 96).size, [20, 20], "a small picture is not blown up here");
}

/// Every option is listed in Options exactly once, in one section, with its group's
/// rows together and each sub-option right under the switch it belongs to.
#[test]
fn every_option_has_one_section() {
    use crate::popups::{OPTS, Opt, Section, opt_parent};
    // Exhaustive: a new option does not build until it is numbered here.
    let index = |o: Opt| match o {
        Opt::Theme => 0, Opt::Font => 1, Opt::InspectorFontSize => 2, Opt::RectButtons => 3, Opt::AssetCache => 4,
        Opt::FollowSystem => 5, Opt::SyntaxTheme => 6, Opt::Round => 7, Opt::FontSize => 8, Opt::Spacing => 9,
        Opt::Animations => 10, Opt::Tooltips => 11, Opt::AltRows => 12, Opt::InactiveOpacity => 13, Opt::Breadcrumbs => 14,
        Opt::CompactPaths => 15, Opt::ShowExt => 16, Opt::ShowHidden => 17, Opt::ShowSystem => 18, Opt::FoldersFirst => 19,
        Opt::HighlightRecents => 20, Opt::RecentsOnTop => 21, Opt::TrackRecents => 22, Opt::CalcSizes => 23, Opt::SizesSsd => 24,
        Opt::SizesHdd => 25, Opt::SizesNet => 26, Opt::SelectionBoxes => 27, Opt::InspectorFont => 28, Opt::SystemIcons => 29,
        Opt::Thumbnails => 30, Opt::ThumbFill => 31, Opt::ScrollBeyond => 32, Opt::DateFormat => 33, Opt::ActOnPress => 34,
        Opt::SingleClick => 35, Opt::SingleClickFolders => 36, Opt::OpenFoldersNewTab => 37, Opt::DblClickParent => 38, Opt::ReverseScroll => 39,
        Opt::CharKey => 40, Opt::MiddleClick => 41, Opt::InspectorLayout => 42, Opt::Startup => 43, Opt::NewTab => 44,
        Opt::GitPictures => 45, Opt::DefaultFm => 46, Opt::SingleInstance => 47, Opt::Meaning => 48, Opt::MeaningFolders => 49,
        Opt::MeaningPictures => 50, Opt::ConfigFile => 51, Opt::ResetAll => 52, Opt::Install => 53, Opt::LineSpacing => 54, Opt::Updates => 55,
    };
    let mut seen = vec![0; 56];
    for o in OPTS {
        seen[index(o.0)] += 1;
    }
    for (i, n) in seen.iter().enumerate() {
        assert_eq!(*n, 1, "option {i} listed {n} times");
    }
    // Sections in the order of the left column, each a block; groups never split.
    let order: Vec<Section> = OPTS.iter().map(|o| o.1).fold(vec![], |mut v, s| {
        if v.last() != Some(&s) {
            v.push(s);
        }
        v
    });
    assert_eq!(order, Section::ALL.to_vec(), "every section has options, in one block, in order");
    let mut groups: Vec<(Section, &str)> = vec![];
    for o in OPTS {
        if groups.last() != Some(&(o.1, o.2)) {
            assert!(!groups.contains(&(o.1, o.2)), "{} / {:?} is split", o.1.name(), o.2);
            groups.push((o.1, o.2));
        }
    }
    for (k, o) in OPTS.iter().enumerate() {
        if let Some(p) = opt_parent(o.0) {
            let at = OPTS.iter().position(|x| x.0 == p).unwrap();
            assert!(at < k && OPTS[at + 1..k].iter().all(|x| opt_parent(x.0) == Some(p)), "{:?} sits right under its switch", o.3);
            assert_eq!((OPTS[at].1, OPTS[at].2), (o.1, o.2), "{:?} shares its switch's group", o.3);
        }
    }
    // Short, plain lines.
    for o in OPTS {
        assert!(o.4.len() <= 60 && !o.4.contains('\u{2014}'), "{:?}: {:?} is long", o.3, o.4);
        assert!(o.4.ends_with('.'), "{:?}", o.4);
    }
}

/// The search covers every section: the words people type find their option first,
/// and a word shared by options in several sections finds each of them.
#[test]
fn option_search_spans_sections() {
    use crate::popups::{OPTS, Opt, Section, find_options};
    let first = |q: &str| find_options(q).first().map(|&i| OPTS[i].0);
    for (q, o) in [
        ("color scheme", Opt::Theme),
        ("default file manager", Opt::DefaultFm),
        ("search by meaning", Opt::Meaning),
        ("reuse the open window", Opt::SingleInstance),
        ("show github pictures", Opt::GitPictures),
        ("hidden", Opt::ShowHidden),
        ("folders first", Opt::FoldersFirst),
        ("install", Opt::Install),
        ("config", Opt::ConfigFile),
    ] {
        assert_eq!(first(q), Some(o), "{q:?}");
    }
    assert!(find_options("").is_empty() && find_options("   ").is_empty(), "no search, no results");
    assert!(find_options("qqzzx").is_empty());
    let sections = |q: &str| find_options(q).iter().map(|&i| OPTS[i].1).collect::<HashSet<Section>>();
    let size = sections("size");
    for s in [Section::Appearance, Section::Files, Section::Preview, Section::System] {
        assert!(size.contains(&s), "\"size\" reaches {}", s.name());
    }
    assert!(sections("font").is_superset(&[Section::Appearance, Section::Preview].into_iter().collect()));
    // A section or group name finds what is in it.
    assert!(find_options("thumbnails").iter().any(|&i| OPTS[i].0 == Opt::ThumbFill));
    assert!(find_options("recents").iter().any(|&i| OPTS[i].0 == Opt::TrackRecents));
    assert_eq!(Section::Appearance.step(-1), Section::System, "sections wrap");
    assert_eq!(Section::System.step(1), Section::Appearance);
    assert_eq!(Section::Files.step(1), Section::Mouse);
}

/// Updates: versions compared as numbers, GitHub's answer read only for this project's
/// downloads, and an install that checks the download before it replaces anything.
#[test]
fn updates_check_compare_and_install() {
    use crate::update::{Fetch, Release, State, Updater, curl_args, newer, parse_release, parse_sum, version};
    assert_eq!(version("v1.2.3"), Some((1, 2, 3)));
    assert_eq!(version("0.10"), Some((0, 10, 0)));
    assert_eq!(version("2.0.0-beta.1"), Some((2, 0, 0)));
    assert_eq!(version("one"), None);
    assert_eq!(version("1.2.3.4"), None);
    assert!(newer("0.2.0", "v0.10.0"), "numbers, not text");
    assert!(!newer("0.2.0", "v0.2.0") && !newer("0.3.0", "0.2.9") && !newer("0.2.0", "garbage"));
    let json = |tag: &str, host: &str| format!(r#"{{"tag_name":"{tag}","body":"Faster search.\n\nNew themes.","assets":[
        {{"name":"maia-linux-x86_64","browser_download_url":"{host}/yappologistic/Maia/releases/download/{tag}/maia-linux-x86_64"}},
        {{"name":"maia-linux-x86_64.sha256","browser_download_url":"{host}/yappologistic/Maia/releases/download/{tag}/maia-linux-x86_64.sha256"}}]}}"#);
    let r = parse_release(json("v9.9.9", "https://github.com").as_bytes()).expect("a release");
    assert_eq!((r.version.as_str(), r.notes.starts_with("Faster search.")), ("v9.9.9", true));
    assert!(parse_release(json("v9.9.9", "https://evil.example").as_bytes()).is_none(), "downloads only from the project's releases");
    assert!(parse_release(br#"{"tag_name":"v9.9.9","assets":[]}"#).is_none(), "no build, no update");
    let h = "a".repeat(64);
    assert_eq!(parse_sum(&format!("{h}  maia-linux-x86_64\n")), Some(h.clone()));
    assert_eq!(parse_sum("not a sum"), None);
    let args = curl_args("https://api.github.com/repos/x/y/releases/latest", None);
    assert_eq!(args[0], "-q", "no ~/.curlrc");
    assert!(args.windows(2).any(|w| w == ["--proto", "=https"]) && args.iter().any(|a| a.starts_with("Accept:")));
    assert!(args.iter().all(|a| !a.to_lowercase().contains("authorization")), "no credentials");
    // An install against a stand-in for GitHub: the right bytes replace the program.
    let d = tmpdir("update");
    let exe = d.join("maia");
    std::fs::write(&exe, b"\x7fELF old").unwrap();
    let build = b"\x7fELF the new build".to_vec();
    let sum = |b: &[u8]| {
        let f = d.join("sum-input");
        std::fs::write(&f, b).unwrap();
        let out = std::process::Command::new("sha256sum").arg(&f).output().unwrap();
        String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().to_string()
    };
    let serve = |bin: Vec<u8>, sum_text: String| -> Fetch {
        let api = json("v9.9.9", "https://github.com");
        std::sync::Arc::new(move |url: &str, to: Option<&Path>| {
            let body = if url.contains("api.github.com") { api.clone().into_bytes() } else if url.ends_with(".sha256") { sum_text.clone().into_bytes() } else { bin.clone() };
            match to {
                Some(p) => std::fs::write(p, &body).map(|_| vec![]).map_err(|e| e.to_string()),
                None => Ok(body),
            }
        })
    };
    let wait = |u: &Updater| {
        let t0 = Instant::now();
        while matches!(u.state(), State::Checking | State::Downloading(_)) {
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        u.state()
    };
    let ctx = egui::Context::default();
    let u = Updater::for_test(serve(build.clone(), format!("{}  maia-linux-x86_64", sum(&build))), exe.clone());
    u.check(&ctx);
    let State::Available(r) = wait(&u) else { panic!("{:?}", u.state()) };
    u.install(r.clone(), &ctx);
    assert_eq!(wait(&u), State::Ready("v9.9.9".into()));
    assert_eq!(std::fs::read(&exe).unwrap(), build, "the new build in place");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755, "and runnable");
    // A download that doesn't match its checksum changes nothing, and leaves nothing.
    std::fs::write(&exe, b"\x7fELF old").unwrap();
    let u = Updater::for_test(serve(b"\x7fELF tampered".to_vec(), format!("{}  maia-linux-x86_64", sum(&build))), exe.clone());
    u.install(r.clone(), &ctx);
    assert!(matches!(wait(&u), State::Failed(e) if e.contains("checksum")));
    assert_eq!(std::fs::read(&exe).unwrap(), b"\x7fELF old");
    assert!(std::fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".maia-update")), "no download left behind");
    // A program in a folder Maia can't write: said so, nothing fetched.
    let u = Updater::for_test(serve(build.clone(), String::new()), PathBuf::from("/proc/maia"));
    u.install(r, &ctx);
    assert!(matches!(u.state(), State::CantWrite(_)));
    let _ = std::fs::remove_dir_all(&d);
}
