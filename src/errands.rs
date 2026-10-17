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
    output_to(c, timeout, None)
}

/// `output`, with what it prints written to `out` instead.
pub fn output_to(c: Command, timeout: Duration, out: Option<std::fs::File>) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut c = crate::media::tool_cmd(c);
    c.stderr(Stdio::piped());
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
/// files and the new file selected.
pub fn pump(app: &mut crate::app::App) {
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

/// The commands that hand work to another program.
pub fn run(app: &mut crate::app::App, cmd: crate::commands::Cmd) {
    use crate::commands::Cmd;
    let ctx = app.ctx.clone();
    let files: Vec<PathBuf> = app.tab().target_entries().iter().filter(|e| !e.is_dir).map(|e| e.path.clone()).collect();
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
