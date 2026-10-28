//! Email and Print: files handed to the mail app as attachments, or to CUPS.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use egui::{CornerRadius, FontId, Key, Modifiers, Sense, pos2, vec2};

use crate::app::App;
use crate::drives::Tools;

// ------------------------------------------------------------------ email

#[cfg(test)]
thread_local! {
    /// The mail app's Exec line a test pretends is the desktop's (tests never read the
    /// session's own).
    pub static MAIL_APP: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The Exec line of the app that opens mailto: links.
fn mail_app() -> Option<String> {
    #[cfg(test)]
    return MAIL_APP.with(|m| m.borrow().clone());
    #[cfg(not(test))]
    crate::mime::apps().default_for("x-scheme-handler/mailto").map(|a| a.exec.clone())
}

/// Mail apps that take attachments on their command line, and how.
const CLIENTS: [&str; 5] = ["thunderbird", "betterbird", "evolution", "kmail", "claws-mail"];

/// The command that opens a new email with `files` attached: xdg-email when there is
/// one, else the mail app itself when it is one that takes attachments.
pub fn email_args(files: &[PathBuf], xdg_email: bool, app: Option<&str>) -> Option<Vec<OsString>> {
    if xdg_email {
        let mut v: Vec<OsString> = vec!["xdg-email".into()];
        for f in files {
            v.push("--attach".into());
            v.push(f.into());
        }
        return Some(v);
    }
    // The app's own words (flatpak run and the like), without its field codes.
    let words: Vec<String> = app?.split_whitespace().filter(|w| !w.starts_with('%')).map(String::from).collect();
    let prog = Path::new(words.first()?).file_name()?.to_string_lossy().to_lowercase();
    let flat = words.join(" ").to_lowercase();
    let which = CLIENTS.iter().find(|c| prog == **c || flat.contains(*c))?;
    let uris: Vec<String> = files.iter().map(|f| crate::clipboard::path_to_uri(f)).collect();
    let mut v: Vec<OsString> = words.iter().map(OsString::from).collect();
    match *which {
        "thunderbird" | "betterbird" => {
            v.push("-compose".into());
            v.push(format!("attachment='{}'", uris.join(",")).into());
        }
        "evolution" => v.push(format!("mailto:?{}", uris.iter().map(|u| format!("attach={u}")).collect::<Vec<_>>().join("&")).into()),
        "kmail" => uris.iter().for_each(|u| {
            v.push("--attach".into());
            v.push(u.into());
        }),
        _ => {
            v.push("--attach".into());
            v.extend(files.iter().map(OsString::from));
        }
    }
    Some(v)
}

/// Open a new email with `files` attached.
pub fn email(files: &[PathBuf]) -> Result<(), String> {
    let tools = Tools::current().ok_or("No mail app is set up")?;
    let app = mail_app();
    if app.is_none() && !CLIENTS.iter().any(|c| tools.has(c)) {
        return Err("No mail app is set up".into());
    }
    // No handler named, but a client installed: xdg-email finds it.
    let args = email_args(files, tools.has("xdg-email"), app.as_deref()).ok_or("Attaching files to an email needs xdg-email")?;
    let (prog, rest) = args.split_first().ok_or("No mail app is set up")?;
    let mut c = if prog == "xdg-email" { tools.cmd("xdg-email") } else { std::process::Command::new(prog) };
    c.args(rest);
    crate::mime::spawn_detached(c)
}

// ------------------------------------------------------------------ print

/// CUPS is there to print with.
pub fn cups(t: &Tools) -> bool {
    t.has("lp") && t.has("lpstat")
}

/// Files CUPS prints as they are: PDF, PostScript, pictures and plain text.
pub fn printable(name: &str, ext: &str) -> bool {
    matches!(ext, "pdf" | "ps" | "eps" | "jpg" | "jpeg" | "png" | "gif" | "tif" | "tiff" | "bmp" | "pnm" | "ppm") || crate::mime::mime_db().for_name(name, false, false).starts_with("text/")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Printers {
    pub names: Vec<String>,
    pub default: Option<String>,
}

/// `lpstat -e` (a destination per line) and `lpstat -d` ("system default destination:
/// NAME", or "no system default destination").
pub fn parse_printers(e: &str, d: &str) -> Printers {
    let names: Vec<String> = e.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains(' ')).map(String::from).collect();
    let default = d.lines().find_map(|l| l.trim().strip_prefix("system default destination:")).map(|n| n.trim().to_string()).filter(|n| names.contains(n));
    Printers { names, default }
}

pub fn printers(t: &Tools) -> Printers {
    let run = |arg: &str| {
        let mut c = t.cmd("lpstat");
        c.arg(arg).env("LC_ALL", "C");
        String::from_utf8_lossy(&crate::errands::output(c, Duration::from_secs(10)).unwrap_or_default()).into_owned()
    };
    parse_printers(&run("-e"), &run("-d"))
}

pub fn lp_args(printer: &str, copies: u32, files: &[PathBuf]) -> Vec<OsString> {
    let mut v: Vec<OsString> = vec!["-d".into(), printer.into(), "-n".into(), copies.to_string().into()];
    v.extend(files.iter().map(OsString::from));
    v
}

pub struct PrintState {
    pub files: Vec<PathBuf>,
    pub printers: Option<Printers>,
    rx: Option<Receiver<Printers>>,
    pub sel: usize,
    pub copies: u32,
    tools: Tools,
}

impl PrintState {
    /// The dialog, its printers read on a worker as it opens.
    pub fn open(tools: Tools, files: Vec<PathBuf>, ctx: &egui::Context) -> PrintState {
        let (tx, rx) = std::sync::mpsc::channel();
        let (t, ctx) = (tools.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(printers(&t));
            ctx.request_repaint();
        });
        PrintState { files, printers: None, rx: Some(rx), sel: 0, copies: 1, tools }
    }
}

/// The Print dialog: printers to pick from, the system's default first chosen, and how
/// many copies. False when it closes.
pub fn dialog(app: &mut App, ctx: &egui::Context, st: &mut PrintState) -> bool {
    use crate::popups::{card, centered, dialog_buttons, dialog_title, plain_button, primary_button};
    let pal = app.pal;
    if let Some(p) = st.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
        st.sel = p.default.as_ref().and_then(|d| p.names.iter().position(|n| n == d)).unwrap_or(0);
        st.printers = Some(p);
        st.rx = None;
    }
    let names = st.printers.as_ref().map(|p| p.names.clone()).unwrap_or_default();
    let (up, down, mut go, esc) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::Enter), i.consume_key(Modifiers::NONE, Key::Escape)));
    if esc {
        return false;
    }
    // Left and right count the copies, as the buttons beside them do.
    let (less, more) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowLeft) || i.consume_key(Modifiers::NONE, Key::Minus), i.consume_key(Modifiers::NONE, Key::ArrowRight) || i.consume_key(Modifiers::NONE, Key::Plus)));
    st.copies = if less { st.copies.saturating_sub(1) } else if more { st.copies + 1 } else { st.copies }.clamp(1, 99);
    if !names.is_empty() {
        st.sel = if down { (st.sel + 1).min(names.len() - 1) } else if up { st.sel.saturating_sub(1) } else { st.sel.min(names.len() - 1) };
    }
    go &= !names.is_empty();
    let mut cancel = false;
    let title = match st.files.as_slice() {
        [f] => format!("Print “{}”", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        fs => format!("Print {} files", fs.len()),
    };
    let fs = app.cfg.font_size;
    centered(ctx, "print", 400.0, &pal, |ui| {
        dialog_title(ui, &title, None, &pal);
        ui.add_space(8.0);
        card(ui, &pal, |ui| {
            let font = FontId::proportional(fs - 1.0);
            let row_h = (fs + 11.0).max(24.0);
            let quiet = match &st.printers {
                None => Some("Looking for printers…"),
                Some(p) if p.names.is_empty() => Some("No printers are set up"),
                _ => None,
            };
            if let Some(q) = quiet {
                ui.label(egui::RichText::new(q).color(pal.text_dim));
                return;
            }
            let default = st.printers.as_ref().and_then(|p| p.default.clone());
            for (i, n) in names.iter().enumerate() {
                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                crate::ui::note_control(ui, r, "printer");
                let on = i == st.sel;
                if on {
                    ui.painter().rect_filled(r, CornerRadius::same(5), pal.selected);
                } else if resp.hovered() {
                    ui.painter().rect_filled(r, CornerRadius::same(5), pal.hover);
                }
                let mut room = r.width() - 16.0;
                if default.as_deref() == Some(n.as_str()) {
                    let g = ui.painter().layout_no_wrap("Default".into(), FontId::proportional(fs - 2.0), if on { pal.on_sel_dim() } else { pal.text_dim });
                    room -= g.size().x + 12.0;
                    ui.painter().galley(pos2(r.right() - 8.0 - g.size().x, r.center().y - g.size().y / 2.0), g, pal.text_dim);
                }
                let g = crate::listing::elided(ui, n, font.clone(), room.max(20.0), if on { pal.on_sel() } else { pal.text });
                ui.painter().galley(pos2(r.left() + 8.0, r.center().y - g.size().y / 2.0), g, pal.text);
                if resp.clicked() {
                    st.sel = i;
                }
                if resp.double_clicked() {
                    go = true;
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Copies").color(pal.text_dim));
            ui.add_space(6.0);
            let step = |ui: &mut egui::Ui, t: &str, what: &'static str| {
                let r = ui.add(egui::Button::new(t).fill(pal.card()).stroke(egui::Stroke::new(1.0, pal.card_line())).min_size(vec2(28.0, 0.0)));
                crate::ui::note_control(ui, r.rect, what);
                r.clicked()
            };
            if step(ui, "−", "fewer copies") {
                st.copies = st.copies.saturating_sub(1).max(1);
            }
            let g = ui.painter().layout_no_wrap(st.copies.to_string(), egui::TextStyle::Body.resolve(ui.style()), pal.text);
            let (r, _) = ui.allocate_exact_size(vec2(g.size().x.max(fs * 1.6), g.size().y), Sense::hover());
            ui.painter().galley(pos2(r.center().x - g.size().x / 2.0, r.top()), g, pal.text);
            if step(ui, "+", "more copies") {
                st.copies = (st.copies + 1).min(99);
            }
        });
        dialog_buttons(ui, |ui| {
            if primary_button(ui, "Print", !names.is_empty(), &pal).clicked() {
                go = true;
            }
            if plain_button(ui, "Cancel", &pal).clicked() {
                cancel = true;
            }
        });
    });
    if go {
        let (tools, files, copies) = (st.tools.clone(), st.files.clone(), st.copies.clamp(1, 99));
        let printer = names[st.sel.min(names.len() - 1)].clone();
        app.errands.run(ctx, move || {
            let mut c = tools.cmd("lp");
            c.args(lp_args(&printer, copies, &files)).env("LC_ALL", "C");
            match crate::errands::output(c, Duration::from_secs(60)) {
                Ok(_) if copies > 1 => crate::errands::Done::say(format!("Sent {copies} copies to {printer}")),
                Ok(_) => crate::errands::Done::say(format!("Sent to {printer}")),
                Err(e) => crate::errands::Done::say(format!("Couldn't print: {e}")),
            }
        });
        return false;
    }
    !cancel
}
