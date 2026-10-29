//! Paste when the clipboard holds no files: a picture copied in a browser or a
//! screenshot tool becomes a picture file, copied text a .txt, named after the time and
//! ready to be renamed. The clipboard is read and the file written on a thread: a
//! picture can be megabytes, and the app it comes from can be slow to hand it over.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;

/// Picture types asked for, best first. Taken as offered, so nothing is converted and a
/// photo keeps its quality.
const PICTURES: [(&str, &str); 6] = [("image/png", "png"), ("image/jpeg", "jpg"), ("image/webp", "webp"), ("image/avif", "avif"), ("image/gif", "gif"), ("image/bmp", "bmp")];
const TEXT: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

pub enum Pasted {
    File(PathBuf),
    /// Neither a picture nor text: the app's own copied files are pasted instead.
    Nothing,
    Failed(String),
}

/// A paste being written, for the tab it was asked in.
pub struct Pending {
    pub panel: u64,
    pub tab: u64,
    /// Where it goes: the tab's folder, or the one folder selected in it.
    pub dest: PathBuf,
    pub rx: mpsc::Receiver<Pasted>,
}

/// Read the clipboard and write what it holds into `dir`.
pub fn start(panel: u64, tab: u64, dir: PathBuf, ctx: &egui::Context) -> Pending {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let d = dir.clone();
    let _ = std::thread::Builder::new().name("paste".into()).spawn(move || {
        let _ = tx.send(write(&d));
        ctx.request_repaint();
    });
    Pending { panel, tab, dest: dir, rx }
}

fn write(dir: &std::path::Path) -> Pasted {
    let mimes: Vec<&str> = PICTURES.iter().map(|p| p.0).collect();
    let (bytes, ext) = match crate::clipboard::get_bytes(&mimes) {
        Some((mime, b)) if !b.is_empty() => (b, PICTURES.iter().find(|p| p.0 == mime).map_or("png", |p| p.1)),
        _ => match crate::clipboard::get_bytes(&TEXT) {
            Some((_, b)) if !String::from_utf8_lossy(&b).trim().is_empty() => (b, "txt"),
            _ => return Pasted::Nothing,
        },
    };
    let stamp = crate::timefmt::format(std::time::SystemTime::now(), "%Y-%m-%d %H.%M");
    let dest = crate::fsx::unique_dest(dir, &format!("Pasted {stamp}.{ext}"));
    let r = std::fs::File::create_new(&dest).and_then(|mut f| f.write_all(&bytes));
    match r {
        Ok(()) => Pasted::File(dest),
        Err(e) => {
            // Half a picture is worse than none.
            let _ = std::fs::remove_file(&dest);
            Pasted::Failed(format!("Couldn't paste here: {}", crate::fsx::plain_error(&e.to_string())))
        }
    }
}
