//! Wayland clipboard on the core wl_data_device protocol (works on every compositor),
//! offering several MIME types at once (text plus file lists), and drag and drop of
//! files in both directions (winit has none on Wayland).
//!
//! Structure adapted from smithay-clipboard (MIT, see assets/SMITHAY-CLIPBOARD-LICENSE.txt),
//! extended for multi-MIME stores and loads, and built on winit's own sctk/calloop versions.

use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use sctk::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler, DragSource};
use sctk::data_device_manager::{DataDeviceManagerState, WritePipe};
use sctk::reexports::calloop::channel::{self, Channel};
use sctk::reexports::calloop::{EventLoop, LoopHandle, PostAction};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::backend::{Backend, ObjectId};
use sctk::reexports::client::globals::{GlobalList, registry_queue_init};
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_keyboard::WlKeyboard;
use sctk::reexports::client::protocol::wl_pointer::WlPointer;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_buffer::WlBuffer;
use sctk::reexports::client::protocol::wl_compositor::WlCompositor;
use sctk::reexports::client::protocol::wl_shm::{self, WlShm};
use sctk::reexports::client::protocol::wl_shm_pool::WlShmPool;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::pointer::{PointerData, PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::{delegate_data_device, delegate_pointer, delegate_registry, delegate_seat, registry_handlers};

/// (mime type, bytes)
pub type Item = (String, Vec<u8>);

enum Command {
    Store(Vec<Item>),
    /// Load the first available of these MIME types.
    Load(Vec<String>),
    /// Carry these files out of the window (the pointer button is still held).
    Drag(Vec<PathBuf>, Option<Picture>),
    Exit,
}

/// Drag and drop as the app sees it. Positions are surface-local logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Dnd {
    /// Files dragged over the window; `own` when it is a drag Maia started.
    Over { x: f32, y: f32, paths: Vec<PathBuf>, own: bool },
    Left,
    Dropped { x: f32, y: f32, paths: Vec<PathBuf>, own: bool },
}

/// Events for the UI thread, and how to wake it.
static EVENTS: Mutex<Vec<Dnd>> = Mutex::new(Vec::new());
static WAKER: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

pub fn set_waker(f: impl Fn() + Send + 'static) {
    *WAKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
}

pub fn take_events() -> Vec<Dnd> {
    std::mem::take(&mut *EVENTS.lock().unwrap_or_else(|e| e.into_inner()))
}

fn emit(ev: Dnd) {
    EVENTS.lock().unwrap_or_else(|e| e.into_inner()).push(ev);
    if let Some(w) = WAKER.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        w();
    }
}

pub struct WlClip {
    tx: channel::Sender<Command>,
    rx: Receiver<Result<Item>>,
}

impl WlClip {
    /// # Safety
    /// `display` must be a valid `wl_display` pointer that outlives this clipboard.
    pub unsafe fn new(display: *mut std::ffi::c_void) -> WlClip {
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let (tx, chan) = channel::channel();
        let (reply_tx, rx) = mpsc::channel();
        std::thread::spawn(move || worker(conn, chan, reply_tx));
        WlClip { tx, rx }
    }

    pub fn store(&self, items: Vec<Item>) {
        let _ = self.tx.send(Command::Store(items));
    }

    pub fn start_drag(&self, paths: Vec<PathBuf>, picture: Option<Picture>) {
        let _ = self.tx.send(Command::Drag(paths, picture));
    }

    pub fn load(&self, prefer: &[&str]) -> Result<Item> {
        while self.rx.try_recv().is_ok() {}
        let _ = self.tx.send(Command::Load(prefer.iter().map(|s| s.to_string()).collect()));
        self.rx.recv_timeout(Duration::from_millis(800)).map_err(|_| Error::other("clipboard timed out"))?
    }
}

impl Drop for WlClip {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Exit);
    }
}

fn worker(conn: Connection, chan: Channel<Command>, reply_tx: Sender<Result<Item>>) {
    let Ok((globals, queue)) = registry_queue_init(&conn) else { return };
    let Ok(mut event_loop) = EventLoop::<State>::try_new() else { return };
    let handle = event_loop.handle();
    let Some(mut state) = State::new(&globals, &queue.handle(), handle.clone(), reply_tx) else { return };
    let _ = handle.insert_source(chan, |ev, _, st| {
        if let channel::Event::Msg(cmd) = ev {
            match cmd {
                Command::Store(items) => st.store(items),
                Command::Load(prefer) => {
                    if let Err(e) = st.load(prefer) {
                        let _ = st.reply_tx.send(Err(e));
                    }
                }
                Command::Drag(paths, picture) => st.start_drag(paths, picture),
                Command::Exit => st.exit = true,
            }
        }
    });
    if WaylandSource::new(conn, queue).insert(handle).is_err() {
        return;
    }
    while !state.exit {
        if event_loop.dispatch(None, &mut state).is_err() {
            break;
        }
    }
}

#[derive(Default)]
struct SeatData {
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    device: Option<DataDevice>,
    has_focus: bool,
    serial: u32,
    /// The pointer press that is still held, and the surface it went to (a drag out
    /// of the window starts from it).
    press: Option<(u32, WlSurface)>,
}

/// The drag Maia carried out of its window.
struct DragOut {
    source: DragSource,
    paths: Vec<PathBuf>,
    /// The picture under the pointer while dragging.
    icon: Option<(WlSurface, WlBuffer)>,
}

impl Drop for DragOut {
    fn drop(&mut self) {
        if let Some((surface, buffer)) = self.icon.take() {
            surface.destroy();
            buffer.destroy();
        }
    }
}

/// A drag from anywhere (possibly Maia itself) over the window.
struct DragIn {
    dev: WlDataDevice,
    paths: Option<Vec<PathBuf>>,
    x: f64,
    y: f64,
    dropped: bool,
    own: bool,
}

struct State {
    ddm: Option<DataDeviceManagerState>,
    reply_tx: Sender<Result<Item>>,
    exit: bool,
    registry: RegistryState,
    seat_state: SeatState,
    seats: HashMap<ObjectId, SeatData>,
    latest_seat: Option<ObjectId>,
    loop_handle: LoopHandle<'static, State>,
    qh: QueueHandle<State>,
    sources: Vec<CopyPasteSource>,
    content: Rc<HashMap<String, Rc<[u8]>>>,
    drag_out: Option<DragOut>,
    drag_in: Option<DragIn>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
}

impl State {
    fn new(globals: &GlobalList, qh: &QueueHandle<Self>, loop_handle: LoopHandle<'static, Self>, reply_tx: Sender<Result<Item>>) -> Option<State> {
        let ddm = DataDeviceManagerState::bind(globals, qh).ok();
        ddm.as_ref()?;
        let seat_state = SeatState::new(globals, qh);
        let seats = seat_state.seats().map(|s| (s.id(), SeatData::default())).collect();
        Some(State {
            ddm,
            reply_tx,
            exit: false,
            registry: RegistryState::new(globals),
            seat_state,
            seats,
            latest_seat: None,
            loop_handle,
            qh: qh.clone(),
            sources: vec![],
            content: Rc::new(HashMap::new()),
            drag_out: None,
            drag_in: None,
            // Version 4 at most: the drag picture is placed with attach offsets.
            compositor: globals.bind::<WlCompositor, _, _>(qh, 1..=4, ()).ok(),
            shm: globals.bind::<WlShm, _, _>(qh, 1..=1, ()).ok(),
        })
    }

    fn active_seat(&mut self) -> Option<&mut SeatData> {
        let id = self.latest_seat.clone().or_else(|| self.seats.iter().find(|(_, s)| s.has_focus).map(|(k, _)| k.clone()))?;
        self.seats.get_mut(&id)
    }

    fn store(&mut self, items: Vec<Item>) {
        let mimes: Vec<String> = items.iter().map(|(m, _)| m.clone()).collect();
        self.content = Rc::new(items.into_iter().map(|(m, b)| (m, Rc::from(b))).collect());
        let qh = self.qh.clone();
        let Some(mgr) = self.ddm.as_ref() else { return };
        let source = mgr.create_copy_paste_source(&qh, mimes.iter().map(|s| s.as_str()));
        let Some(seat) = self.active_seat() else { return };
        if let Some(dev) = seat.device.as_ref() {
            source.set_selection(dev, seat.serial);
        }
        self.sources.push(source);
    }

    fn load(&mut self, prefer: Vec<String>) -> Result<()> {
        let seat = self.active_seat().ok_or_else(|| Error::other("no seat"))?;
        let offer = seat.device.as_ref().and_then(|d| d.data().selection_offer()).ok_or_else(|| Error::other("selection is empty"))?;
        let mime = offer
            .with_mime_types(|offered| prefer.iter().find(|p| offered.iter().any(|o| o == *p)).cloned())
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "no supported type"))?;
        let pipe = offer.receive(mime.clone()).map_err(|_| Error::other("offer not ready"))?;
        set_non_blocking(pipe.as_raw_fd())?;
        let mut buf = [0u8; 4096];
        let mut data = Vec::new();
        let _ = self.loop_handle.insert_source(pipe, move |_, file, st| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.read(&mut buf) {
                    Ok(0) => {
                        let _ = st.reply_tx.send(Ok((mime.clone(), std::mem::take(&mut data))));
                        break PostAction::Remove;
                    }
                    Ok(n) => data.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(e) => {
                        let _ = st.reply_tx.send(Err(e));
                        break PostAction::Remove;
                    }
                }
            }
        });
        Ok(())
    }

    fn start_drag(&mut self, paths: Vec<PathBuf>, picture: Option<Picture>) {
        let qh = self.qh.clone();
        let Some(mgr) = self.ddm.as_ref() else { return };
        // Copy only: whoever takes the files must never make them vanish from here.
        let source = mgr.create_drag_and_drop_source(&qh, DRAG_MIMES, DndAction::Copy);
        // Owns the picture: dropped (and destroyed) if the drag can't start.
        let icon = picture.and_then(|p| self.drag_icon(&p));
        let out = DragOut { source, paths, icon };
        let Some(seat) = self.active_seat() else { return };
        let (Some(dev), Some((serial, surface))) = (seat.device.as_ref(), seat.press.clone()) else { return };
        out.source.start_drag(dev, &surface, out.icon.as_ref().map(|i| &i.0), serial);
        self.drag_out = Some(out);
    }

    /// A surface showing the picture, below-right of the pointer like the badge inside
    /// the window.
    fn drag_icon(&self, pic: &Picture) -> Option<(WlSurface, WlBuffer)> {
        use std::os::fd::{AsFd, FromRawFd};
        let (compositor, shm) = (self.compositor.as_ref()?, self.shm.as_ref()?);
        let (w, h, px) = (pic.w, pic.h, &pic.px);
        let fd = unsafe { libc::memfd_create(c"maia-drag".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(px).ok()?;
        let pool: WlShmPool = shm.create_pool(file.as_fd(), px.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888, &self.qh, ());
        pool.destroy();
        let surface = compositor.create_surface(&self.qh, ());
        let scale = if surface.version() >= 3 { pic.scale.max(1) } else { 1 };
        if scale > 1 {
            surface.set_buffer_scale(scale);
        }
        surface.attach(Some(&buffer), 14, 12);
        surface.damage(0, 0, w as i32, h as i32);
        surface.commit();
        Some((surface, buffer))
    }

    /// Bytes for a MIME type the drag out offers.
    fn drag_bytes(paths: &[PathBuf], mime: &str) -> Vec<u8> {
        if mime == "text/uri-list" {
            crate::clipboard::uri_list(paths).into_bytes()
        } else {
            paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n").into_bytes()
        }
    }

    /// Read the dragged file list; a drop that already happened completes when it lands.
    fn read_drag(&mut self, offer: DragOffer) {
        // The offer is looked up again when the list lands (its agreed action changes).
        let Ok(pipe) = offer.receive("text/uri-list".into()) else { return };
        if set_non_blocking(pipe.as_raw_fd()).is_err() {
            return;
        }
        let mut buf = [0u8; 4096];
        let mut data = Vec::new();
        let _ = self.loop_handle.insert_source(pipe, move |_, file, st| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.read(&mut buf) {
                    Ok(0) => {
                        let paths = crate::clipboard::parse_uri_list(&String::from_utf8_lossy(&data));
                        let fresh = st.drag_in.as_ref().and_then(|d| st.drag_offer(&d.dev));
                        if let Some(d) = st.drag_in.as_mut() {
                            d.paths = Some(paths.clone());
                            let (x, y, own) = (d.x as f32, d.y as f32, d.own);
                            if d.dropped {
                                emit(Dnd::Dropped { x, y, paths, own });
                                if let Some(o) = fresh {
                                    finish(&o);
                                }
                                st.drag_in = None;
                            } else {
                                emit(Dnd::Over { x, y, paths, own });
                            }
                        }
                        break PostAction::Remove;
                    }
                    Ok(n) => data.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(_) => break PostAction::Remove,
                }
            }
        });
    }

    fn drag_offer(&self, dev: &WlDataDevice) -> Option<DragOffer> {
        self.seats.values().filter_map(|s| s.device.as_ref()).find(|d| d.inner() == dev).and_then(|d| d.data().drag_offer())
    }

    fn send(&mut self, pipe: WritePipe, mime: String) {
        let Some(bytes) = self.content.get(&mime).cloned() else { return };
        self.write_out(pipe, bytes);
    }

    fn write_out(&mut self, pipe: WritePipe, bytes: Rc<[u8]>) {
        if set_non_blocking(pipe.as_raw_fd()).is_err() {
            return;
        }
        let mut written = 0;
        let _ = self.loop_handle.insert_source(pipe, move |_, file, _| {
            let file = unsafe { file.get_mut() };
            loop {
                if written >= bytes.len() {
                    break PostAction::Remove;
                }
                match file.write(&bytes[written..]) {
                    Ok(n) => written += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(_) => break PostAction::Remove,
                }
            }
        });
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.insert(seat.id(), SeatData::default());
    }
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, cap: Capability) {
        let Some(sd) = self.seats.get_mut(&seat.id()) else { return };
        match cap {
            Capability::Keyboard => {
                sd.keyboard = Some(seat.get_keyboard(qh, seat.id()));
                if sd.device.is_none() {
                    sd.device = self.ddm.as_ref().map(|m| m.get_data_device(qh, &seat));
                }
            }
            Capability::Pointer => sd.pointer = self.seat_state.get_pointer(qh, &seat).ok(),
            _ => {}
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat, cap: Capability) {
        let Some(sd) = self.seats.get_mut(&seat.id()) else { return };
        match cap {
            Capability::Keyboard => {
                sd.device = None;
                if let Some(k) = sd.keyboard.take() {
                    if k.version() >= 3 {
                        k.release();
                    }
                }
            }
            Capability::Pointer => {
                if let Some(p) = sd.pointer.take() {
                    if p.version() >= 3 {
                        p.release();
                    }
                }
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.remove(&seat.id());
    }
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, pointer: &WlPointer, events: &[PointerEvent]) {
        let Some(seat) = pointer.data::<PointerData>().map(|d| d.seat().id()) else { return };
        for e in events {
            if let PointerEventKind::Press { serial, .. } | PointerEventKind::Release { serial, .. } = e.kind {
                if let Some(sd) = self.seats.get_mut(&seat) {
                    sd.serial = serial;
                    sd.press = matches!(e.kind, PointerEventKind::Press { .. }).then(|| (serial, e.surface.clone()));
                    self.latest_seat = Some(seat.clone());
                }
            }
        }
    }
}

const DRAG_MIMES: [&str; 3] = ["text/uri-list", "text/plain;charset=utf-8", "text/plain"];

/// The picture under the pointer while files are dragged out: premultiplied ARGB8888
/// (little-endian BGRA) at `scale` pixels per logical pixel.
pub struct Picture {
    pub w: usize,
    pub h: usize,
    pub scale: i32,
    pub px: Vec<u8>,
}

/// Tell the source the drop is complete (only valid once an action was agreed).
fn finish(offer: &DragOffer) {
    if !offer.selected_action.is_empty() {
        offer.finish();
    }
}

impl DataDeviceHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, dev: &WlDataDevice, x: f64, y: f64, _: &WlSurface) {
        let Some(offer) = self.drag_offer(dev) else { return };
        // Only file lists are taken; anything else (text, images) is refused.
        if !offer.with_mime_types(|m| m.iter().any(|t| t == "text/uri-list")) {
            offer.accept_mime_type(offer.serial, None);
            self.drag_in = None;
            return;
        }
        offer.accept_mime_type(offer.serial, Some("text/uri-list".into()));
        offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
        // A drag Maia carried out of its window and back is its own.
        let own = self.drag_out.as_ref().map(|d| d.paths.clone());
        self.drag_in = Some(DragIn { dev: dev.clone(), paths: own.clone(), x, y, dropped: false, own: own.is_some() });
        match own {
            Some(paths) => emit(Dnd::Over { x: x as f32, y: y as f32, paths, own: true }),
            None => self.read_drag(offer),
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        if self.drag_in.as_ref().is_some_and(|d| !d.dropped) {
            self.drag_in = None;
            emit(Dnd::Left);
        }
    }
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64) {
        let Some(d) = self.drag_in.as_mut() else { return };
        d.x = x;
        d.y = y;
        if let Some(paths) = d.paths.clone() {
            emit(Dnd::Over { x: x as f32, y: y as f32, paths, own: d.own });
        }
    }
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, dev: &WlDataDevice) {
        let offer = self.drag_offer(dev);
        let Some(d) = self.drag_in.as_mut() else { return };
        d.dropped = true;
        match (d.paths.clone(), offer) {
            (Some(paths), offer) => {
                emit(Dnd::Dropped { x: d.x as f32, y: d.y as f32, paths, own: d.own });
                if let Some(o) = offer {
                    finish(&o);
                }
                self.drag_in = None;
            }
            // Still reading the list: the drop completes when it arrives.
            (None, _) => {}
        }
    }
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataSourceHandler for State {
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource, mime: String, pipe: WritePipe) {
        match self.drag_out.as_ref().filter(|d| d.source.inner() == source) {
            Some(d) => {
                let bytes = Rc::from(Self::drag_bytes(&d.paths, &mime));
                self.write_out(pipe, bytes);
            }
            None => self.send(pipe, mime),
        }
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, deleted: &WlDataSource) {
        self.sources.retain(|s| s.inner() != deleted);
        if self.drag_out.as_ref().is_some_and(|d| d.source.inner() == deleted) {
            self.drag_out = None;
        }
    }
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if self.drag_out.as_ref().is_some_and(|d| d.source.inner() == source) {
            self.drag_out = None;
        }
    }
}

impl DataOfferHandler for State {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl ProvidesRegistryState for State {
    registry_handlers![SeatState];
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
}

impl Dispatch<WlKeyboard, ObjectId, State> for State {
    fn event(state: &mut State, _: &WlKeyboard, event: <WlKeyboard as Proxy>::Event, data: &ObjectId, _: &Connection, _: &QueueHandle<State>) {
        use sctk::reexports::client::protocol::wl_keyboard::Event as E;
        let Some(sd) = state.seats.get_mut(data) else { return };
        match event {
            E::Key { serial, .. } | E::Modifiers { serial, .. } => {
                sd.serial = serial;
                state.latest_seat = Some(data.clone());
            }
            E::Enter { serial, .. } => {
                sd.serial = serial;
                sd.has_focus = true;
            }
            E::Leave { .. } => {
                sd.has_focus = false;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_seat!(State);
delegate_pointer!(State);
delegate_data_device!(State);
delegate_registry!(State);

fn set_non_blocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
