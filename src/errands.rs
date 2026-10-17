//! Short jobs a command hands to another program: setting the wallpaper, printing,
//! fixing a picture. They run one after another on a thread of their own (two edits of
//! the same picture never overlap) and report back to the frame.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// What a finished job hands back.
#[derive(Default)]
pub struct Done {
    pub toast: String,
    /// A change Undo can take back.
    pub undo: Option<crate::app::UndoEntry>,
    /// Files written in place: their thumbnails and previews are made again.
    pub changed: Vec<PathBuf>,
    /// A new file to select.
    pub select: Option<PathBuf>,
}

impl Done {
    pub fn say(toast: impl Into<String>) -> Done {
        Done { toast: toast.into(), ..Default::default() }
    }
}

type Job = Box<dyn FnOnce() -> Done + Send>;

/// The thread starts with the first job, so a session that never uses one costs nothing.
#[derive(Default)]
pub struct Runner {
    tx: Option<Sender<Job>>,
    rx: Option<Receiver<Done>>,
    /// Jobs given and not yet reported.
    pub busy: usize,
}

impl Runner {
    pub fn run(&mut self, ctx: &egui::Context, job: impl FnOnce() -> Done + Send + 'static) {
        if self.tx.is_none() {
            let (tx, jobs) = channel::<Job>();
            let (done, rx) = channel();
            let ctx = ctx.clone();
            let ok = std::thread::Builder::new().name("errands".into()).spawn(move || {
                for j in jobs {
                    if done.send(j()).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
            if ok.is_err() {
                return;
            }
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        if self.tx.as_ref().is_some_and(|t| t.send(Box::new(job)).is_ok()) {
            self.busy += 1;
        }
    }

    pub fn poll(&mut self) -> Option<Done> {
        let d = self.rx.as_ref()?.try_recv().ok()?;
        self.busy = self.busy.saturating_sub(1);
        Some(d)
    }
}

/// Run a program to its end and return what it printed, or the first line of what it
/// said went wrong. Killed after `timeout`; dies with Maia.
pub fn output(c: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    output_io(c, timeout, None, None)
}

/// `output`, reading `input` and with what it prints written to `out` instead.
pub fn output_io(c: Command, timeout: Duration, input: Option<std::fs::File>, out: Option<std::fs::File>) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut c = crate::media::tool_cmd(c);
    c.stderr(Stdio::piped());
    if let Some(f) = input {
        c.stdin(Stdio::from(f));
    }
    match out {
        Some(f) => c.stdout(Stdio::from(f)),
        None => c.stdout(Stdio::piped()),
    };
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let read = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut v = vec![];
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut v);
            }
            v
        })
    };
    let so = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let se = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("It took too long".into());
            }
        }
    };
    let (so, se) = (so.join().unwrap_or_default(), se.join().unwrap_or_default());
    if status.success() {
        return Ok(so);
    }
    let said = String::from_utf8_lossy(&se);
    let line = said.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
    Err(if line.is_empty() { format!("It stopped with {status}") } else { line })
}

/// Hand finished jobs' results to the app: the toast, Undo, fresh previews of changed
/// files and the new file selected. A crop finished with Enter starts here.
pub fn pump(app: &mut crate::app::App) {
    let crops: Vec<(PathBuf, egui::Rect)> = app
        .panels
        .values_mut()
        .filter_map(|p| p.inspector.as_mut())
        .filter(|i| i.crop.as_ref().is_some_and(|c| c.save))
        .filter_map(|i| i.crop.take().and_then(|c| Some((c.path, c.sel?))))
        .collect();
    for (path, sel) in crops {
        let Some((tools, prog)) = imagemagick() else { continue };
        let ctx = app.ctx.clone();
        app.errands.run(&ctx, move || match crate::picfix::crop(&tools, prog, &path, sel) {
            Ok(dest) => Done { toast: format!("Saved {}", name(&dest)), undo: Some(crate::app::UndoEntry { label: format!("Crop {}", name(&path)), kind: crate::app::UndoKind::Created(vec![dest.clone()]) }), changed: vec![], select: Some(dest) },
            Err(e) => Done::say(format!("Couldn't crop {}: {e}", name(&path))),
        });
    }
    while let Some(d) = app.errands.poll() {
        for p in &d.changed {
            app.thumbs.invalidate(p);
        }
        if !d.changed.is_empty() || d.select.is_some() {
            for panel in app.panels.values_mut() {
                if let Some(i) = panel.inspector.as_mut() {
                    i.invalidate();
                }
            }
        }
        if let Some(u) = d.undo {
            app.undo.push(u);
        }
        if let Some(p) = d.select {
            let t = app.tab();
            if p.parent() == Some(t.path.as_path()) {
                t.reload();
                t.pending_restore = Some(crate::tab::HistState { path: t.path.clone(), cursor: Some(p.clone()), selected: vec![p], scroll: t.current_scroll });
                t.scroll_to_cursor = true;
            }
        }
        if !d.toast.is_empty() {
            app.toast(d.toast);
        }
    }
}

fn name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// ImageMagick, through the tools this app may run.
fn imagemagick() -> Option<(crate::drives::Tools, &'static str)> {
    let t = crate::drives::Tools::current()?;
    let p = crate::picfix::magick(&t)?;
    Some((t, p))
}

/// "photo.jpg", or "3 pictures".
fn what(paths: &[PathBuf]) -> String {
    match paths {
        [p] => name(p),
        ps => format!("{} pictures", ps.len()),
    }
}

/// Copies of `pics` at `size`, beside them.
fn resize(app: &mut crate::app::App, pics: Vec<PathBuf>, size: crate::picfix::Size) {
    let Some((tools, prog)) = imagemagick() else {
        app.toast("Changing pictures needs ImageMagick");
        return;
    };
    let ctx = app.ctx.clone();
    app.errands.run(&ctx, move || {
        let (mut made, mut err) = (vec![], None);
        for p in &pics {
            match crate::picfix::resize(&tools, prog, p, size) {
                Ok(Some(d)) => made.push(d),
                Ok(None) => {}
                Err(e) => err = Some(format!("Couldn't resize {}: {e}", name(p))),
            }
        }
        let toast = match (err, made.as_slice()) {
            (Some(e), _) => e,
            (None, [one]) => format!("Saved {}", name(one)),
            (None, []) if matches!(size, crate::picfix::Size::Fit(_)) => format!("{} already within 1920 px", if pics.len() == 1 { "It's" } else { "They're" }),
            (None, []) => format!("{} already that size", if pics.len() == 1 { "It's" } else { "They're" }),
            (None, many) => format!("Saved {} resized copies", many.len()),
        };
        let undo = (!made.is_empty()).then(|| crate::app::UndoEntry { label: format!("Resize {}", what(&pics)), kind: crate::app::UndoKind::Created(made.clone()) });
        let select = (made.len() == 1).then(|| made[0].clone());
        Done { toast, undo, changed: vec![], select }
    });
}

/// The commands that hand work to another program.
pub fn run(app: &mut crate::app::App, cmd: crate::commands::Cmd) {
    use crate::commands::Cmd;
    let ctx = app.ctx.clone();
    let files: Vec<PathBuf> = app.tab().target_entries().iter().filter(|e| !e.is_dir).map(|e| e.path.clone()).collect();
    // Files inside an archive aren't on the disk for another program to read.
    if app.tab().archive.is_some() {
        app.toast("Extract the archive first");
        return;
    }
    match cmd {
        Cmd::SetWallpaper => {
            let [path] = files.as_slice() else {
                app.toast("Select one picture");
                return;
            };
            let Some(tools) = crate::drives::Tools::current() else {
                app.toast("Maia can't set the wallpaper on this desktop");
                return;
            };
            let (path, desktop) = (path.clone(), crate::wallpaper::desktop());
            app.errands.run(&ctx, move || {
                let desk = crate::wallpaper::look_around(desktop);
                match crate::wallpaper::set(&tools, &desk, &path) {
                    Ok(_) => Done::say("Wallpaper set"),
                    Err(e) => Done::say(e),
                }
            });
        }
        Cmd::RotateLeft | Cmd::RotateRight | Cmd::ResizeHalf | Cmd::ResizeQuarter | Cmd::ResizeFit | Cmd::ResizeCustom | Cmd::CropPicture => {
            let pics: Vec<PathBuf> = files.into_iter().filter(|p| crate::picfix::is_picture(&p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default())).collect();
            if pics.is_empty() || app.tab().path.starts_with(crate::sidebar::trash_dir()) {
                app.toast("Select pictures");
                return;
            }

            let Some((tools, prog)) = imagemagick() else {
                app.toast("Changing pictures needs ImageMagick");
                return;
            };
            match cmd {
                Cmd::RotateLeft | Cmd::RotateRight => {
                    // Another turn of the same pictures: their originals are in the
                    // trash already, and one Undo goes back to them.
                    let again = matches!(app.undo.last(), Some(crate::app::UndoEntry { kind: crate::app::UndoKind::Replaced(p), .. }) if *p == pics);
                    let right = cmd == Cmd::RotateRight;
                    app.errands.run(&ctx, move || {
                        let (mut done, mut err) = (vec![], None);
                        for p in &pics {
                            match crate::picfix::rotate(&tools, prog, p, right, again) {
                                Ok(()) => done.push(p.clone()),
                                Err(e) => err = Some(format!("Couldn't turn {}: {e}", name(p))),
                            }
                        }
                        let undo = (!again && !done.is_empty()).then(|| crate::app::UndoEntry { label: format!("Rotate {}", what(&done)), kind: crate::app::UndoKind::Replaced(done.clone()) });
                        Done { toast: err.unwrap_or_default(), undo, changed: done, select: None }
                    });
                }
                Cmd::ResizeHalf => resize(app, pics, crate::picfix::Size::Scale(0.5)),
                Cmd::ResizeQuarter => resize(app, pics, crate::picfix::Size::Scale(0.25)),
                Cmd::ResizeFit => resize(app, pics, crate::picfix::Size::Fit(1920)),
                Cmd::ResizeCustom => {
                    // The picture's own size to start from, when it has been shown.
                    let start = match pics.as_slice() {
                        [p] => app.thumbs.dims(p).map(|[w, h]| format!("{w} × {h}")).unwrap_or_default(),
                        _ => String::new(),
                    };
                    app.popup = crate::popups::Popup::prompt(
                        "Resize to a longer side, width × height or a percentage",
                        start,
                        Box::new(move |app: &mut crate::app::App, text: String| match crate::picfix::parse_size(&text) {
                            Some(size) => resize(app, pics, size),
                            None => app.toast("Type a size such as 1280, 1280 × 720 or 50%"),
                        }),
                    );
                }
                _ => {
                    let path = pics[0].clone();
                    let id = app.active;
                    let Some(p) = app.panels.get_mut(&id) else { return };
                    // The preview follows the cursor: put it on the picture.
                    let t = p.tab_mut();
                    if let Some(i) = t.entries.iter().position(|e| e.path == path) {
                        t.cursor = Some(i as u32);
                    }
                    let ins = p.inspector.get_or_insert_with(crate::inspector::Inspector::default);
                    ins.git = None;
                    ins.compare = None;
                    ins.crop = Some(crate::picfix::Crop::new(path));
                }
            }
        }
        Cmd::Email => match crate::sendto::email(&files) {
            Ok(()) => app.toast("Opening a new email"),
            Err(e) => app.toast(e),
        },
        Cmd::Print => match crate::drives::Tools::current().filter(crate::sendto::cups) {
            Some(t) if !files.is_empty() => app.popup = crate::popups::Popup::Print(Box::new(crate::sendto::PrintState::open(t, files, &ctx))),
            Some(_) => app.toast("Select files to print"),
            None => app.toast("Printing needs CUPS"),
        },
        _ => {}
    }
}
