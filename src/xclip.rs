//! X11 clipboard and drag out, on a connection of our own served by a background thread
//! (as wlclip.rs does on Wayland): several types at once (text plus file lists), so files
//! copied or cut interoperate with other file managers, and files dragged past the
//! window's edge carry on as an XDND drag into other apps. Drops in come through winit.

use std::io::{Error, ErrorKind, Read, Result, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;
use x11rb::{CURRENT_TIME, NONE};

use crate::wlclip::{Dnd, Item, emit};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        INCR,
        UTF8_STRING,
        TEXT,
        MAIA_SELECTION,
        XdndAware,
        XdndSelection,
        XdndTypeList,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndActionCopy,
        XdndActionMove,
        XdndActionLink,
    }
}

/// Larger transfers go in pieces of this size (INCR), smaller if the server says so.
pub const CHUNK: usize = 256 << 10;
/// A transfer in pieces, or a drop, the other side stopped answering.
const STALE: Duration = Duration::from_secs(5);
/// Highest XDND version spoken.
const XDND: u32 = 5;
/// Buttons 1 to 5 in a key and button state.
const BUTTONS: u16 = 0x1f00;

enum Command {
    Store(Vec<Item>),
    /// Load the first available of these MIME types.
    Load(Vec<String>),
    /// Carry these files out of the window (the pointer button is still held); positions
    /// back over it are reported divided by the scale.
    Drag(Vec<PathBuf>, f32),
}

pub struct XClip {
    tx: Sender<Command>,
    /// Wakes the thread for a command.
    wake: UnixStream,
    rx: Receiver<Result<Item>>,
    /// The window's own connection, whose pointer grab a drag out takes over.
    window: Option<XCBConnection>,
}

impl XClip {
    pub fn new(window: &winit::window::Window) -> Option<XClip> {
        use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
        let xcb = match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Xlib(h) => unsafe { (x11_dl::xlib_xcb::Xlib_xcb::open().ok()?.XGetXCBConnection)(h.display?.as_ptr().cast()) },
            RawDisplayHandle::Xcb(h) => h.connection?.as_ptr(),
            _ => return None,
        };
        let id = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => h.window as Window,
            RawWindowHandle::Xcb(h) => h.window.get(),
            _ => return None,
        };
        // Lives as long as the window's display: the clipboard is never dropped.
        let conn = unsafe { XCBConnection::from_raw_xcb_connection(xcb, false) }.ok()?;
        Self::spawn(Some(conn), id)
    }

    /// The clipboard thread; `maia` is the window drags out come back to.
    pub fn spawn(window: Option<XCBConnection>, maia: Window) -> Option<XClip> {
        let (wake, woken) = UnixStream::pair().ok()?;
        wake.set_nonblocking(true).ok()?;
        woken.set_nonblocking(true).ok()?;
        let (tx, cmds) = mpsc::channel();
        let (reply_tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Some(st) = State::new(reply_tx, maia) {
                worker(st, cmds, woken);
            }
        });
        Some(XClip { tx, wake, rx, window })
    }

    fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
        let _ = (&self.wake).write(&[0]);
    }

    pub fn store(&self, items: Vec<Item>) {
        self.send(Command::Store(items));
    }

    pub fn load(&self, prefer: &[&str]) -> Result<Item> {
        while self.rx.try_recv().is_ok() {}
        self.send(Command::Load(prefer.iter().map(|s| s.to_string()).collect()));
        self.rx.recv_timeout(Duration::from_millis(800)).map_err(|_| Error::other("clipboard timed out"))?
    }

    pub fn start_drag(&self, paths: Vec<PathBuf>, scale: f32) {
        // The press's implicit grab is the window's: let go of it so ours can take over.
        if let Some(c) = &self.window {
            let _ = c.ungrab_pointer(CURRENT_TIME);
            let _ = c.flush();
        }
        self.send(Command::Drag(paths, scale));
    }
}

fn worker(mut st: State, cmds: Receiver<Command>, mut woken: UnixStream) {
    let fds = [st.conn.as_raw_fd(), woken.as_raw_fd()];
    loop {
        loop {
            match st.conn.poll_for_event() {
                Ok(Some(ev)) => st.event(ev),
                Ok(None) => break,
                Err(_) => return,
            }
        }
        st.pointer_moved();
        if st.conn.flush().is_err() {
            return;
        }
        let timeout = st.deadline().map_or(-1, |d| d.saturating_duration_since(Instant::now()).as_millis() as i32 + 1);
        let mut pfd = fds.map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        unsafe { libc::poll(pfd.as_mut_ptr(), 2, timeout) };
        let _ = woken.read(&mut [0; 64]);
        loop {
            match cmds.try_recv() {
                Ok(cmd) => st.command(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        st.expire(Instant::now());
    }
}

fn xdnd(conn: &XCBConnection, to: Window, kind: Atom, data: [u32; 5]) {
    let _ = conn.send_event(false, to, EventMask::NO_EVENT, ClientMessageEvent::new(32, to, kind, data));
}

/// A type we serve.
struct Offer {
    atom: Atom,
    bytes: Rc<[u8]>,
}

/// A selection asked for: the types wanted, then the one chosen from the owner's
/// targets, and its bytes while they come in pieces.
struct Load {
    want: Vec<(String, Atom)>,
    chosen: Option<(String, Atom)>,
    incr: Option<Vec<u8>>,
}

/// Bytes going to another app in pieces.
struct Piecewise {
    requestor: Window,
    property: Atom,
    kind: Atom,
    bytes: Rc<[u8]>,
    at: usize,
    since: Instant,
}

/// The drag Maia carried out of its window.
struct DragOut {
    offers: Vec<Offer>,
    paths: Vec<PathBuf>,
    scale: f32,
    /// The pointer: root position, keys and buttons held, time; `moved` until followed.
    at: (i16, i16),
    state: KeyButMask,
    time: Timestamp,
    moved: bool,
    target: Option<Target>,
    /// Over Maia's own window, where the drag is Maia's again: the position in it.
    home: Option<(f32, f32)>,
    /// Let go: until when the target's word is waited for.
    released: Option<Instant>,
}

/// The XDND-aware window under the pointer.
#[derive(Clone, Copy)]
struct Target {
    win: Window,
    /// A position was sent and its status hasn't come back.
    waiting: bool,
    accepted: bool,
    dropped: bool,
}

struct State {
    conn: XCBConnection,
    atoms: Atoms,
    root: Window,
    /// Our own unmapped window: selection owner, drag source, and where loads land.
    win: Window,
    maia: Window,
    chunk: usize,
    reply_tx: Sender<Result<Item>>,
    /// What we hold on the clipboard (empty once another app takes it).
    clip: Vec<Offer>,
    load: Option<Load>,
    sending: Vec<Piecewise>,
    drag: Option<DragOut>,
}

impl State {
    fn new(reply_tx: Sender<Result<Item>>, maia: Window) -> Option<State> {
        let (conn, screen) = XCBConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
        let win = conn.generate_id().ok()?;
        let aux = CreateWindowAux::new().override_redirect(1).event_mask(EventMask::PROPERTY_CHANGE);
        conn.create_window(0, win, root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, x11rb::COPY_FROM_PARENT, &aux).ok()?;
        let chunk = CHUNK.min(conn.maximum_request_bytes() - 64);
        Some(State { conn, atoms, root, win, maia, chunk, reply_tx, clip: vec![], load: None, sending: vec![], drag: None })
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Store(items) => {
                self.clip = self.offers(items);
                let _ = self.conn.set_selection_owner(self.win, self.atoms.CLIPBOARD, CURRENT_TIME);
            }
            Command::Load(prefer) => self.start_load(prefer),
            Command::Drag(paths, scale) => self.start_drag(paths, scale),
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::SelectionRequest(e) => self.serve(e),
            Event::SelectionClear(e) if e.selection == self.atoms.CLIPBOARD => self.clip.clear(),
            Event::SelectionNotify(e) if e.requestor == self.win => self.loaded(e),
            Event::PropertyNotify(e) => self.property(e),
            Event::MotionNotify(e) => self.pointer(e.root_x, e.root_y, e.state, e.time),
            Event::ButtonRelease(e) if self.drag.as_ref().is_some_and(|d| d.released.is_none()) => {
                self.pointer(e.root_x, e.root_y, e.state, e.time);
                self.pointer_moved();
                self.release();
            }
            Event::ClientMessage(e) => self.message(e),
            _ => {}
        }
    }

    /// The soonest a transfer or drop goes stale.
    fn deadline(&self) -> Option<Instant> {
        let sends = self.sending.iter().map(|p| p.since + STALE);
        sends.chain(self.drag.as_ref().and_then(|d| d.released)).min()
    }

    fn expire(&mut self, now: Instant) {
        self.sending.retain(|p| p.since + STALE > now);
        if self.drag.as_ref().and_then(|d| d.released).is_some_and(|t| t <= now) {
            self.end_drag();
        }
    }

    fn intern<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Vec<Atom> {
        let cookies: Vec<_> = names.into_iter().map(|n| self.conn.intern_atom(false, n.as_bytes()).ok()).collect();
        cookies.into_iter().map(|c| c.and_then(|c| c.reply().ok()).map_or(NONE, |r| r.atom)).collect()
    }

    fn offers(&self, items: Vec<Item>) -> Vec<Offer> {
        let atoms = self.intern(items.iter().map(|(m, _)| m.as_str()));
        items.into_iter().zip(atoms).map(|((_, bytes), atom)| Offer { atom, bytes: Rc::from(bytes) }).collect()
    }

    // ------------------------------------------------ serving

    /// Answer another app asking for one of our selections.
    fn serve(&mut self, e: SelectionRequestEvent) {
        // Obsolete clients name no property: the target is used instead.
        let property = if e.property == NONE { e.target } else { e.property };
        let ok = self.put(e.selection, e.requestor, property, e.target);
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { NONE },
        };
        let _ = self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
    }

    /// Write the target's bytes to the requestor's property: false if we have none.
    fn put(&mut self, selection: Atom, requestor: Window, property: Atom, target: Atom) -> bool {
        let a = self.atoms;
        let offers = match &self.drag {
            Some(d) if selection == a.XdndSelection => &d.offers,
            _ if selection == a.CLIPBOARD => &self.clip,
            _ => return false,
        };
        let text = offers.iter().any(|o| o.atom == a.UTF8_STRING);
        if target == a.TARGETS && !offers.is_empty() {
            let mut list = vec![a.TARGETS];
            list.extend(offers.iter().map(|o| o.atom));
            if text {
                list.extend([AtomEnum::STRING.into(), a.TEXT]);
            }
            let _ = self.conn.change_property32(PropMode::REPLACE, requestor, property, AtomEnum::ATOM, &list);
            return true;
        }
        // X11's older names for text: STRING is Latin-1, TEXT is given UTF-8.
        let string = text && target == u32::from(AtomEnum::STRING);
        let old = string || (text && target == a.TEXT);
        let wanted = if old { a.UTF8_STRING } else { target };
        let kind = if old && !string { a.UTF8_STRING } else { target };
        let Some(mut bytes) = offers.iter().find(|o| o.atom == wanted).map(|o| o.bytes.clone()) else { return false };
        if string {
            bytes = String::from_utf8_lossy(&bytes).chars().map(|c| u8::try_from(c).unwrap_or(b'?')).collect();
        }
        if bytes.len() <= self.chunk {
            let _ = self.conn.change_property8(PropMode::REPLACE, requestor, property, kind, &bytes);
            return true;
        }
        // Too large for one request: the size first, then a piece each time the requestor
        // deletes the property.
        let _ = self.conn.change_window_attributes(requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE));
        let _ = self.conn.change_property32(PropMode::REPLACE, requestor, property, a.INCR, &[bytes.len() as u32]);
        self.sending.push(Piecewise { requestor, property, kind, bytes, at: 0, since: Instant::now() });
        true
    }

    fn property(&mut self, e: PropertyNotifyEvent) {
        if e.state == Property::DELETE {
            let Some(i) = self.sending.iter().position(|p| p.requestor == e.window && p.property == e.atom) else { return };
            let p = &mut self.sending[i];
            let piece = &p.bytes[p.at..p.bytes.len().min(p.at + self.chunk)];
            let _ = self.conn.change_property8(PropMode::REPLACE, p.requestor, p.property, p.kind, piece);
            p.at += piece.len();
            p.since = Instant::now();
            // The empty piece that ends it was just written.
            if piece.is_empty() {
                self.sending.swap_remove(i);
            }
        } else if e.window == self.win && e.atom == self.atoms.MAIA_SELECTION && self.load.as_ref().is_some_and(|l| l.incr.is_some()) {
            match self.read().map(|r| r.value) {
                Some(v) if v.is_empty() => {
                    let bytes = self.load.as_mut().and_then(|l| l.incr.take()).unwrap_or_default();
                    self.loaded_bytes(Ok(bytes));
                }
                Some(v) => self.load.as_mut().and_then(|l| l.incr.as_mut()).into_iter().for_each(|b| b.extend_from_slice(&v)),
                None => self.loaded_bytes(Err(Error::other("transfer failed"))),
            }
        }
    }

    // ------------------------------------------------ loading

    fn start_load(&mut self, prefer: Vec<String>) {
        let atoms = self.intern(prefer.iter().map(String::as_str));
        let want: Vec<(String, Atom)> = prefer.into_iter().zip(atoms).collect();
        // Our own selection is answered without a round trip.
        if !self.clip.is_empty() {
            let offered: Vec<Atom> = self.clip.iter().map(|o| o.atom).collect();
            let r = self.choose(&want, &offered).and_then(|(mime, atom)| Some((mime, self.clip.iter().find(|o| o.atom == atom)?.bytes.to_vec())));
            let _ = self.reply_tx.send(r.ok_or_else(|| Error::new(ErrorKind::NotFound, "no supported type")));
            return;
        }
        self.load = Some(Load { want, chosen: None, incr: None });
        let _ = self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, self.atoms.TARGETS, self.atoms.MAIA_SELECTION, CURRENT_TIME);
    }

    /// The first wanted type offered; plain text also takes X11's own text types (STRING
    /// is Latin-1, made UTF-8 when it lands).
    fn choose(&self, want: &[(String, Atom)], offered: &[Atom]) -> Option<(String, Atom)> {
        if let Some(w) = want.iter().find(|w| offered.contains(&w.1)) {
            return Some(w.clone());
        }
        want.iter().any(|w| w.0 == "text/plain").then_some(())?;
        let own = [("UTF8_STRING", self.atoms.UTF8_STRING), ("STRING", AtomEnum::STRING.into())];
        own.into_iter().find(|t| offered.contains(&t.1)).map(|(m, a)| (m.to_string(), a))
    }

    fn read(&self) -> Option<GetPropertyReply> {
        self.conn.get_property(true, self.win, self.atoms.MAIA_SELECTION, AtomEnum::ANY, 0, u32::MAX / 4).ok()?.reply().ok()
    }

    fn loaded(&mut self, e: SelectionNotifyEvent) {
        let Some(load) = &self.load else { return };
        // A late answer to an earlier load is ignored.
        if e.selection != self.atoms.CLIPBOARD || e.target != load.chosen.as_ref().map_or(self.atoms.TARGETS, |c| c.1) {
            return;
        }
        let Some(r) = (e.property != NONE).then(|| self.read()).flatten() else { return self.loaded_bytes(Err(Error::other("selection refused"))) };
        if r.type_ == self.atoms.INCR {
            // Reading (so deleting) the property asked for the first piece.
            if let Some(l) = self.load.as_mut() {
                l.incr = Some(Vec::with_capacity(r.value32().and_then(|mut v| v.next()).unwrap_or(0) as usize));
            }
            return;
        }
        self.loaded_bytes(Ok(r.value));
    }

    /// The owner's targets (then the chosen type is asked for), or that type's bytes.
    fn loaded_bytes(&mut self, bytes: Result<Vec<u8>>) {
        let Some(mut load) = self.load.take() else { return };
        let reply = match (bytes, load.chosen.take()) {
            (Ok(b), None) => {
                let offered: Vec<Atom> = b.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect();
                match self.choose(&load.want, &offered) {
                    Some(chosen) => {
                        let _ = self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, chosen.1, self.atoms.MAIA_SELECTION, CURRENT_TIME);
                        self.load = Some(Load { chosen: Some(chosen), ..load });
                        return;
                    }
                    None => Err(Error::new(ErrorKind::NotFound, "no supported type")),
                }
            }
            (Ok(b), Some((mime, _))) if mime == "STRING" => Ok((mime, b.iter().map(|&c| c as char).collect::<String>().into_bytes())),
            (Ok(b), Some((mime, _))) => Ok((mime, b)),
            (Err(e), _) => Err(e),
        };
        let _ = self.reply_tx.send(reply);
    }

    // ------------------------------------------------ drag out

    fn start_drag(&mut self, paths: Vec<PathBuf>, scale: f32) {
        self.end_drag();
        // The window's grab may not be let go of yet: try for a moment.
        let mask = EventMask::POINTER_MOTION | EventMask::BUTTON_RELEASE;
        let grabbed = (0..20).any(|i| {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            let r = self.conn.grab_pointer(false, self.root, mask, GrabMode::ASYNC, GrabMode::ASYNC, NONE, NONE, CURRENT_TIME);
            r.ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.status == GrabStatus::SUCCESS)
        });
        let Some(p) = grabbed.then(|| self.conn.query_pointer(self.root).ok()?.reply().ok()).flatten() else { return };
        let plain = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n").into_bytes();
        let mut items = vec![("text/uri-list".to_string(), crate::clipboard::uri_list(&paths).into_bytes())];
        items.extend(["UTF8_STRING", "text/plain;charset=utf-8", "text/plain"].map(|m| (m.to_string(), plain.clone())));
        let offers = self.offers(items);
        let types: Vec<Atom> = offers.iter().map(|o| o.atom).collect();
        let _ = self.conn.change_property32(PropMode::REPLACE, self.win, self.atoms.XdndTypeList, AtomEnum::ATOM, &types);
        let _ = self.conn.set_selection_owner(self.win, self.atoms.XdndSelection, CURRENT_TIME);
        self.drag = Some(DragOut { offers, paths, scale, at: (p.root_x, p.root_y), state: p.mask, time: CURRENT_TIME, moved: true, target: None, home: None, released: None });
        self.pointer_moved();
        // Let go before the grab took hold: dropped where it is.
        if u16::from(p.mask) & BUTTONS == 0 {
            self.release();
        }
    }

    fn pointer(&mut self, x: i16, y: i16, state: KeyButMask, time: Timestamp) {
        if let Some(d) = self.drag.as_mut().filter(|d| d.released.is_none()) {
            // Buttons aside: a release where the pointer already was moves nothing.
            d.moved |= (x, y) != d.at || (u16::from(state) ^ u16::from(d.state)) & !BUTTONS != 0;
            (d.at, d.state, d.time) = ((x, y), state, time);
        }
    }

    /// Follow the pointer (once for a burst of motion): over Maia's window the drag is
    /// Maia's again; elsewhere the aware window under it is entered, and sent each new
    /// position once it has answered the last.
    fn pointer_moved(&mut self) {
        let a = self.atoms;
        let Some(d) = self.drag.as_ref().filter(|d| d.moved && d.released.is_none()) else { return };
        let under = self.aware_at(d.at.0, d.at.1);
        let home = under.filter(|u| u.0 == self.maia);
        let target = under.filter(|_| home.is_none());
        let Some(d) = self.drag.as_mut() else { return };
        if d.target.map(|t| t.win) != target.map(|t| t.0) {
            if let Some(t) = d.target.take() {
                xdnd(&self.conn, t.win, a.XdndLeave, [self.win, 0, 0, 0, 0]);
            }
            if let Some((win, version, ..)) = target {
                // More than three types are read from XdndTypeList.
                let ty = |i: usize| d.offers.get(i).map_or(NONE, |o| o.atom);
                xdnd(&self.conn, win, a.XdndEnter, [self.win, version << 24 | (d.offers.len() > 3) as u32, ty(0), ty(1), ty(2)]);
                d.target = Some(Target { win, waiting: false, accepted: false, dropped: false });
            }
        }
        if let Some((_, _, x, y)) = home {
            let (x, y) = (x as f32 / d.scale, y as f32 / d.scale);
            d.home = Some((x, y));
            d.moved = false;
            return emit(Dnd::Over { x, y, paths: d.paths.clone(), own: true });
        }
        if d.home.take().is_some() {
            emit(Dnd::Left);
        }
        let Some(t) = d.target.as_mut().filter(|t| !t.waiting) else {
            // Sent once the last status is back.
            d.moved = d.target.is_some();
            return;
        };
        t.waiting = true;
        d.moved = false;
        // As for drops into Maia from other apps: copied, moved with Shift, linked with Ctrl+Shift.
        let action = match (d.state.contains(KeyButMask::SHIFT), d.state.contains(KeyButMask::CONTROL)) {
            (true, true) => a.XdndActionLink,
            (true, false) => a.XdndActionMove,
            _ => a.XdndActionCopy,
        };
        let (x, y) = d.at;
        xdnd(&self.conn, t.win, a.XdndPosition, [self.win, 0, (x as u16 as u32) << 16 | y as u16 as u32, d.time, action]);
    }

    /// The XDND-aware window under a root position, its version, and the position in it:
    /// down from the root through the windows stacked there.
    fn aware_at(&self, x: i16, y: i16) -> Option<(Window, u32, i16, i16)> {
        let mut w = self.root;
        loop {
            let t = self.conn.translate_coordinates(self.root, w, x, y).ok()?.reply().ok()?;
            if w != self.root {
                let r = self.conn.get_property(false, w, self.atoms.XdndAware, AtomEnum::ATOM, 0, 1).ok()?.reply().ok()?;
                if let Some(v) = r.value32().and_then(|mut v| v.next()) {
                    return (v >= 3).then_some((w, v.min(XDND), t.dst_x, t.dst_y));
                }
            }
            if t.child == NONE {
                return None;
            }
            w = t.child;
        }
    }

    /// The button was let go: back in Maia it is dropped there, elsewhere on the target.
    fn release(&mut self) {
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let Some(d) = self.drag.as_mut() else { return };
        if let Some((x, y)) = d.home {
            emit(Dnd::Dropped { x, y, paths: std::mem::take(&mut d.paths), own: true });
            self.drag = None;
            return;
        }
        d.released = Some(Instant::now() + STALE);
        self.try_drop();
    }

    /// Once let go: a target that accepted the last position gets the drop (when it has
    /// answered it), any other is left.
    fn try_drop(&mut self) {
        let Some(d) = self.drag.as_mut().filter(|d| d.released.is_some()) else { return };
        let time = d.time;
        match d.target.as_mut() {
            Some(t) if t.waiting || t.dropped => {}
            Some(t) if t.accepted => {
                t.dropped = true;
                xdnd(&self.conn, t.win, self.atoms.XdndDrop, [self.win, 0, time, 0, 0]);
            }
            _ => self.end_drag(),
        }
    }

    fn message(&mut self, e: ClientMessageEvent) {
        let a = self.atoms;
        let l = e.data.as_data32();
        let Some(t) = self.drag.as_mut().and_then(|d| d.target.as_mut()).filter(|t| t.win == l[0]) else { return };
        if e.type_ == a.XdndStatus {
            t.waiting = false;
            t.accepted = l[1] & 1 != 0;
            self.try_drop();
            self.pointer_moved();
        } else if e.type_ == a.XdndFinished && t.dropped {
            self.drag = None;
        }
    }

    /// Done with the drag: a target told of it but not dropped on is left.
    fn end_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        if let Some(t) = d.target.filter(|t| !t.dropped) {
            xdnd(&self.conn, t.win, self.atoms.XdndLeave, [self.win, 0, 0, 0, 0]);
        }
        if d.home.is_some() {
            emit(Dnd::Left);
        }
        if d.released.is_none() {
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        }
    }
}
