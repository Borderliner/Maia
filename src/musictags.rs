//! Music tags: title, artist, album, album artist, track, year, genre and the cover of
//! sound files, read through ffprobe and written through ffmpeg, which copies the sound
//! as it is. Each file is rewritten to a part file that takes its place; the original
//! goes to the trash first, so Undo brings it back.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use egui::{FontId, Id, Sense, pos2, vec2};

use crate::app::App;
use crate::drives::Tools;
use crate::popups::{self as pp, Popup};

/// The tags edited, in the dialog's order: (ffmpeg's key, label, other spellings files
/// use for it).
pub const FIELDS: [(&str, &str, &[&str]); 7] = [
    ("title", "Title", &[]),
    ("artist", "Artist", &[]),
    ("album", "Album", &[]),
    ("album_artist", "Album artist", &["albumartist", "album artist"]),
    ("track", "Track", &["tracknumber"]),
    ("date", "Year", &["year"]),
    ("genre", "Genre", &[]),
];

/// The sound files whose tags ffmpeg writes back.
pub fn taggable(ext: &str) -> bool {
    matches!(ext, "mp3" | "flac" | "m4a" | "ogg" | "oga" | "opus")
}

/// Formats that can carry a cover picture.
pub fn holds_cover(ext: &str) -> bool {
    matches!(ext, "mp3" | "flac" | "m4a")
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tags {
    /// One value per `FIELDS` entry (empty: not set).
    pub values: [String; 7],
    pub cover: bool,
}

impl Tags {
    pub fn get(&self, key: &str) -> &str {
        FIELDS.iter().position(|f| f.0 == key).map(|i| self.values[i].as_str()).unwrap_or("")
    }
}

/// `ffprobe -of flat` output of the format's and streams' tags and the streams'
/// kinds: the tags under any of their spellings, any case, and whether a cover is there.
pub fn parse(flat: &str) -> Tags {
    let mut t = Tags::default();
    for line in flat.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v).replace("\\\"", "\"").replace("\\\\", "\\");
        if k.ends_with(".disposition.attached_pic") && v == "1" {
            t.cover = true;
            continue;
        }
        let Some(tag) = k.strip_prefix("format.tags.").or_else(|| k.strip_prefix("streams.stream.0.tags.")) else { continue };
        let tag = tag.to_lowercase();
        if let Some(i) = FIELDS.iter().position(|f| f.0 == tag || f.2.contains(&tag.as_str())) {
            if t.values[i].is_empty() {
                t.values[i] = v.trim().to_string();
            }
        }
    }
    t
}

pub fn read(t: &Tools, path: &Path) -> Result<Tags, String> {
    let mut c = t.cmd("ffprobe");
    c.args(["-v", "error", "-show_entries", "format_tags:stream_tags:stream_disposition=attached_pic", "-of", "flat"]).arg(file_arg(path));
    let out = crate::errands::output(c, Duration::from_secs(20))?;
    Ok(parse(&String::from_utf8_lossy(&out)))
}

fn file_arg(p: &Path) -> std::ffi::OsString {
    let mut s = std::ffi::OsString::from("file:");
    s.push(p);
    s
}

/// What happens to the cover.
#[derive(Clone, Debug, PartialEq)]
pub enum Cover {
    Keep,
    Set(PathBuf),
    Remove,
}

/// ffmpeg's arguments after the inputs for these changes (field index, new value).
pub fn write_args(ext: &str, changes: &[(usize, String)], cover: &Cover) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    let cover = if holds_cover(ext) { cover } else { &Cover::Keep };
    match cover {
        Cover::Keep => a.extend(["-map", "0"].map(String::from)),
        Cover::Remove => a.extend(["-map", "0:a"].map(String::from)),
        Cover::Set(_) => a.extend(["-map", "0:a", "-map", "1"].map(String::from)),
    }
    a.extend(["-c", "copy"].map(String::from));
    if matches!(cover, Cover::Set(_)) {
        a.extend(["-disposition:v:0", "attached_pic"].map(String::from));
    }
    // Ogg keeps its tags with the stream, not the file.
    let key = if matches!(ext, "ogg" | "oga" | "opus") { "-metadata:s:a:0" } else { "-metadata" };
    for (i, v) in changes {
        a.push(key.into());
        a.push(format!("{}={v}", FIELDS[*i].0));
    }
    let fmt = match ext {
        "mp3" => "mp3",
        "flac" => "flac",
        "m4a" => "ipod",
        "opus" => "opus",
        _ => "ogg",
    };
    if fmt == "mp3" {
        a.extend(["-id3v2_version", "3"].map(String::from));
    }
    a.extend(["-f".to_string(), fmt.to_string()]);
    a
}

/// Write the changes into `path`: through a part file beside it, the original to the
/// trash once the new one is whole.
pub fn write(t: &Tools, path: &Path, changes: &[(usize, String)], cover: &Cover) -> Result<(), String> {
    let ext = ext_of(path);
    if !taggable(&ext) {
        return Err("Maia can't write tags into this kind of file".into());
    }
    let part = path.with_file_name(format!(".{}.part", name_of(path)));
    let mut c = t.cmd("ffmpeg");
    c.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(file_arg(path));
    if let Cover::Set(pic) = cover {
        if holds_cover(&ext) {
            c.arg("-i").arg(file_arg(pic));
        }
    }
    c.args(write_args(&ext, changes, cover)).arg(file_arg(&part));
    let r = crate::errands::output(c, Duration::from_secs(300)).and_then(|_| {
        if std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) == 0 {
            return Err("ffmpeg wrote nothing".to_string());
        }
        if let Ok(m) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&part, m.permissions());
        }
        crate::trashcan::delete(path).map_err(|e| e.to_string())?;
        std::fs::rename(&part, path).map_err(|e| e.to_string())
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    r
}

// ------------------------------------------------------------------ the dialog

pub struct State {
    pub files: Vec<PathBuf>,
    /// A picture selected with them, offered as the cover.
    pub picture: Option<PathBuf>,
    /// What each file holds now, once read.
    pub read: Option<Result<Vec<Tags>, String>>,
    rx: Option<Receiver<Result<Vec<Tags>, String>>>,
    /// What is typed into each field; None leaves the files' own values alone.
    pub typed: [Option<String>; 7],
    pub cover: Cover,
    focus: bool,
    tools: Tools,
}

impl State {
    /// The value every file shares for field `i`, or None when they differ.
    pub fn shared(&self, i: usize) -> Option<String> {
        let Some(Ok(all)) = &self.read else { return None };
        let first = all.first()?.values[i].clone();
        all.iter().all(|t| t.values[i] == first).then_some(first)
    }

    /// The changes to make: fields typed into that differ from what some file has.
    pub fn changes(&self) -> Vec<(usize, String)> {
        let Some(Ok(all)) = &self.read else { return vec![] };
        (0..FIELDS.len()).filter_map(|i| self.typed[i].as_ref().filter(|v| all.iter().any(|t| t.values[i] != v.trim())).map(|v| (i, v.trim().to_string()))).collect()
    }
}

/// Edit Tags for the selection: sound files ffmpeg can write, and perhaps one picture to
/// make their cover.
pub fn open(app: &mut App) {
    let t = app.tab();
    if t.archive.is_some() || t.path.starts_with(crate::sidebar::trash_dir()) {
        app.toast("Extract or restore them first");
        return;
    }
    let ents = t.target_entries();
    let files: Vec<PathBuf> = ents.iter().filter(|e| !e.is_dir && taggable(e.ext())).map(|e| e.path.clone()).collect();
    let pics: Vec<PathBuf> = ents.iter().filter(|e| !e.is_dir && matches!(e.ext(), "jpg" | "jpeg" | "png")).map(|e| e.path.clone()).collect();
    if files.is_empty() || files.len() + pics.len() != ents.len() || pics.len() > 1 {
        app.toast("Select MP3, FLAC, M4A, Ogg or Opus files (and a picture for their cover)");
        return;
    }
    let Some(tools) = Tools::current().filter(|t| t.has("ffmpeg") && t.has("ffprobe")) else {
        app.toast("Editing tags needs ffmpeg");
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let (list, t2, ctx) = (files.clone(), tools.clone(), app.ctx.clone());
    std::thread::spawn(move || {
        let r: Result<Vec<Tags>, String> = list.iter().map(|p| read(&t2, p).map_err(|e| format!("{}: {e}", name_of(p)))).collect();
        let _ = tx.send(r);
        ctx.request_repaint();
    });
    let picture = pics.into_iter().next();
    let cover = picture.clone().map_or(Cover::Keep, Cover::Set);
    app.popup = Popup::MusicTags(Box::new(State { files, picture, read: None, rx: Some(rx), typed: Default::default(), cover, focus: true, tools }));
}

/// Whether Edit Tags fits these (extension, is a folder) items.
pub fn applies(items: &[(&str, bool)]) -> bool {
    let sound = items.iter().filter(|(x, d)| !d && taggable(x)).count();
    let pics = items.iter().filter(|(x, d)| !d && matches!(*x, "jpg" | "jpeg" | "png")).count();
    sound > 0 && pics <= 1 && sound + pics == items.len() && Tools::current().is_some_and(|t| t.has("ffmpeg") && t.has("ffprobe"))
}

pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    if let Some(rx) = &st.rx {
        if let Ok(r) = rx.try_recv() {
            st.read = Some(r);
            st.rx = None;
        }
    }
    let pal = app.pal;
    let k = pp::take_keys(ctx);
    if k.esc {
        return false;
    }
    let fs = app.cfg.font_size;
    let changes = st.changes();
    let ready = matches!(st.read, Some(Ok(_)));
    let ok = ready && (!changes.is_empty() || st.cover != Cover::Keep);
    let mut go = k.enter && ok;
    let mut cancel = false;
    let n = st.files.len();
    let title = match st.files.as_slice() {
        [one] => format!("Tags of {}", name_of(one)),
        _ => format!("Tags of {n} files"),
    };
    pp::centered(ctx, "music tags", 520.0, &pal, |ui| {
        pp::dialog_title(ui, &title, None, &pal);
        ui.add_space(8.0);
        let label_w = (fs * 7.0).round();
        match &st.read {
            None => {
                ui.label(egui::RichText::new("Reading the tags…").color(pal.text_dim));
            }
            Some(Err(e)) => {
                ui.label(egui::RichText::new(format!("Can't read the tags: {e}")).color(pal.danger));
            }
            Some(Ok(_)) => {
                for i in 0..FIELDS.len() {
                    let shared = st.shared(i);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(label_w, fs * 1.6), Sense::hover());
                        ui.painter().text(pos2(r.left(), r.center().y), egui::Align2::LEFT_CENTER, FIELDS[i].1, FontId::proportional(fs - 0.5), pal.text_dim);
                        // Typed or not, the field shows the files' own value until changed;
                        // values that differ show as Mixed and stay unless typed over.
                        let mut text = st.typed[i].clone().unwrap_or_else(|| shared.clone().unwrap_or_default());
                        let hint = if shared.is_none() { "Mixed" } else { "" };
                        let r = ui.add(egui::TextEdit::singleline(&mut text).id(Id::new(("music tags", i))).hint_text(egui::RichText::new(hint).color(pal.text_dim).italics()).desired_width(ui.available_width()).margin(egui::Margin::symmetric(6, 3)));
                        crate::ui::note_control(ui, r.rect, ["tag title", "tag artist", "tag album", "tag album artist", "tag track", "tag year", "tag genre"][i]);
                        if st.focus && i == 0 {
                            r.request_focus();
                            st.focus = false;
                        }
                        if r.changed() {
                            st.typed[i] = Some(text);
                        }
                    });
                }
                let covers = st.files.iter().any(|f| holds_cover(&ext_of(f)));
                if covers {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(label_w, fs * 1.6), Sense::hover());
                        ui.painter().text(pos2(r.left(), r.center().y), egui::Align2::LEFT_CENTER, "Cover", FontId::proportional(fs - 0.5), pal.text_dim);
                        if crate::devtools::chip(ui, "Keep", st.cover == Cover::Keep, "cover keep", "Leave each file's cover as it is", &pal, false) {
                            st.cover = Cover::Keep;
                        }
                        if let Some(p) = st.picture.clone() {
                            let on = st.cover == Cover::Set(p.clone());
                            let l = format!("Use {}", name_of(&p));
                            if crate::devtools::chip(ui, &l, on, "cover set", "The picture selected with them", &pal, false) {
                                st.cover = Cover::Set(p);
                            }
                        }
                        if crate::devtools::chip(ui, "Remove", st.cover == Cover::Remove, "cover remove", "Take the cover out", &pal, false) {
                            st.cover = Cover::Remove;
                        }
                    });
                }
            }
        }
        ui.add_space(6.0);
        pp::hints(ui, &[("Enter", "save"), ("Esc", "close")], &pal);
        pp::dialog_buttons(ui, |ui| {
            let b = pp::primary_button(ui, "Save", ok, &pal);
            crate::ui::note_control(ui, b.rect, "tags save");
            go |= b.clicked() && ok;
            let c = pp::plain_button(ui, "Cancel", &pal);
            crate::ui::note_control(ui, c.rect, "tags cancel");
            cancel = c.clicked();
        });
    });
    if cancel {
        return false;
    }
    if go {
        let (files, cover, tools) = (st.files.clone(), st.cover.clone(), st.tools.clone());
        let ctx = app.ctx.clone();
        app.errands.run(&ctx, move || {
            let (mut done, mut err) = (vec![], None);
            for f in &files {
                match write(&tools, f, &changes, &cover) {
                    Ok(()) => done.push(f.clone()),
                    Err(e) => err = Some(format!("Couldn't tag {}: {e}", name_of(f))),
                }
            }
            let what = match done.as_slice() {
                [one] => name_of(one),
                d => format!("{} files", d.len()),
            };
            let toast = err.unwrap_or_else(|| format!("Saved the tags of {what}"));
            let undo = (!done.is_empty()).then(|| crate::app::UndoEntry { label: format!("Tag {what}"), kind: crate::app::UndoKind::Replaced(done.clone()) });
            crate::errands::Done { toast, undo, changed: done, select: None }
        });
        return false;
    }
    true
}

// ------------------------------------------------------------------ for Batch Rename

/// The tags Batch Rename can put in names, read for the files that have them.
pub fn for_rename(t: &Tools, files: &[PathBuf]) -> Vec<Option<Tags>> {
    files.iter().map(|f| taggable_or_wav(&ext_of(f)).then(|| read(t, f).ok()).flatten()).collect()
}

fn taggable_or_wav(ext: &str) -> bool {
    taggable(ext) || matches!(ext, "wav" | "wma" | "aac" | "aiff")
}

/// "3/12" as "03": a track number padded for names that sort.
pub fn track_name(v: &str) -> String {
    let n = v.split('/').next().unwrap_or("").trim();
    match n.parse::<u32>() {
        Ok(k) => format!("{k:02}"),
        Err(_) => n.to_string(),
    }
}
