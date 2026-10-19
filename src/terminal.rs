//! The terminal pane: your shell on a pseudo-terminal, docked under the panels, going
//! to the folder shown whenever it waits at its prompt. A thread reads what the shell
//! writes and puts it through the emulator (`vt`), so the UI only draws the screen it
//! leaves. Closing the pane hangs the shell up and reaps it: nothing runs while it is
//! closed.

use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use egui::{Color32, CornerRadius, Event, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};

use crate::app::{Action, App};
use crate::commands::{Chord, Cmd};
use crate::theme::{Glyph, Palette};
use crate::vt::{self, Cell, Term};

/// The gap above the pane (as between split panels) and its header's height.
const GAP: f32 = 3.0;
const HEADER_H: f32 = 30.0;
const MIN_H: f32 = 90.0;
const DEFAULT_H: f32 = 240.0;
/// Space around the cells.
const PAD: f32 = 6.0;

/// What the reader thread and the UI share.
pub struct Shared {
    term: Mutex<Term>,
    /// The screen changed since it was drawn (a burst of output asks for one repaint).
    fresh: AtomicBool,
    /// The shell ended: the pane closes.
    ended: AtomicBool,
}

impl Shared {
    pub fn term(&self) -> MutexGuard<'_, Term> {
        self.term.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

/// A program on a pseudo-terminal, read on a thread.
pub struct Pty {
    master: Arc<OwnedFd>,
    child: Option<Child>,
    /// An eventfd: written when the pane closes, which ends the reader's wait.
    stop: OwnedFd,
    pub shared: Arc<Shared>,
    /// Typed input the terminal has not taken yet (its buffer was full).
    pending: Vec<u8>,
}

impl Pty {
    /// Start `cmd` as a session leader with the terminal as its controlling one.
    pub fn spawn(mut cmd: Command, cols: usize, rows: usize, ctx: egui::Context) -> std::io::Result<Pty> {
        let err = std::io::Error::last_os_error;
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(err());
        }
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut name = [0 as libc::c_char; 128];
        if unsafe { libc::grantpt(fd) != 0 || libc::unlockpt(fd) != 0 || libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) != 0 } {
            return Err(err());
        }
        let sfd = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if sfd < 0 {
            return Err(err());
        }
        let slave = unsafe { OwnedFd::from_raw_fd(sfd) };
        // Line editing that knows a character can be several bytes.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(sfd, &mut t) } == 0 {
            t.c_iflag |= libc::IUTF8;
            unsafe { libc::tcsetattr(sfd, libc::TCSANOW, &t) };
        }
        set_size(fd, cols, rows);
        cmd.env("TERM", "xterm-256color").env("COLORTERM", "truecolor").env_remove("COLUMNS").env_remove("LINES");
        cmd.stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        };
        let child = cmd.spawn()?;
        // The parent keeps no end of the terminal's slave side: the shell's exit closes it.
        drop(cmd);
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if efd < 0 {
            return Err(err());
        }
        let stop = unsafe { OwnedFd::from_raw_fd(efd) };
        let shared = Arc::new(Shared { term: Mutex::new(Term::new(cols, rows)), fresh: AtomicBool::new(false), ended: AtomicBool::new(false) });
        let master = Arc::new(master);
        let (m, s, sh) = (master.clone(), stop.try_clone()?, shared.clone());
        std::thread::Builder::new().name("terminal".into()).spawn(move || read(m, s, sh, ctx))?;
        Ok(Pty { master, child: Some(child), stop, shared, pending: vec![] })
    }

    /// Send input; what the terminal can't take now waits for `flush`.
    pub fn write(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        self.flush();
    }

    /// Send waiting input; true when some is still waiting.
    pub fn flush(&mut self) -> bool {
        while !self.pending.is_empty() {
            let n = unsafe { libc::write(self.master.as_raw_fd(), self.pending.as_ptr().cast(), self.pending.len()) };
            if n <= 0 {
                break;
            }
            self.pending.drain(..n as usize);
        }
        !self.pending.is_empty()
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        self.shared.term().screen.resize(cols, rows);
        set_size(self.master.as_raw_fd(), cols, rows);
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// The shell waits at its prompt: no program of its runs in the foreground.
    pub fn idle(&self) -> bool {
        let fg = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        fg > 0 && self.pid() == Some(fg as u32)
    }

    pub fn cwd(&self) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", self.pid()?)).ok()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let one = 1u64;
        unsafe { libc::write(self.stop.as_raw_fd(), (&one as *const u64).cast(), 8) };
        let Some(mut child) = self.child.take() else { return };
        let fg = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        unsafe {
            if fg > 0 {
                libc::kill(-fg, libc::SIGHUP);
            }
            libc::kill(-(child.id() as i32), libc::SIGHUP);
        }
        // Reaped on a thread: a shell takes a moment to go, and one that ignores the
        // hangup is killed after two seconds.
        let _ = std::thread::Builder::new().name("terminal reaper".into()).spawn(move || {
            for _ in 0..40 {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

fn set_size(fd: i32, cols: usize, rows: usize) {
    let ws = libc::winsize { ws_row: rows as u16, ws_col: cols as u16, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

/// The reader: waits for output, parses it into the screen and asks for a repaint (once
/// until the UI has drawn it). Ends when the shell does or the pane closes.
fn read(master: Arc<OwnedFd>, stop: OwnedFd, shared: Arc<Shared>, ctx: egui::Context) {
    let mut buf = vec![0u8; 16 << 10];
    loop {
        let mut fds = [libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 }, libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            return;
        }
        let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 && matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
            continue;
        }
        // EIO: the shell and everything it started closed the terminal.
        if n <= 0 {
            break;
        }
        let replies = {
            let mut t = shared.term();
            t.feed(&buf[..n as usize]);
            std::mem::take(&mut t.screen.replies)
        };
        if !replies.is_empty() {
            unsafe { libc::write(master.as_raw_fd(), replies.as_ptr().cast(), replies.len()) };
        }
        if !shared.fresh.swap(true, Ordering::AcqRel) {
            ctx.request_repaint();
        }
    }
    shared.ended.store(true, Ordering::Release);
    ctx.request_repaint();
}

/// The pane in the window.
pub struct Pane {
    pub pty: Option<Pty>,
    /// The last screen, shown while a closed pane folds away.
    fading: Option<Arc<Shared>>,
    pub focused: bool,
    /// The height dragged to, in points.
    pub height: f32,
    /// Cells the shell was last told it has.
    size: (usize, usize),
    /// The active panel's folder when last looked at.
    seen: PathBuf,
    /// The folder the shell was started in or last sent to.
    dir: PathBuf,
    /// Text being selected: where the drag began and where it is (lasting row, column).
    sel: Option<((u64, usize), (u64, usize))>,
    /// Wheel movement short of a whole row.
    wheel: f32,
    /// When the cursor last moved or a key went in: the blink starts over.
    blink: f64,
}

impl Pane {
    pub fn new(height: f32) -> Pane {
        Pane { pty: None, fading: None, focused: false, height: if height > 0.0 { height } else { DEFAULT_H }, size: (80, 24), seen: PathBuf::new(), dir: PathBuf::new(), sel: None, wheel: 0.0, blink: 0.0 }
    }

    /// The screen's rows as text (tests).
    #[cfg(test)]
    pub fn text(&self) -> String {
        let Some(p) = &self.pty else { return String::new() };
        let t = p.shared.term();
        (0..t.screen.rows).map(|y| t.screen.row_text(y)).collect::<Vec<_>>().join("\n")
    }
}

/// A folder the shell can go to for this tab: a real one on this machine (not the
/// Computer view, an archive, the trash or a kernel filesystem).
fn place(t: &crate::tab::Tab) -> Option<PathBuf> {
    let p = &t.path;
    let real = t.archive.is_none() && p.is_absolute() && !crate::tab::is_computer(p);
    (real && !p.ancestors().any(crate::trashcan::is_trash_files) && !crate::fsx::is_virtual_fs(p) && p.is_dir()).then(|| p.clone())
}

/// Ctrl+`: open the pane, give it the keyboard, or (when it has it) close it.
pub fn toggle(app: &mut App) {
    if app.terminal.pty.is_none() {
        open(app);
    } else if !app.terminal.focused {
        app.terminal.focused = true;
    } else {
        close(app);
    }
}

fn open(app: &mut App) {
    let t = app.panel().tab();
    let (here, dir) = (t.path.clone(), place(t).unwrap_or_else(crate::fsx::home));
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let mut cmd = Command::new(shell);
    cmd.current_dir(&dir);
    let (cols, rows) = app.terminal.size;
    match Pty::spawn(cmd, cols, rows, app.ctx.clone()) {
        Ok(p) => {
            let pane = &mut app.terminal;
            (pane.pty, pane.fading, pane.focused, pane.seen, pane.dir, pane.sel) = (Some(p), None, true, here, dir, None);
        }
        Err(e) => app.toast(format!("Couldn't start a terminal: {e}")),
    }
}

fn close(app: &mut App) {
    let pane = &mut app.terminal;
    pane.fading = pane.pty.take().map(|p| p.shared.clone());
    pane.focused = false;
    pane.sel = None;
    app.ctx.memory_mut(|m| m.surrender_focus(id()));
}

fn id() -> Id {
    Id::new("terminal")
}

/// Room the pane takes at the bottom of `full`, with the gap above it.
pub fn room(app: &App, full: Rect) -> f32 {
    if app.terminal.pty.is_none() {
        return 0.0;
    }
    let most = (full.height() - crate::panel::MIN_PANEL_H - GAP).max(MIN_H);
    app.terminal.height.clamp(MIN_H, most).min(full.height() * 0.8) + GAP
}

/// Follow the folder the active panel shows: with the shell at its prompt, it goes
/// there, typed as a command with a space before it (which keeps it out of the history
/// where HISTCONTROL ignores such lines).
fn follow(app: &mut App) {
    let Some(t) = app.panels.get(&app.active).map(|p| p.tab()) else { return };
    let Some(pty) = app.terminal.pty.as_mut().filter(|_| t.path != app.terminal.seen) else { return };
    app.terminal.seen = t.path.clone();
    let Some(dir) = place(t) else { return };
    if !pty.idle() || pty.cwd().as_deref() == Some(dir.as_path()) {
        return;
    }
    let Some(s) = dir.to_str().filter(|s| !s.contains(char::is_control)) else { return };
    // Ctrl+E Ctrl+U first: whatever was typed and not sent goes.
    pty.write(format!("\x05\x15 cd -- '{}'\r", s.replace('\'', r"'\''")).as_bytes());
    app.terminal.dir = dir;
}

/// Keys while the terminal has the keyboard: all go to the shell, but for the command
/// that closes the pane and those that switch tabs.
pub fn keys(app: &mut App, ctx: &egui::Context, events: &[Event]) {
    let Some(pty) = app.terminal.pty.as_mut() else {
        app.terminal.focused = false;
        return;
    };
    let mods = ctx.input(|i| i.modifiers);
    let (app_cursor, bracketed) = {
        let t = pty.shared.term();
        (t.screen.app_cursor, t.screen.bracketed_paste)
    };
    let mut out = vec![];
    for ev in events {
        match ev {
            Event::Key { key, pressed: true, modifiers: m, .. } => {
                if let (Some(c @ (Cmd::TerminalPanel | Cmd::NextTab | Cmd::PrevTab)), _) = app.keymap.lookup(&[Chord::from_key(*key, *m)]) {
                    app.actions.push(Action::Cmd(c));
                } else if (m.ctrl && m.shift && *key == Key::V) || (m.shift && !m.ctrl && *key == Key::Insert) {
                    paste(&mut out, &crate::clipboard::get_text().unwrap_or_default(), bracketed);
                } else {
                    key_bytes(*key, *m, app_cursor, &mut out);
                }
            }
            Event::Text(t) => {
                if mods.alt {
                    out.push(0x1b);
                }
                out.extend_from_slice(t.as_bytes());
            }
            Event::Ime(egui::ImeEvent::Commit(t)) => out.extend_from_slice(t.as_bytes()),
            Event::Copy if mods.shift => {
                if let Some((a, b)) = app.terminal.sel {
                    ctx.copy_text(pty.shared.term().screen.text(a, b));
                }
            }
            Event::Copy => out.push(0x03),
            Event::Cut => out.push(0x18),
            Event::Paste(s) if mods.shift => paste(&mut out, s, bracketed),
            Event::Paste(_) => out.push(0x16),
            _ => {}
        }
    }
    if !out.is_empty() {
        pty.shared.term().screen.scroll = 0;
        pty.write(&out);
        app.terminal.blink = ctx.input(|i| i.time);
    }
    ctx.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Key { .. } | Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_) | Event::Ime(_))));
}

/// Pasted text: line breaks as Enter sends them, marked as pasted when the program asked.
fn paste(out: &mut Vec<u8>, s: &str, bracketed: bool) {
    let s = s.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(s.replace("\x1b[201~", "").as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(s.as_bytes());
    }
}

/// What a key sends, as xterm does. Keys that type text send it with their Text event.
fn key_bytes(key: Key, m: Modifiers, app_cursor: bool, out: &mut Vec<u8>) {
    // xterm's modifier parameter: 1 + Shift + 2 Alt + 4 Ctrl.
    let n = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let mut csi = |c: char, ss3: bool| {
        let _ = if n > 1 {
            write!(out, "\x1b[1;{n}{c}")
        } else if ss3 {
            write!(out, "\x1bO{c}")
        } else {
            write!(out, "\x1b[{c}")
        };
    };
    match key {
        Key::ArrowUp => csi('A', app_cursor),
        Key::ArrowDown => csi('B', app_cursor),
        Key::ArrowRight => csi('C', app_cursor),
        Key::ArrowLeft => csi('D', app_cursor),
        Key::Home => csi('H', app_cursor),
        Key::End => csi('F', app_cursor),
        Key::F1 => csi('P', true),
        Key::F2 => csi('Q', true),
        Key::F3 => csi('R', true),
        Key::F4 => csi('S', true),
        Key::Insert | Key::Delete | Key::PageUp | Key::PageDown | Key::F5 | Key::F6 | Key::F7 | Key::F8 | Key::F9 | Key::F10 | Key::F11 | Key::F12 => {
            let k = match key {
                Key::Insert => 2,
                Key::Delete => 3,
                Key::PageUp => 5,
                Key::PageDown => 6,
                Key::F5 => 15,
                Key::F6 => 17,
                Key::F7 => 18,
                Key::F8 => 19,
                Key::F9 => 20,
                Key::F10 => 21,
                Key::F11 => 23,
                _ => 24,
            };
            let _ = if n > 1 { write!(out, "\x1b[{k};{n}~") } else { write!(out, "\x1b[{k}~") };
        }
        Key::Tab if m.shift => out.extend_from_slice(b"\x1b[Z"),
        Key::Escape => out.push(0x1b),
        _ => {
            let b = match key {
                Key::Enter => b'\r',
                Key::Tab => b'\t',
                Key::Backspace if m.ctrl => 0x08,
                Key::Backspace => 0x7f,
                _ if !m.ctrl => return,
                Key::Space | Key::Num2 => 0,
                Key::OpenBracket | Key::Num3 => 0x1b,
                Key::Backslash | Key::Num4 => 0x1c,
                Key::CloseBracket | Key::Num5 => 0x1d,
                Key::Num6 => 0x1e,
                Key::Slash | Key::Minus | Key::Num7 => 0x1f,
                Key::Num8 => 0x7f,
                k => match k.name().as_bytes() {
                    [c @ b'A'..=b'Z'] => c - b'@',
                    _ => return,
                },
            };
            if m.alt {
                out.push(0x1b);
            }
            out.push(b);
        }
    }
}

/// The 16 ANSI colours from the theme: its danger red, the git status green and amber,
/// its accent for blue and code colours for magenta, cyan and the greys.
fn ansi(pal: &Palette) -> [Color32; 16] {
    let s = &pal.syntax;
    let ink = |c: Color32| if pal.dark { c } else { c.lerp_to_gamma(Color32::BLACK, 0.25) };
    let black = if pal.dark { pal.bg.lerp_to_gamma(pal.text, 0.22) } else { pal.text };
    let base = [black, pal.danger, ink(crate::gitstatus::GREEN), ink(crate::gitstatus::AMBER), pal.accent, s.keyword, s.kind, pal.text_dim];
    let mut c = [Color32::BLACK; 16];
    for (i, b) in base.into_iter().enumerate() {
        c[i] = b;
        // The bright half: a step toward the text colour (lighter on dark, darker on light).
        c[i + 8] = b.lerp_to_gamma(pal.text, 0.3);
    }
    (c[8], c[15]) = (s.comment, pal.text);
    c
}

fn color(code: u16, ansi: &[Color32; 16], default: Color32) -> Color32 {
    match code {
        vt::DEFAULT => default,
        0..16 => ansi[code as usize],
        16..232 => {
            let i = code - 16;
            let v = |n: u16| if n == 0 { 0 } else { (55 + n * 40) as u8 };
            Color32::from_rgb(v(i / 36), v(i / 6 % 6), v(i % 6))
        }
        232..256 => Color32::from_gray(8 + (code - 232) as u8 * 10),
        _ => {
            let ch = |s: u16| {
                let v = (code >> s & 31) as u8;
                v << 3 | v >> 2
            };
            Color32::from_rgb(ch(10), ch(5), ch(0))
        }
    }
}

/// Draw the pane under the panels, whose lowest edge is at `above` (it eases with them
/// as the pane opens and closes), with the divider that resizes it.
pub fn pane(app: &mut App, ui: &mut Ui, full: Rect, above: f32) {
    follow(app);
    if app.terminal.pty.as_ref().is_some_and(|p| p.shared.ended()) {
        close(app);
    }
    let rect = Rect::from_min_max(pos2(full.left(), above + GAP), full.max);
    let pal = app.pal;
    let pane = &mut app.terminal;
    if rect.height() < 1.0 {
        pane.fading = None;
        return;
    }
    let Some(shared) = pane.pty.as_ref().map(|p| p.shared.clone()).or_else(|| pane.fading.clone()) else { return };
    let open = pane.pty.is_some();
    let ctx = ui.ctx().clone();
    // A press anywhere else gives the keyboard back to the files.
    let press = ctx.input(|i| i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten());
    if let Some(p) = press {
        pane.focused = open && rect.contains(p);
        pane.blink = ctx.input(|i| i.time);
    }
    let focused = pane.focused && matches!(app.popup, crate::popups::Popup::None);
    ctx.memory_mut(|m| {
        if focused {
            m.request_focus(id());
            m.set_focus_lock_filter(id(), egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true });
        } else if m.has_focus(id()) {
            m.surrender_focus(id());
        }
    });
    // The divider.
    if open {
        let handle = Rect::from_center_size(pos2(rect.center().x, rect.top() - GAP / 2.0), vec2(rect.width(), 6.0));
        let resp = ui.interact(handle, Id::new("terminal divider"), Sense::drag());
        if resp.hovered() || resp.dragged() {
            ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
            ui.painter().rect_filled(handle.shrink(2.0), CornerRadius::same(1), pal.accent.gamma_multiply(0.6));
        }
        if let Some(p) = resp.interact_pointer_pos().filter(|_| resp.dragged()) {
            pane.height = full.bottom() - p.y;
        }
        if resp.double_clicked() {
            pane.height = DEFAULT_H;
        }
    }
    let rad = if app.cfg.round_corners { 8 } else { 0 };
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(rad), pal.panel);
    let body = Rect::from_min_max(pos2(rect.left(), rect.top() + HEADER_H), rect.max);
    p.rect_filled(body, CornerRadius { nw: 0, ne: 0, sw: rad, se: rad }, pal.bg);
    p.line_segment([body.left_top(), body.right_top()], Stroke::new(1.0_f32, pal.border));
    let font = FontId::monospace(app.cfg.inspector_font_size);
    let (cw, row_h) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font).round()));
    // The size the pane is going to (not the one it eases through): the shell is told once.
    let goal = room(app, full) - GAP - HEADER_H - 2.0 * PAD;
    let cols = ((full.width() - 2.0 * PAD) / cw).floor().max(2.0) as usize;
    let rows = (goal / row_h).floor().max(1.0) as usize;
    let pane = &mut app.terminal;
    if let Some(pty) = pane.pty.as_mut().filter(|_| (cols, rows) != pane.size) {
        pty.resize(cols, rows);
        pane.size = (cols, rows);
        // Rewrapped rows are numbered anew: a selection would land on other text.
        pane.sel = None;
    }
    if pane.pty.as_mut().is_some_and(|p| p.flush()) {
        ctx.request_repaint_after(std::time::Duration::from_millis(10));
    }
    let mut term = shared.term();
    let s = &mut term.screen;
    // ------------------------------------------------ header
    let title = if s.title.is_empty() { tilde(s.cwd.as_deref().map_or(pane.dir.as_path(), Path::new)) } else { s.title.clone() };
    let hdr = Rect::from_min_max(rect.min, pos2(rect.right(), body.top()));
    let mut hui = ui.new_child(UiBuilder::new().max_rect(hdr.shrink2(vec2(6.0, 3.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    hui.set_clip_rect(hdr.intersect(ui.clip_rect()));
    let mut closing = false;
    if open && crate::ui::glyph_button_as(&mut hui, Glyph::Close, "Close terminal", true, &pal, "terminal close").clicked() {
        closing = true;
    }
    hui.add_space(4.0);
    hui.add(egui::Label::new(egui::RichText::new(title).size(12.0).color(pal.text_dim)).truncate().selectable(false));
    // ------------------------------------------------ cells
    let resp = ui.interact(body, id(), Sense::click_and_drag());
    let (x0, y0) = (body.left() + PAD, body.top() + PAD);
    let shown = (((body.height() - PAD) / row_h).ceil().max(0.0) as usize).min(s.rows);
    let at = |s: &vt::Screen, p: Pos2| {
        let y = ((p.y - y0) / row_h).floor().clamp(0.0, (s.rows - 1) as f32) as usize;
        (s.view_line(y), ((p.x - x0) / cw).floor().clamp(0.0, (s.cols - 1) as f32) as usize)
    };
    if resp.drag_started() {
        if let Some(p) = ctx.input(|i| i.pointer.press_origin()) {
            let c = at(s, p);
            pane.sel = Some((c, c));
        }
    } else if resp.dragged() {
        if let (Some(p), Some(sel)) = (resp.interact_pointer_pos(), pane.sel.as_mut()) {
            sel.1 = at(s, p);
        }
    } else if resp.clicked() {
        pane.sel = None;
    }
    // The wheel scrolls back through what went by; in a full-screen program it moves
    // the cursor, as xterm's alternate scroll does.
    if resp.hovered() {
        let notches: f32 = ctx.input(|i| {
            i.events
                .iter()
                .map(|e| match e {
                    Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, .. } => delta.y / row_h,
                    Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, .. } => delta.y * 3.0,
                    Event::MouseWheel { delta, .. } => delta.y * rows as f32,
                    _ => 0.0,
                })
                .sum()
        });
        pane.wheel += notches;
        let whole = pane.wheel.trunc();
        pane.wheel -= whole;
        if whole != 0.0 {
            if s.alt() {
                let k = if whole > 0.0 { if s.app_cursor { "\x1bOA" } else { "\x1b[A" } } else if s.app_cursor { "\x1bOB" } else { "\x1b[B" };
                if let Some(pty) = pane.pty.as_mut() {
                    pty.write(k.repeat(whole.abs() as usize).as_bytes());
                }
            } else {
                s.scroll_view(whole as isize);
            }
        }
    }
    let now = ctx.input(|i| i.time);
    if shared.fresh.swap(false, Ordering::AcqRel) {
        pane.blink = now;
    }
    let ansi = ansi(&pal);
    let sel = pane.sel.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
    let mut bgs: Vec<(Rect, Color32)> = vec![];
    let mut galleys = Vec::with_capacity(shown);
    ui.fonts_mut(|f| {
        for y in 0..shown {
            let n = s.view_line(y);
            let Some(row) = s.line(n) else { continue };
            let top = y0 + y as f32 * row_h;
            let mut job = egui::text::LayoutJob::default();
            let mut key = None;
            // Where the text is laid and where its column is: a glyph wider or narrower
            // than its cells (a fallback font's) puts the next run back on its column.
            let (mut laid, mut want) = (0.0, 0.0);
            let end = row.cells.iter().rposition(|c| c.ch() != ' ').map_or(0, |i| i + 1);
            for (x, &c) in row.cells.iter().enumerate().take(s.cols) {
                let picked = sel.is_some_and(|(a, b)| (n, x) >= a && (n, x) <= b);
                let (fg, bg) = colors(c, picked, &ansi, &pal);
                if bg != pal.bg {
                    let r = Rect::from_min_size(pos2(x0 + x as f32 * cw, top), vec2(cw, row_h));
                    match bgs.last_mut() {
                        Some((last, col)) if *col == bg && (last.right() - r.left()).abs() < 0.5 && last.top() == r.top() => last.max.x = r.right(),
                        _ => bgs.push((r, bg)),
                    }
                }
                if x >= end || c.flags() & vt::SPACER != 0 {
                    continue;
                }
                let ch = c.ch();
                let w = if c.flags() & vt::WIDE != 0 { 2.0 * cw } else { cw };
                let k = (fg, c.c & (vt::ITALIC | vt::UNDERLINE | vt::STRIKE));
                if key != Some(k) || (want - laid) > 0.5 || (laid - want) > 0.5 {
                    let lead = want - laid;
                    let line = |on: bool| if on { Stroke::new(1.0_f32, fg) } else { Stroke::NONE };
                    let format = egui::TextFormat { font_id: font.clone(), color: fg, italics: k.1 & vt::ITALIC != 0, underline: line(k.1 & vt::UNDERLINE != 0), strikethrough: line(k.1 & vt::STRIKE != 0), ..Default::default() };
                    job.append("", lead, format);
                    (key, laid) = (Some(k), want);
                }
                job.text.push(ch);
                if let Some(sec) = job.sections.last_mut() {
                    sec.byte_range.end = job.text.len();
                }
                laid += if ch.is_ascii() { cw } else { f.glyph_width(&font, ch) };
                want += w;
            }
            // A selection runs on to the end of the rows it spans, past what a kept row holds.
            if let Some((a, b)) = sel.filter(|(a, b)| (a.0..=b.0).contains(&n)) {
                let from = if n == a.0 { a.1 } else { 0 }.max(row.cells.len());
                let to = if n == b.0 { b.1 + 1 } else { s.cols };
                if from < to {
                    bgs.push((Rect::from_min_max(pos2(x0 + from as f32 * cw, top), pos2(x0 + to as f32 * cw, top + row_h)), pal.selected));
                }
            }
            if !job.text.is_empty() {
                galleys.push((pos2(x0, top), f.layout_job(job)));
            }
        }
    });
    let p = ui.painter_at(body);
    for (r, c) in bgs {
        p.rect_filled(r, CornerRadius::ZERO, c);
    }
    for (pos, g) in galleys {
        p.galley(pos, g, pal.text);
    }
    // ------------------------------------------------ cursor
    let (cx, cy) = s.cursor();
    let vy = cy + s.scroll;
    if open && s.cursor_shown && vy < shown {
        // It blinks for ten seconds after the last key or output, then rests (no repaints).
        let phase = (now - pane.blink).max(0.0) / 0.53;
        let blink = app.cfg.animations && focused && phase < 19.0;
        if blink {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((1.0 - phase.fract()) * 0.53));
        }
        if !blink || (phase as u64) % 2 == 0 {
            let cell = s.line(s.view_line(vy)).and_then(|r| r.cells.get(cx).copied()).unwrap_or(Cell::BLANK);
            let w = if cell.flags() & vt::WIDE != 0 { 2.0 * cw } else { cw };
            let r = Rect::from_min_size(pos2(x0 + cx as f32 * cw, y0 + vy as f32 * row_h), vec2(w, row_h));
            match (focused, s.shape) {
                (false, _) => {
                    p.rect_stroke(r.shrink(0.5), CornerRadius::same(1), Stroke::new(1.0_f32, pal.accent), StrokeKind::Inside);
                }
                (true, vt::Shape::Bar) => {
                    p.rect_filled(Rect::from_min_size(r.min, vec2(2.0, row_h)), CornerRadius::ZERO, pal.accent);
                }
                (true, vt::Shape::Underline) => {
                    p.rect_filled(Rect::from_min_max(pos2(r.left(), r.bottom() - 2.0), r.max), CornerRadius::ZERO, pal.accent);
                }
                (true, vt::Shape::Block) => {
                    p.rect_filled(r, CornerRadius::same(1), pal.accent);
                    if cell.ch() != ' ' {
                        p.text(r.min, egui::Align2::LEFT_TOP, cell.ch(), font.clone(), pal.bg);
                    }
                }
            }
        }
    }
    drop(term);
    if closing {
        close(app);
    }
}

/// A path with the home folder written as ~.
fn tilde(p: &Path) -> String {
    match p.strip_prefix(crate::fsx::home()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// A cell's text and background colours.
fn colors(c: Cell, picked: bool, ansi: &[Color32; 16], pal: &Palette) -> (Color32, Color32) {
    if picked {
        return (pal.on_sel(), pal.selected);
    }
    let f = c.flags();
    // Bold text in one of the eight colours takes its bright twin, as xterm does.
    let fg = if f & vt::BOLD != 0 && c.fg < 8 { c.fg + 8 } else { c.fg };
    let (mut fg, mut bg) = (color(fg, ansi, pal.text), color(c.bg, ansi, pal.bg));
    if f & vt::INVERSE != 0 {
        (fg, bg) = (bg, fg);
    }
    if f & vt::DIM != 0 {
        fg = fg.lerp_to_gamma(bg, 0.4);
    }
    if f & vt::HIDDEN != 0 {
        fg = bg;
    }
    (fg, bg)
}
