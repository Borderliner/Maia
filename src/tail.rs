//! Following a file as it grows (a log being written): a worker waits on inotify for the
//! file's folder, so nothing wakes while the file is quiet, reads what was appended and
//! hands it over. A file that is truncated or replaced (rotated) is read again from its
//! last megabyte. Where inotify does not work (some network mounts) it looks once a
//! second instead. Dropping the `Tail` ends the worker.

use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Read again from at most this far before the end, after a reset.
pub const TAIL: u64 = 1 << 20;
/// What waits to be taken: past this the oldest goes (the view shows the newest).
const PENDING: usize = 8 << 20;

#[derive(Default)]
struct Got {
    data: Vec<u8>,
    /// What was shown before is no longer the file's start: start over with `data`.
    reset: bool,
}

pub struct Tail {
    got: Arc<Mutex<Got>>,
    /// An eventfd: written when the tail is dropped, which ends the worker's wait.
    stop: Arc<OwnedFd>,
}

impl Drop for Tail {
    fn drop(&mut self) {
        let one = 1u64;
        unsafe { libc::write(self.stop.as_raw_fd(), &one as *const u64 as *const libc::c_void, 8) };
    }
}

impl Tail {
    /// Follow `path` from byte `from` (None: from its last megabyte, as a reset).
    pub fn start(path: PathBuf, from: Option<u64>, ctx: egui::Context) -> Option<Tail> {
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if efd < 0 {
            return None;
        }
        let stop = Arc::new(unsafe { OwnedFd::from_raw_fd(efd) });
        let got = Arc::new(Mutex::new(Got::default()));
        let (g2, s2) = (got.clone(), stop.clone());
        std::thread::spawn(move || follow(path, from, g2, s2, ctx));
        Some(Tail { got, stop })
    }

    /// What arrived since last asked: (start over, bytes).
    pub fn take(&self) -> Option<(bool, Vec<u8>)> {
        let mut g = self.got.lock().unwrap_or_else(|e| e.into_inner());
        if g.data.is_empty() && !g.reset {
            return None;
        }
        let reset = std::mem::take(&mut g.reset);
        Some((reset, std::mem::take(&mut g.data)))
    }
}

fn follow(path: PathBuf, from: Option<u64>, got: Arc<Mutex<Got>>, stop: Arc<OwnedFd>, ctx: egui::Context) {
    let name = path.file_name().map(|n| n.as_encoded_bytes().to_vec()).unwrap_or_default();
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let ifd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    let ifd = (ifd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(ifd) });
    let watching = ifd.as_ref().is_some_and(|fd| {
        let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else { return false };
        let mask = libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_ATTRIB;
        unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), mask) >= 0 }
    });
    let (mut pos, mut ino) = (from.unwrap_or(u64::MAX), 0u64);
    if let Ok(m) = std::fs::metadata(&path) {
        ino = m.ino();
    }
    let mut buf = vec![0u8; 16 << 10];
    let mut first = true;
    loop {
        if !first {
            let mut fds = vec![libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            if let (true, Some(fd)) = (watching, &ifd) {
                fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            }
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, if watching { -1 } else { 1000 }) };
            if n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return;
            }
            if fds[0].revents != 0 {
                return;
            }
            if watching {
                // Only this file's events count; others in the folder are drained.
                let mut ours = false;
                if let Some(fd) = &ifd {
                    loop {
                        let r = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                        if r <= 0 {
                            break;
                        }
                        let hdr = std::mem::size_of::<libc::inotify_event>();
                        let mut off = 0usize;
                        while off + hdr <= r as usize {
                            let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const libc::inotify_event) };
                            let end = (off + hdr + ev.len as usize).min(r as usize);
                            let n = buf[off + hdr..end].split(|b| *b == 0).next().unwrap_or(&[]);
                            ours |= n == name.as_slice();
                            off = end;
                        }
                    }
                }
                if !ours {
                    continue;
                }
            }
        }
        first = false;
        let Ok(m) = std::fs::metadata(&path) else { continue };
        let len = m.len();
        let reset = pos == u64::MAX || m.ino() != ino || len < pos;
        if reset {
            ino = m.ino();
            pos = len.saturating_sub(TAIL);
        }
        if len <= pos && !reset {
            continue;
        }
        let Ok(mut f) = std::fs::File::open(&path) else { continue };
        if f.seek(SeekFrom::Start(pos)).is_err() {
            continue;
        }
        let mut data = vec![];
        let want = (len - pos).min(PENDING as u64);
        if f.take(want).read_to_end(&mut data).is_err() {
            continue;
        }
        // Read from the middle of the file: the first, partial line goes.
        if reset && pos > 0 {
            let cut = data.iter().position(|&b| b == b'\n').map_or(data.len(), |p| p + 1);
            data.drain(..cut);
        }
        pos += want;
        {
            let mut g = got.lock().unwrap_or_else(|e| e.into_inner());
            if reset {
                g.data.clear();
                g.reset = true;
            }
            g.data.extend_from_slice(&data);
            if g.data.len() > PENDING {
                let over = g.data.len() - PENDING;
                let cut = g.data[over..].iter().position(|&b| b == b'\n').map_or(over, |p| over + p + 1);
                g.data.drain(..cut);
                g.reset = true;
            }
        }
        ctx.request_repaint();
        // A busy log is taken in a few times a second, not on every write.
        std::thread::sleep(Duration::from_millis(150));
    }
}
